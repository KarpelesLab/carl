//! End-to-end tests of the shim/daemon split: real `carl` processes, each test
//! with its own data directory (and therefore its own daemon).

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{Receiver, channel},
    thread,
    time::{Duration, Instant},
};

use serde_json::{Value, json};

const TIMEOUT: Duration = Duration::from_secs(10);

/// A fresh data dir, unique to this test run: a previous run's daemons may
/// still be winding down in theirs.
fn data_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    dir
}

/// An MCP client driving one `carl` shim.
struct Client {
    child: Child,
    stdin: Option<ChildStdin>,
    responses: Receiver<Value>,
    /// Notifications received while waiting for a response.
    notifications: std::cell::RefCell<std::collections::VecDeque<Value>>,
}

impl Client {
    fn start(data: &Path) -> Self {
        Self::start_in(data, Path::new(env!("CARGO_TARGET_TMPDIR")))
    }

    /// Start a client whose shim runs in `cwd`, as an agent started there.
    fn start_in(data: &Path, cwd: &Path) -> Self {
        Self::spawn(data, cwd, None)
    }

    fn spawn(data: &Path, cwd: &Path, areas: Option<&str>) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_carl"));
        if let Some(areas) = areas {
            command.env("CARL_AREAS", areas);
        }
        let mut child = command
            .current_dir(cwd)
            .env("CARL_DATA_DIR", data)
            .env("CARL_IDLE_TIMEOUT", "1")
            .env("CARL_HEALTH_INTERVAL", "1")
            .env("RUST_LOG", "debug")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, responses) = channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(serde_json::from_str(&line).unwrap()).is_err() {
                    break;
                }
            }
        });
        let mut client = Self {
            stdin: child.stdin.take(),
            child,
            responses,
            notifications: Default::default(),
        };
        client.send(json!({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                       "clientInfo": {"name": "test", "version": "0"}},
        }));
        let init = client.recv();
        assert_eq!(init["result"]["serverInfo"]["name"], "carl", "{init}");
        client.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        client
    }

    fn send(&mut self, msg: Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{msg}").unwrap();
        stdin.flush().unwrap();
    }

    /// The next response; notifications arriving first are set aside.
    fn recv(&self) -> Value {
        loop {
            let msg = self.responses.recv_timeout(TIMEOUT).expect("no response");
            if msg.get("id").is_some() {
                return msg;
            }
            self.notifications.borrow_mut().push_back(msg);
        }
    }

    /// The next notification (set aside earlier, or still to come).
    fn notification(&self) -> Value {
        if let Some(msg) = self.notifications.borrow_mut().pop_front() {
            return msg;
        }
        let msg = self
            .responses
            .recv_timeout(TIMEOUT)
            .expect("no notification");
        assert!(
            msg.get("id").is_none(),
            "expected a notification, got {msg}"
        );
        msg
    }

    /// Call `tool`; returns (is_error, text parsed as JSON or as a string).
    fn call(&mut self, id: u64, tool: &str, args: Value) -> (bool, Value) {
        self.send(json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": tool, "arguments": args},
        }));
        self.parse(self.recv())
    }

    fn parse(&self, resp: Value) -> (bool, Value) {
        let result = &resp["result"];
        let text = result["content"][0]["text"].as_str().unwrap_or_default();
        let value = serde_json::from_str(text).unwrap_or_else(|_| json!(text));
        (result["isError"] == json!(true), value)
    }

    /// Names of the tools this session currently exposes.
    fn tool_names(&mut self, id: u64) -> Vec<String> {
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": "tools/list"}));
        let resp = self.recv();
        let mut names: Vec<String> = resp["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap().to_string())
            .collect();
        names.sort();
        names
    }

    fn ping(&mut self, id: u64) -> Value {
        self.send(json!({
            "jsonrpc": "2.0", "id": id, "method": "tools/call",
            "params": {"name": "carl_ping", "arguments": {}},
        }));
        self.recv()
    }

    fn assert_pong(&mut self, id: u64) {
        let resp = self.ping(id);
        assert_eq!(resp["id"], id, "{resp}");
        assert_eq!(resp["result"]["content"][0]["text"], "pong", "{resp}");
    }

    /// Close stdin, as a client does on shutdown, and wait for the shim.
    fn close(mut self) {
        drop(self.stdin.take());
        let deadline = Instant::now() + TIMEOUT;
        while self.child.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "shim did not exit");
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Pid of the daemon currently holding the data dir's lock.
fn daemon_pid(data: &Path) -> u32 {
    let mut pid = None;
    wait_until("the lock file names a daemon", || {
        pid = fs::read_to_string(data.join("daemon.lock"))
            .ok()
            .and_then(|s| s.trim().parse().ok());
        pid.is_some()
    });
    pid.unwrap()
}

/// The socket the (last started) daemon listens on, from its log.
fn socket_path(data: &Path) -> PathBuf {
    let log = fs::read_to_string(data.join("daemon.log")).unwrap();
    let line = log
        .lines()
        .rfind(|l| l.contains("daemon listening"))
        .unwrap();
    PathBuf::from(line.split_once("socket=").unwrap().1.trim())
}

/// Running, as opposed to gone or a zombie. Zombies matter in containers, where
/// PID 1 may never reap the orphaned daemon.
fn alive(pid: u32) -> bool {
    fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| Some(stat.rsplit_once(") ")?.1.starts_with(|c| c != 'Z')))
        .unwrap_or(false)
}

