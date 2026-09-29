//! The shim: what an MCP client actually launches when it runs `carl`.
//!
//! It holds no state and runs no tools. It connects to the per-user daemon
//! (see [`crate::daemon`]), starting one if needed, then relays
//! newline-delimited JSON-RPC between the client's stdio and the daemon.
//!
//! If the daemon goes away (an rsupd update restarted it, or it crashed), the
//! shim reconnects, starting a new one, and replays the client's `initialize`
//! handshake. The client sees nothing except an error for each request that
//! was in flight at the time.

use std::{
    collections::HashSet,
    env,
    fs::{self, OpenOptions},
    io::{self, BufRead, BufReader, Write},
    net::Shutdown,
    os::unix::{fs::OpenOptionsExt, net::UnixStream},
    path::PathBuf,
    process::{Command, Stdio},
    sync::{Arc, Mutex, OnceLock},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use serde_json::Value;

use crate::{
    config::Config,
    ipc::{self, ClientHello, DaemonHello},
};

/// How long to keep trying to reach (or start) the daemon at startup.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Delay before spawning another daemon when the last one isn't listening yet;
/// doubles up to [`RESPAWN_MAX`].
const RESPAWN_AFTER: Duration = Duration::from_secs(2);
const RESPAWN_MAX: Duration = Duration::from_secs(16);

/// State shared by the two relay directions.
struct Link {
    /// Write half of the current daemon connection.
    writer: UnixStream,
    /// JSON-encoded ids of client requests the daemon hasn't answered yet.
    pending: HashSet<String>,
    /// The client's `initialize` request: (JSON-encoded id, raw line).
    initialize: Option<(String, String)>,
    /// The client's `notifications/initialized`, raw line.
    initialized: Option<String>,
}

pub fn run(config: Config) -> Result<()> {
    self_exe().context("locating our own binary")?;
    let (reader, writer) = connect(&config, CONNECT_TIMEOUT)?;
    let link = Arc::new(Mutex::new(Link {
        writer,
        pending: HashSet::new(),
        initialize: None,
        initialized: None,
    }));

    let daemon_link = link.clone();
    thread::spawn(move || {
        if let Err(e) = daemon_to_client(reader, &config, &daemon_link) {
            tracing::error!(error = format!("{e:#}"), "lost the carl daemon");
            std::process::exit(1);
        }
    });

    client_to_daemon(&link)
}

/// Relay stdin to the daemon until the client closes it.
fn client_to_daemon(link: &Mutex<Link>) -> Result<()> {
    for line in io::stdin().lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let msg: Option<Value> = serde_json::from_str(&line).ok();
        let method = msg.as_ref().and_then(|m| m.get("method")?.as_str());
        let id = msg.as_ref().and_then(|m| m.get("id")).map(Value::to_string);

        let mut link = link.lock().unwrap();
        match (method, id) {
            (Some(method), Some(id)) => {
                if method == "initialize" {
                    link.initialize = Some((id.clone(), line.clone()));
                }
                link.pending.insert(id);
            }
            (Some("notifications/initialized"), None) => {
                link.initialized = Some(line.clone());
            }
            _ => {}
        }
        // A failed write means the daemon is gone. The other direction sees
        // the same and reconnects, failing whatever is pending, this included.
        let _ = link.writer.write_all(format!("{line}\n").as_bytes());
    }

    // The client hung up: let the daemon close the session promptly.
    let _ = link.lock().unwrap().writer.shutdown(Shutdown::Write);
    Ok(())
}

/// Relay the daemon to stdout, reconnecting whenever the daemon goes away.
fn daemon_to_client(
    mut reader: BufReader<UnixStream>,
    config: &Config,
    link: &Mutex<Link>,
) -> Result<()> {
    let mut stdout = io::stdout().lock();
    let mut line = String::new();
    loop {
        line.clear();
        let complete =
            matches!(reader.read_line(&mut line), Ok(n) if n > 0) && line.ends_with('\n');
        if !complete {
            // EOF, error, or a line cut short by a dying daemon.
            reader = reconnect(config, link, &mut stdout)?;
            continue;
        }
        if let Some(id) = response_id(&line) {
            link.lock().unwrap().pending.remove(&id);
        }
        stdout.write_all(line.as_bytes())?;
        stdout.flush()?;
    }
}

