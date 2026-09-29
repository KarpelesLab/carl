//! The daemon: the one long-lived process per user that actually runs Carl.
//!
//! Every `carl` an MCP client launches is a shim (see [`crate::shim`]) that
//! connects here, so all agents on the machine share one `Carl` and its state.
//! It is started on demand by the first shim and exits once no shim has been
//! connected for [`Config::idle_timeout`].
//!
//! Exactly one daemon runs per data dir: it holds a lock on `daemon.lock`
//! there for its whole life, and only the lock holder may (re)bind the socket.
//! Every [`Config::health_interval`] it checks that nobody deleted the lock
//! file or the socket from under it, re-taking or rebinding as needed.
//!
//! Each connection is served on its own threads: blocking socket I/O is bridged
//! into an rmcp session on the tokio runtime through an in-memory duplex pipe.

use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io::{self, BufReader, Write},
    net::Shutdown,
    os::unix::{
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use rmcp::ServiceExt;
use tokio::runtime::Handle;
use tokio_util::io::SyncIoBridge;

use crate::{
    config::Config,
    ipc::{self, ClientHello, DaemonHello},
    server::Carl,
};

/// How long a starting daemon waits for the lock. A daemon that is shutting
/// down holds it for a moment after removing its socket; waiting avoids leaving
/// the shim that spawned us with nobody to talk to.
const LOCK_WAIT: Duration = Duration::from_secs(2);

/// Buffer size of the in-memory pipe between a socket and its rmcp session.
const PIPE_SIZE: usize = 64 * 1024;

pub fn run(config: Config) -> Result<()> {
    // Leave the session of the agent that spawned us, so a Ctrl-C or a closed
    // terminal over there doesn't take down Carl for every other agent. Fails
    // harmlessly if we already lead a session (e.g. launched by hand).
    let _ = rustix::process::setsid();

    config.ensure_data_dir()?;
    let lock_path = config.lock_path();
    let Some(mut lock) = acquire_lock(&lock_path, LOCK_WAIT)? else {
        tracing::info!("another daemon is already running");
        return Ok(());
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let handle = runtime.handle().clone();
    let carl = Carl::new(config.clone());
    let sessions = Arc::new(Sessions::new());
    let mut listener = Listener::bind(&config, &handle, &carl, &sessions)?;
    tracing::info!(pid = std::process::id(), socket = %config.socket.display(), "daemon listening");

    while !sessions.wait_idle(config.idle_timeout, config.health_interval) {
        // The lock file was deleted or replaced (say, `rm -rf` of the data
        // dir). Take the lock again on whatever is there now, unless a newer
        // daemon already holds it: then that one owns the keystore, and we
        // bow out. Our shims reconnect to it.
        if !same_file(&lock, &lock_path)? {
            tracing::warn!(path = %lock_path.display(), "lock file replaced");
            config.ensure_data_dir()?;
            match acquire_lock(&lock_path, Duration::ZERO)? {
                Some(new) => lock = new,
                None => {
                    tracing::warn!("another daemon took over, exiting");
                    runtime.shutdown_background();
                    return Ok(());
                }
            }
        }
        // The socket was deleted (a /tmp cleaner, say): shims can't find us.
        if !listener.is_current()? {
            tracing::warn!(socket = %config.socket.display(), "socket gone, rebinding");
            listener.close();
            listener = Listener::bind(&config, &handle, &carl, &sessions)?;
        }
    }

    // Remove the socket before releasing the lock (on exit), so a shim never
    // connects to a daemon that is going away without being able to start a
    // new one.
    if listener.is_current()? {
        let _ = fs::remove_file(&config.socket);
    }
    tracing::info!("idle, exiting");
    runtime.shutdown_background();
    Ok(())
}

/// Take the daemon lock, waiting up to `wait`. `None` means another daemon
/// holds it. Writes our pid into the lock file.
fn acquire_lock(path: &Path, wait: Duration) -> io::Result<Option<File>> {
    let deadline = Instant::now() + wait;
    loop {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(path)?;
        match file.try_lock() {
            // Locked, but only a lock on the file currently at `path` counts:
            // it may have been deleted or replaced since we opened it.
            Ok(()) if same_file(&file, path)? => {
                file.set_len(0)?;
                writeln!(file, "{}", std::process::id())?;
                return Ok(Some(file));
            }
            Ok(()) => continue,
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(TryLockError::WouldBlock) => return Ok(None),
            Err(TryLockError::Error(e)) => return Err(e),
        }
    }
}

/// Whether `file` is still the file found at `path`.
fn same_file(file: &File, path: &Path) -> io::Result<bool> {
    let ours = file.metadata()?;
    match fs::metadata(path) {
        Ok(theirs) => Ok(ours.dev() == theirs.dev() && ours.ino() == theirs.ino()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// The listening socket, served by its own accept thread.
struct Listener {
    path: PathBuf,
    /// Identity (dev, ino) of the socket file we bound.
    id: (u64, u64),
    /// Clone of the listening socket, kept to shut it down.
    socket: UnixListener,
}

impl Listener {
    /// Bind the configured socket and start accepting on it. Only call while
    /// holding the lock: any file already at the path is then stale.
    fn bind(
        config: &Config,
        handle: &Handle,
        carl: &Carl,
        sessions: &Arc<Sessions>,
    ) -> Result<Self> {
        let path = config.socket.clone();
        config.ensure_socket_dir()?;
        match fs::remove_file(&path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
        let listener =
            UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        let meta = fs::symlink_metadata(&path)?;

        let socket = listener.try_clone()?;
        let (handle, carl, sessions) = (handle.clone(), carl.clone(), sessions.clone());
        thread::spawn(move || accept_loop(listener, handle, carl, sessions));
        Ok(Self {
            path,
            id: (meta.dev(), meta.ino()),
            socket,
        })
    }

    /// Whether our socket file is still the one at the path.
    fn is_current(&self) -> io::Result<bool> {
        match fs::symlink_metadata(&self.path) {
            Ok(meta) => Ok((meta.dev(), meta.ino()) == self.id),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Stop accepting. Established sessions are unaffected.
    fn close(self) {
        // Wakes the accept thread with EINVAL, which ends its loop.
        let _ = rustix::net::shutdown(&self.socket, rustix::net::Shutdown::Both);
    }
}

fn accept_loop(listener: UnixListener, handle: Handle, carl: Carl, sessions: Arc<Sessions>) {
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(e) if e.raw_os_error() == Some(rustix::io::Errno::INVAL.raw_os_error()) => {
                return; // Listener::close
            }
            Err(e) => {
                tracing::warn!(error = %e, "accept failed");
                continue;
            }
        };
        let (handle, carl, sessions) = (handle.clone(), carl.clone(), sessions.clone());
        thread::spawn(move || {
            if let Err(e) = serve_connection(stream, handle, carl, &sessions) {
                tracing::warn!(error = format!("{e:#}"), "connection failed");
            }
        });
    }
}

/// Serve one shim until either side hangs up.
fn serve_connection(
    stream: UnixStream,
    handle: Handle,
    carl: Carl,
    sessions: &Sessions,
) -> Result<()> {
    ipc::check_peer(&stream)?;
    stream.set_read_timeout(Some(ipc::HELLO_TIMEOUT))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let hello: ClientHello = ipc::read_msg(&mut reader).context("reading hello")?;
    let mut writer = stream;
    ipc::write_msg(&mut writer, &DaemonHello::current())?;
    writer.set_read_timeout(None)?;

    let session = sessions.open();
    tracing::info!(
        session = session.id,
        shim_pid = hello.pid,
        shim_ppid = hello.ppid,
        cwd = hello.cwd.as_ref().map(|p| p.display().to_string()),
        shim_version = hello.version,
        "session opened"
    );

    let (server_io, bridge_io) = tokio::io::duplex(PIPE_SIZE);
    let (bridge_read, bridge_write) = tokio::io::split(bridge_io);

    // Socket → session. `reader` may already hold bytes past the hello.
    let inbound = thread::spawn({
        let handle = handle.clone();
        move || {
            let mut to_session = SyncIoBridge::new_with_handle(bridge_write, handle);
            let _ = io::copy(&mut reader, &mut to_session);
            // EOF on the session's input ends the rmcp service.
            let _ = to_session.shutdown();
        }
    });

    // Session → socket.
    let mut socket_out = writer.try_clone()?;
    let outbound = thread::spawn({
        let handle = handle.clone();
        move || {
            let mut from_session = SyncIoBridge::new_with_handle(bridge_read, handle);
            let _ = io::copy(&mut from_session, &mut socket_out);
            // Also unblocks `inbound` if the session ended first.
            let _ = socket_out.shutdown(Shutdown::Both);
        }
    });

    let result = handle.block_on(async move {
        let service = carl.serve(tokio::io::split(server_io)).await?;
        service.waiting().await?;
        anyhow::Ok(())
    });

    let _ = inbound.join();
    let _ = outbound.join();
    tracing::info!(session = session.id, "session closed");
    result
}

/// Live-session bookkeeping, for idle exit and log correlation.
struct Sessions {
    state: Mutex<SessionState>,
    /// Signalled when a session closes.
    closed: Condvar,
}

struct SessionState {
    next_id: u64,
    active: usize,
    /// When `active` last dropped to zero (or daemon start).
    idle_since: Instant,
}

impl Sessions {
    fn new() -> Self {
        Self {
            state: Mutex::new(SessionState {
                next_id: 1,
                active: 0,
                idle_since: Instant::now(),
            }),
            closed: Condvar::new(),
        }
    }

    fn open(&self) -> SessionGuard<'_> {
        let mut state = self.state.lock().unwrap();
        let id = state.next_id;
        state.next_id += 1;
        state.active += 1;
        SessionGuard { sessions: self, id }
    }

    /// Block until no session has been open for `idle_timeout` (returns
    /// `true`), or until `max_wait` has passed (returns `false`).
    fn wait_idle(&self, idle_timeout: Duration, max_wait: Duration) -> bool {
        let deadline = Instant::now() + max_wait;
        let mut state = self.state.lock().unwrap();
        loop {
            let idle_left = (state.active == 0)
                .then(|| idle_timeout.saturating_sub(state.idle_since.elapsed()));
            if idle_left == Some(Duration::ZERO) {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let wait = idle_left.map_or(deadline - now, |left| left.min(deadline - now));
            state = self.closed.wait_timeout(state, wait).unwrap().0;
        }
    }
}

struct SessionGuard<'a> {
    sessions: &'a Sessions,
    id: u64,
}

impl Drop for SessionGuard<'_> {
    fn drop(&mut self) {
        let mut state = self.sessions.state.lock().unwrap();
        state.active -= 1;
        if state.active == 0 {
            state.idle_since = Instant::now();
            self.sessions.closed.notify_all();
        }
    }
}