fn wait_until(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + TIMEOUT;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn relays_tool_calls_through_daemon() {
    let data = data_dir("relay");
    let mut client = Client::start(&data);
    client.assert_pong(1);
    assert!(alive(daemon_pid(&data)));
    client.close();
}

#[test]
fn agents_share_one_daemon() {
    let data = data_dir("shared");
    let mut a = Client::start(&data);
    let pid = daemon_pid(&data);
    let mut b = Client::start(&data);
    a.assert_pong(1);
    b.assert_pong(1);
    assert_eq!(daemon_pid(&data), pid);

    let log = fs::read_to_string(data.join("daemon.log")).unwrap();
    assert_eq!(log.matches("daemon listening").count(), 1, "{log}");
    assert_eq!(log.matches("session opened").count(), 2, "{log}");
}

#[test]
fn reconnects_after_daemon_dies() {
    let data = data_dir("reconnect");
    let mut client = Client::start(&data);
    client.assert_pong(1);
    let old = daemon_pid(&data);

    let pid = rustix::process::Pid::from_raw(old as i32).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::KILL).unwrap();
    wait_until("the old daemon is gone", || !alive(old));
    wait_until("a new daemon is up", || {
        fs::read_to_string(data.join("daemon.lock")).is_ok_and(|pid| pid.trim().parse() != Ok(old))
    });

    // Same session, no new initialize from the client.
    client.assert_pong(2);
    assert_ne!(daemon_pid(&data), old);
}

#[test]
fn daemon_exits_when_idle() {
    let data = data_dir("idle");
    let mut client = Client::start(&data);
    client.assert_pong(1);
    let pid = daemon_pid(&data);
    let socket = socket_path(&data);
    assert!(socket.starts_with(format!("/tmp/carl-{}", rustix::process::getuid().as_raw())));
    client.close();

    wait_until("the daemon exits", || !alive(pid));
    assert!(!socket.exists());
}