/// Connect to a (possibly new) daemon and bring it to where the old one was.
fn reconnect(
    config: &Config,
    link: &Mutex<Link>,
    stdout: &mut impl Write,
) -> Result<BufReader<UnixStream>> {
    tracing::warn!("lost connection to the carl daemon, reconnecting");
    // Hold the link throughout, so the client's messages queue up behind the
    // replay instead of racing it.
    let mut link = link.lock().unwrap();
    // A daemon whose socket was deleted can't be replaced (it holds the lock)
    // and only notices at its next health check, so wait out a full interval
    // rather than dropping the client.
    let (mut reader, writer) = connect(config, CONNECT_TIMEOUT.max(config.health_interval * 2))?;
    link.writer = writer;

    // Requests the old daemon never answered died with it: fail them so the
    // client isn't left waiting. An unanswered `initialize` is the exception,
    // since the replay below answers it.
    let init = link.initialize.clone();
    let init_answered = init
        .as_ref()
        .is_some_and(|(id, _)| !link.pending.contains(id));
    for id in link.pending.drain() {
        if init.as_ref().is_some_and(|(init_id, _)| *init_id == id) {
            continue;
        }
        writeln!(
            stdout,
            r#"{{"jsonrpc":"2.0","id":{id},"error":{{"code":-32603,"message":"Carl's daemon restarted before answering; retry the request."}}}}"#
        )?;
    }
    stdout.flush()?;

    let Some((init_id, init_line)) = init else {
        return Ok(reader);
    };
    link.writer.write_all(format!("{init_line}\n").as_bytes())?;
    if !init_answered {
        // The client is still waiting for this reply; let it through.
        link.pending.insert(init_id);
        return Ok(reader);
    }

    // The client already has its `initialize` reply, so swallow the new one.
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            bail!("daemon closed the connection during initialize");
        }
        if response_id(&line).as_ref() == Some(&init_id) {
            break;
        }
        stdout.write_all(line.as_bytes())?;
    }
    if let Some(initialized) = link.initialized.clone() {
        link.writer
            .write_all(format!("{initialized}\n").as_bytes())?;
        // The new daemon may expose other tools (an update added some, or the
        // session's areas changed): have the client fetch the list again.
        writeln!(
            stdout,
            r#"{{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}}"#
        )?;
        stdout.flush()?;
    }
    tracing::info!("reconnected to the carl daemon");
    Ok(reader)
}

/// JSON-encoded id of `line` if it is a JSON-RPC response.
fn response_id(line: &str) -> Option<String> {
    let msg: Value = serde_json::from_str(line).ok()?;
    if msg.get("method").is_some() {
        return None;
    }
    msg.get("id").map(Value::to_string)
}

/// Reach the daemon, starting one if nobody is listening.
fn connect(config: &Config, timeout: Duration) -> Result<(BufReader<UnixStream>, UnixStream)> {
    let socket = &config.socket;
    let deadline = Instant::now() + timeout;
    let mut last_spawn: Option<Instant> = None;
    let mut respawn_after = RESPAWN_AFTER;
    let mut delay = Duration::from_millis(10);
    loop {
        let err = match UnixStream::connect(socket) {
            Ok(stream) => match handshake(stream) {
                Ok(conn) => return Ok(conn),
                Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                    return Err(e).context("refusing to talk to the process on the daemon socket");
                }
                // Most likely a daemon that is shutting down.
                Err(e) => e,
            },
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                if last_spawn.is_none_or(|t| t.elapsed() >= respawn_after) {
                    if last_spawn.is_some() {
                        respawn_after = (respawn_after * 2).min(RESPAWN_MAX);
                    }
                    spawn_daemon(config)?;
                    last_spawn = Some(Instant::now());
                }
                e
            }
            Err(e) => return Err(e).with_context(|| format!("connecting to {}", socket.display())),
        };
        if Instant::now() >= deadline {
            return Err(err).with_context(|| {
                format!(
                    "no carl daemon on {} (see {})",
                    socket.display(),
                    config.log_path().display()
                )
            });
        }
        thread::sleep(delay);
        delay = (delay * 2).min(Duration::from_millis(200));
    }
}

fn handshake(stream: UnixStream) -> io::Result<(BufReader<UnixStream>, UnixStream)> {
    ipc::check_peer(&stream)?;
    stream.set_read_timeout(Some(ipc::HELLO_TIMEOUT))?;
    let mut writer = stream.try_clone()?;
    ipc::write_msg(&mut writer, &ClientHello::current())?;
    let mut reader = BufReader::new(stream);
    let hello: DaemonHello = ipc::read_msg(&mut reader)?;
    reader.get_ref().set_read_timeout(None)?;
    if hello.version != ipc::VERSION {
        tracing::info!(
            daemon = hello.version,
            shim = ipc::VERSION,
            "daemon runs a different version"
        );
    }
    tracing::debug!(daemon_pid = hello.pid, "connected to the carl daemon");
    Ok((reader, writer))
}

/// Start `carl daemon` in the background, logging to the log directory.
fn spawn_daemon(config: &Config) -> Result<()> {
    config.ensure_data_dir()?;
    config.ensure_log_dir()?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(config.log_path())?;
    let mut child = Command::new(self_exe()?)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .context("starting the carl daemon")?;
    tracing::debug!(pid = child.id(), "spawned the carl daemon");
    // Reap it if it exits while we live, e.g. after losing the startup race.
    thread::spawn(move || child.wait());
    Ok(())
}

/// Path of our own binary, as it was when we started. It must be captured
/// early: an update renames the running binary aside before deleting it, after
/// which the kernel reports our executable as the renamed, deleted file. The
/// new build is at the original path, and that is the daemon we want to start.
static SELF_EXE: OnceLock<PathBuf> = OnceLock::new();

fn self_exe() -> io::Result<&'static PathBuf> {
    if let Some(exe) = SELF_EXE.get() {
        return Ok(exe);
    }
    let exe = env::current_exe()?;
    Ok(SELF_EXE.get_or_init(|| fs::canonicalize(&exe).unwrap_or(exe)))
}
