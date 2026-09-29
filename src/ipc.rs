//! Shim ↔ daemon plumbing shared by both sides: the hello exchange that opens
//! every connection, and the peer check.
//!
//! After the hello, the connection carries the MCP client's newline-delimited
//! JSON-RPC unchanged, in both directions.

use std::{
    io::{self, BufRead, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    time::Duration,
};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// Wire protocol version. Bump only for incompatible changes: rsupd restarts
/// the daemon on update but not the shims, so a daemon must keep accepting
/// hellos from older shims. Add fields as `#[serde(default)]` instead.
pub const PROTOCOL: u32 = 1;

/// How long either side waits for the other's hello.
pub const HELLO_TIMEOUT: Duration = Duration::from_secs(5);

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// First line a shim sends: who is on the other end of this session.
#[derive(Debug, Serialize, Deserialize)]
pub struct ClientHello {
    pub carl: u32,
    pub version: String,
    /// The shim's pid.
    pub pid: u32,
    /// The shim's parent, normally the MCP client (the agent) itself.
    #[serde(default)]
    pub ppid: Option<u32>,
    /// The shim's working directory, normally the agent's project.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    /// The shim's `CARL_AREAS`: tool areas this session starts with.
    #[serde(default)]
    pub areas: Option<String>,
}

impl ClientHello {
    pub fn current() -> Self {
        Self {
            carl: PROTOCOL,
            version: VERSION.to_string(),
            pid: std::process::id(),
            ppid: Some(std::os::unix::process::parent_id()),
            cwd: std::env::current_dir().ok(),
            areas: std::env::var("CARL_AREAS").ok(),
        }
    }
}

/// The daemon's answer to a [`ClientHello`].
#[derive(Debug, Serialize, Deserialize)]
pub struct DaemonHello {
    pub carl: u32,
    pub version: String,
    pub pid: u32,
}

impl DaemonHello {
    pub fn current() -> Self {
        Self {
            carl: PROTOCOL,
            version: VERSION.to_string(),
            pid: std::process::id(),
        }
    }
}

/// Write `msg` as a single JSON line.
pub fn write_msg<T: Serialize>(w: &mut impl Write, msg: &T) -> io::Result<()> {
    let mut buf = serde_json::to_vec(msg)?;
    buf.push(b'\n');
    w.write_all(&buf)
}

/// Read a single JSON line.
pub fn read_msg<T: DeserializeOwned>(r: &mut impl BufRead) -> io::Result<T> {
    let mut line = String::new();
    if r.read_line(&mut line)? == 0 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok(serde_json::from_str(&line)?)
}

/// Fail unless the process on the other end of `stream` runs as our user.
///
/// Both sides check: the daemon so no other user can drive it, the shim so it
/// never hands an agent's traffic to an impostor.
pub fn check_peer(stream: &UnixStream) -> io::Result<()> {
    let peer = rustix::net::sockopt::socket_peercred(stream)?;
    let us = rustix::process::getuid();
    if peer.uid != us {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "peer runs as uid {}, not {}",
                peer.uid.as_raw(),
                us.as_raw()
            ),
        ));
    }
    Ok(())
}