#[test]
fn concurrent_starts_elect_one_daemon() {
    let data = data_dir("race");
    let clients: Vec<Client> = thread::scope(|s| {
        let starts: Vec<_> = (0..10).map(|_| s.spawn(|| Client::start(&data))).collect();
        starts.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for (i, mut client) in clients.into_iter().enumerate() {
        client.assert_pong(i as u64 + 1);
    }

    let log = fs::read_to_string(data.join("daemon.log")).unwrap();
    assert_eq!(log.matches("daemon listening").count(), 1, "{log}");
    assert_eq!(log.matches("session opened").count(), 10, "{log}");
}

#[test]
fn rebinds_a_deleted_socket() {
    let data = data_dir("rebind");
    let mut a = Client::start(&data);
    a.assert_pong(1);
    let pid = daemon_pid(&data);
    let socket = socket_path(&data);

    fs::remove_file(&socket).unwrap(); // as a /tmp cleaner would
    wait_until("the socket is back", || socket.exists());

    let mut b = Client::start(&data);
    b.assert_pong(1);
    a.assert_pong(2);
    assert_eq!(daemon_pid(&data), pid);
    let log = fs::read_to_string(data.join("daemon.log")).unwrap();
    assert_eq!(log.matches("daemon listening").count(), 1, "{log}");
}

#[test]
fn one_daemon_survives_a_deleted_lock() {
    let data = data_dir("relock");
    let mut a = Client::start(&data);
    a.assert_pong(1);
    let old = daemon_pid(&data);

    // As `rm -rf` of the data dir would, minus the log we inspect. A new
    // client then likely starts a second daemon on a fresh lock before the
    // old one's health check runs; either way, only one may remain.
    fs::remove_file(socket_path(&data)).unwrap();
    fs::remove_file(data.join("daemon.lock")).unwrap();
    let mut b = Client::start(&data);
    b.assert_pong(1);

    let owner = daemon_pid(&data);
    if owner != old {
        wait_until("the old daemon yields", || !alive(old));
    }
    assert!(alive(owner));
    a.assert_pong(2);
    b.assert_pong(2);
}

#[test]
fn agents_describe_list_and_message_each_other() {
    let data = data_dir("agents");
    let (dir_a, dir_b) = (data.join("proj-a"), data.join("proj-b"));
    fs::create_dir_all(&dir_a).unwrap();
    fs::create_dir_all(&dir_b).unwrap();
    let mut a = Client::start_in(&data, &dir_a);
    let mut b = Client::start_in(&data, &dir_b);

    let (err, me) = a.call(
        1,
        "agent_describe",
        json!({"task": "refactoring billing", "name": "biller"}),
    );
    assert!(!err, "{me}");
    assert_eq!(me["name"], "biller");

    // B sees A: where it started, which client, what it's doing.
    let (_, list) = b.call(1, "agent_list", json!({}));
    let agents = list["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 2, "{list}");
    let seen_a = agents.iter().find(|x| x["name"] == "biller").unwrap();
    assert_eq!(seen_a["cwd"], dir_a.display().to_string());
    assert_eq!(seen_a["client"], "test 0");
    assert_eq!(seen_a["task"], "refactoring billing");
    assert_eq!(seen_a["you"], false);
    let (_, me_b) = b.call(2, "agent_whoami", json!({}));
    assert_eq!(me_b["name"], "test@proj-b");

    // A waits for mail; B's message wakes it up.
    a.send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                  "params": {"name": "agent_inbox", "arguments": {"wait_seconds": 30}}}));
    thread::sleep(Duration::from_millis(300));
    let started = Instant::now();
    let (err, sent) = b.call(
        3,
        "agent_send",
        json!({"to": "biller", "message": "leave src/billing to me"}),
    );
    assert!(!err, "{sent}");
    let (_, inbox) = a.parse(a.recv());
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "the wait wasn't woken"
    );
    assert!(inbox["notice"].as_str().unwrap().contains("not the user"));
    let message = &inbox["data"]["messages"][0];
    assert_eq!(message["text"], "leave src/billing to me");
    assert_eq!(message["from"]["name"], "test@proj-b");
    assert_eq!(message["from"]["cwd"], dir_b.display().to_string());

    // Read messages are gone; a disconnected agent leaves the list.
    let (_, inbox) = a.call(3, "agent_inbox", json!({}));
    assert_eq!(inbox["data"]["messages"], json!([]));
    b.close();
    wait_until("B leaves the agent list", || {
        let (_, list) = a.call(4, "agent_list", json!({}));
        list["agents"].as_array().unwrap().len() == 1
    });
    let (err, _) = a.call(5, "agent_send", json!({"to": "all", "message": "anyone?"}));
    assert!(err);
}

#[test]
fn agent_tools_need_the_daemon() {
    let data = data_dir("standalone");
    let mut child = Command::new(env!("CARGO_BIN_EXE_carl"))
        .arg("standalone")
        .env("CARL_DATA_DIR", &data)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    for msg in [
        json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
               "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                          "clientInfo": {"name": "t", "version": "0"}}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
               "params": {"name": "agent_list", "arguments": {}}}),
    ] {
        writeln!(stdin, "{msg}").unwrap();
    }
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    lines.next(); // initialize
    let resp: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    assert_eq!(resp["result"]["isError"], true, "{resp}");
    drop(stdin);
    let _ = child.wait();
}

#[test]
fn areas_are_enabled_per_session_on_demand() {
    let data = data_dir("areas");
    let mut a = Client::start(&data);
    let mut b = Client::start(&data);

    // A fresh session sees the carl_* and agent_* tools only.
    let tools = a.tool_names(1);
    assert!(
        tools
            .iter()
            .all(|t| t.starts_with("carl_") || t.starts_with("agent_")),
        "{tools:?}"
    );
    assert!(tools.contains(&"carl_enable".to_string()));
    let (err, msg) = a.call(2, "google_mail_search", json!({"query": "x"}));
    assert!(
        err && msg.as_str().unwrap().contains("carl_enable"),
        "{msg}"
    );

    // Enabling google.mail notifies the client, then its tools are listed.
    a.send(json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
                  "params": {"name": "carl_enable", "arguments": {"areas": ["google.mail"]}}}));
    let response = a.recv();
    assert_eq!(
        a.notification()["method"],
        "notifications/tools/list_changed"
    );
    let (_, result) = a.parse(response);
    assert!(
        result["added_tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t == "google_link")
    );
    let tools = a.tool_names(4);
    assert!(tools.contains(&"google_mail_search".to_string()));
    assert!(
        tools.contains(&"google_link".to_string()),
        "account tools come along"
    );
    assert!(!tools.contains(&"google_drive_search".to_string()));

    // Other sessions are unaffected.
    assert!(!b.tool_names(1).contains(&"google_mail_search".to_string()));

    // Scaffolded areas can't be enabled; disabling removes the tools again.
    let (err, _) = a.call(5, "carl_enable", json!({"areas": ["wallet"]}));
    assert!(err);
    a.send(json!({"jsonrpc": "2.0", "id": 6, "method": "tools/call",
                  "params": {"name": "carl_disable", "arguments": {"areas": ["google.mail"]}}}));
    let _ = (a.recv(), a.notification());
    let tools = a.tool_names(7);
    assert!(!tools.iter().any(|t| t.starts_with("google_")), "{tools:?}");
}

