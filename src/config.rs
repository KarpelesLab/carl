//! Runtime configuration for Carl.
//!
//! Configuration is intentionally minimal at this stage. Values are resolved
//! from environment variables with sensible defaults so Carl can be launched by
//! an MCP client (over stdio) with zero required setup. As features land, each
//! feature module is expected to own its own typed config section here.

use std::{
    fs, io,
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::{Path, PathBuf},
    time::Duration,
};

/// How long the daemon lingers with no shim connected before exiting.
const DEFAULT_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// How often the daemon checks that its lock and socket are still its own.
const DEFAULT_HEALTH_INTERVAL: Duration = Duration::from_secs(30);

/// Top-level configuration shared across all features.
#[derive(Debug, Clone)]
pub struct Config {
    /// Directory where Carl persists state (wallet keystore, etc.), plus the
    /// daemon's lock.
    ///
    /// Defaults to `$CARL_DATA_DIR`, then `$XDG_DATA_HOME/carl`, then
    /// `~/.local/share/carl`. Never a cache directory: it will hold key
    /// material, so losing it can mean losing funds. The lock lives here, next
    /// to what it protects, so two daemons can never share a keystore whatever
    /// environment their shims were launched from.
    pub data_dir: PathBuf,
    /// Where the daemon writes its log: `$CARL_DATA_DIR` if set, else
    /// `$XDG_STATE_HOME/carl`, else `~/.local/state/carl`.
    pub log_dir: PathBuf,
    /// Unix socket the daemon listens on: `$CARL_SOCKET`, else
    /// `/tmp/carl-<uid>/<hash of data_dir>.sock`.
    ///
    /// Deliberately not under `$XDG_RUNTIME_DIR` or `$TMPDIR`: every agent must
    /// compute the same path whatever its environment, and `/tmp` also keeps it
    /// short (socket paths max out at 108 bytes). One socket per data dir, so
    /// separate instances (tests, a second keystore) don't collide.
    pub socket: PathBuf,
    /// How long the daemon stays up after its last shim disconnects
    /// (`$CARL_IDLE_TIMEOUT`, in seconds).
    pub idle_timeout: Duration,
    /// Tool areas new sessions start with (`$CARL_AREAS`: comma-separated
    /// ids or `all`). In the daemon, each session uses its shim's value.
    pub areas: Option<String>,
    /// How often the daemon verifies it still owns its lock and socket
    /// (`$CARL_HEALTH_INTERVAL`, in seconds; mostly for tests).
    pub health_interval: Duration,
}

impl Config {
    /// Build a [`Config`] from the process environment.
    pub fn from_env() -> Self {
        let (data_dir, log_dir) = match std::env::var_os("CARL_DATA_DIR") {
            // An explicit data dir holds everything, so tests and side-by-side
            // instances stay self-contained.
            Some(dir) => (PathBuf::from(&dir), PathBuf::from(dir)),
            None => (
                xdg_dir("XDG_DATA_HOME", ".local/share"),
                xdg_dir("XDG_STATE_HOME", ".local/state"),
            ),
        };

        let socket = std::env::var_os("CARL_SOCKET")
            .map(PathBuf::from)
            .unwrap_or_else(|| default_socket(&data_dir));

        Self {
            data_dir,
            log_dir,
            socket,
            areas: std::env::var("CARL_AREAS").ok(),
            idle_timeout: env_secs("CARL_IDLE_TIMEOUT").unwrap_or(DEFAULT_IDLE_TIMEOUT),
            health_interval: env_secs("CARL_HEALTH_INTERVAL").unwrap_or(DEFAULT_HEALTH_INTERVAL),
        }
    }

    /// Lock held for the daemon's whole lifetime; contains its pid.
    pub fn lock_path(&self) -> PathBuf {
        self.data_dir.join("daemon.lock")
    }

    /// Where the daemon's stderr (its tracing output) goes.
    pub fn log_path(&self) -> PathBuf {
        self.log_dir.join("daemon.log")
    }

    /// Create the data directory if needed; see [`ensure_private_dir`].
    pub fn ensure_data_dir(&self) -> io::Result<()> {
        ensure_private_dir(&self.data_dir)
    }

    /// Create the log directory if needed; see [`ensure_private_dir`].
    pub fn ensure_log_dir(&self) -> io::Result<()> {
        ensure_private_dir(&self.log_dir)
    }

    /// Create the socket's directory if needed. It usually sits in the shared
    /// `/tmp`, where another user may have created it first, so beyond
    /// [`ensure_private_dir`] it must be a real directory (not a symlink) that
    /// no one else can write to. Otherwise someone could swap our socket for
    /// theirs.
    pub fn ensure_socket_dir(&self) -> io::Result<()> {
        let dir = self.socket.parent().unwrap_or(Path::new("."));
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        let meta = fs::symlink_metadata(dir)?;
        if !meta.is_dir()
            || meta.uid() != rustix::process::getuid().as_raw()
            || meta.mode() & 0o022 != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "refusing to use {}: it must be a directory owned by us and writable only by us",
                    dir.display()
                ),
            ));
        }
        Ok(())
    }
}

/// Create `dir` (mode 0700) if needed, and refuse one owned by another user:
/// the daemon socket lives in the data dir, so whoever owns it controls who
/// Carl talks to, and the log records which agents connected from where.
fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let owner = fs::metadata(dir)?.uid();
    if owner != rustix::process::getuid().as_raw() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is owned by uid {owner}, not by us", dir.display()),
        ));
    }
    Ok(())
}

impl Default for Config {
    fn default() -> Self {
        Self::from_env()
    }
}

/// `/tmp/carl-<uid>/<16 hex digits>.sock`, the digits being a stable (FNV-1a)
/// hash of the absolute data dir path.
fn default_socket(data_dir: &Path) -> PathBuf {
    let data_dir = std::path::absolute(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
    let hash = data_dir
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .fold(0xcbf29ce484222325u64, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x100000001b3)
        });
    let uid = rustix::process::getuid().as_raw();
    PathBuf::from(format!("/tmp/carl-{uid}/{hash:016x}.sock"))
}

fn env_secs(var: &str) -> Option<Duration> {
    std::env::var(var)
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_secs)
}

/// `$<var>/carl` if that is an absolute path (the XDG spec says to ignore
/// relative ones), else `~/<fallback>/carl`, else `.carl` as a last resort.
fn xdg_dir(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|dir| dir.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(fallback)))
        .map(|base| base.join("carl"))
        .unwrap_or_else(|| PathBuf::from(".carl"))
}