#[test]
fn carl_areas_presets_a_session() {
    let data = data_dir("areas-env");
    let mut client = Client::spawn(
        &data,
        Path::new(env!("CARGO_TARGET_TMPDIR")),
        Some("google.drive"),
    );
    let tools = client.tool_names(1);
    assert!(
        tools.contains(&"google_drive_read".to_string()),
        "{tools:?}"
    );
    assert!(
        !tools.contains(&"agent_list".to_string()),
        "CARL_AREAS replaces the defaults"
    );
}

#[test]
fn sessions_resume_after_the_daemon_dies() {
    let data = data_dir("resume");
    let mut client = Client::start(&data);
    let (err, _) = client.call(
        1,
        "agent_describe",
        json!({"task": "long job", "name": "survivor"}),
    );
    assert!(!err);
    client.send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                       "params": {"name": "carl_enable", "arguments": {"areas": ["google.drive"]}}}));
    let _ = (client.recv(), client.notification());

    let old = daemon_pid(&data);
    let pid = rustix::process::Pid::from_raw(old as i32).unwrap();
    rustix::process::kill_process(pid, rustix::process::Signal::KILL).unwrap();
    wait_until("the old daemon is gone", || !alive(old));

    // After reconnecting, the shim tells the client to refetch its tools.
    let notification = client.notification();
    assert_eq!(
        notification["method"], "notifications/tools/list_changed",
        "{notification}"
    );

    // The new daemon restored the session: areas, name and task.
    let tools = client.tool_names(3);
    assert!(
        tools.contains(&"google_drive_read".to_string()),
        "{tools:?}"
    );
    assert!(
        !tools.contains(&"google_mail_search".to_string()),
        "only the areas it had come back: {tools:?}"
    );
    let (_, me) = client.call(4, "agent_whoami", json!({}));
    assert_eq!(me["name"], "survivor");
    assert_eq!(me["task"], "long job");
    assert_ne!(daemon_pid(&data), old);
}

#[test]
fn an_oversized_log_is_rotated_and_logging_continues() {
    let data = data_dir("logrotate");
    fs::create_dir_all(&data).unwrap();
    let log = data.join("daemon.log");
    fs::write(&log, "old line\n".repeat(1_200_000)).unwrap(); // ~10.8 MB

    let mut client = Client::start(&data);
    client.assert_pong(1);
    let (_, _) = client.call(2, "agent_describe", json!({"task": "rotating"}));

    let old = fs::read_to_string(data.join("daemon.log.1")).unwrap();
    assert!(old.starts_with("old line\n"), "the big log moved to .1");
    wait_until("the new log gets the daemon's lines", || {
        fs::read_to_string(&log)
            .is_ok_and(|l| l.contains("log rotated") && l.contains("session opened"))
    });
    assert!(fs::metadata(&log).unwrap().len() < 1024 * 1024);
}

#[test]
fn agent_messages_are_pushed_as_channel_events() {
    let data = data_dir("agentpush");
    let mut a = Client::start(&data);
    let mut b = Client::start(&data);
    let (_, me) = b.call(
        1,
        "agent_describe",
        json!({"task": "listening", "name": "listener"}),
    );
    assert_eq!(me["name"], "listener");
    let (_, whoami) = a.call(1, "agent_whoami", json!({}));

    let (err, _) = a.call(
        2,
        "agent_send",
        json!({"to": "listener", "message": "build is green"}),
    );
    assert!(!err);
    let event = b.notification();
    assert_eq!(event["method"], "notifications/claude/channel", "{event}");
    let content = event["params"]["content"].as_str().unwrap();
    assert!(content.contains("build is green"), "{content}");
    assert!(content.contains("not the user"), "{content}");
    assert_eq!(event["params"]["meta"]["kind"], "agent_message");
    assert_eq!(event["params"]["meta"]["from_id"], whoami["id"].to_string());

    // Long messages are clipped in the event; the inbox has them whole.
    let long = "x".repeat(3000);
    a.call(3, "agent_send", json!({"to": "listener", "message": long}));
    let content = b.notification()["params"]["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        content.contains("truncated") && content.len() < 2600,
        "{}",
        content.len()
    );
    let (_, inbox) = b.call(2, "agent_inbox", json!({}));
    assert_eq!(
        inbox["data"]["messages"][1]["text"].as_str().unwrap().len(),
        3000
    );
}
