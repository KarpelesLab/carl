//! Registry of the agents connected to the daemon, and their message inboxes.
//!
//! Each shim connection is one agent. Its id is the shim's pid, so it stays
//! the same when the shim reconnects after a daemon restart. What an agent
//! set up (enabled areas, the name and task it gave, unread messages) is also
//! saved under `<data dir>/sessions/`, keyed by the shim's pid and start time,
//! and restored when that shim reconnects: a daemon update or crash doesn't
//! reset anyone's session. A clean disconnect deletes the file.

use std::{
    collections::{HashMap, VecDeque},
    fs,
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::google::encoding::{rfc3339, unix_now};
use crate::ipc::ClientHello;

/// Most unread messages kept per agent; older ones are dropped.
const INBOX_LIMIT: usize = 200;

/// Longest message, in characters.
pub const MAX_MESSAGE_CHARS: usize = 16_000;

/// Longest self-description, in characters.
const MAX_TASK_CHARS: usize = 500;

#[derive(Default)]
pub struct Agents {
    inner: Mutex<Inner>,
    /// Where session state is saved; `None` keeps it in memory only.
    dir: Option<PathBuf>,
    /// Set as the daemon exits: sessions ending then will resume elsewhere,
    /// so their saved state is kept.
    shutting_down: AtomicBool,
}

#[derive(Default)]
struct Inner {
    agents: HashMap<u32, Agent>,
    next_message: u64,
}

struct Agent {
    id: u32,
    /// `<pid>-<start time>` of the shim: names its saved state.
    key: Option<String>,
    /// Chosen by the agent, else derived from its client and directory.
    name: String,
    named: bool,
    /// MCP client, e.g. `claude-code 2.1.0`.
    client: Option<String>,
    /// The agent process (the shim's parent).
    pid: Option<u32>,
    /// Where the agent was started.
    cwd: Option<PathBuf>,
    /// What the agent says it is doing.
    task: Option<String>,
    connected_at: u64,
    task_updated_at: Option<u64>,
    /// Enabled tool areas, once the session changed them.
    areas: Option<Vec<String>>,
    inbox: VecDeque<Value>,
    /// Wakes a pending `agent_inbox` wait.
    notify: Arc<Notify>,
}

/// What survives a daemon restart.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Saved {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    task: Option<String>,
    #[serde(default)]
    task_updated_at: Option<u64>,
    #[serde(default)]
    areas: Option<Vec<String>>,
    #[serde(default)]
    inbox: Vec<Value>,
}

impl Agent {
    fn describe(&self, viewer: u32) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "you": self.id == viewer,
            "client": self.client,
            "cwd": self.cwd.as_ref().map(|p| p.display().to_string()),
            "pid": self.pid,
            "task": self.task,
            "task_updated_at": self.task_updated_at.map(rfc3339),
            "connected_at": rfc3339(self.connected_at),
        })
    }

    fn saved(&self) -> Saved {
        Saved {
            // A default name is rebuilt from the client on reconnect.
            name: self.named.then(|| self.name.clone()),
            task: self.task.clone(),
            task_updated_at: self.task_updated_at,
            areas: self.areas.clone(),
            inbox: self.inbox.iter().cloned().collect(),
        }
    }
}

impl Agents {
    /// A registry saving session state under `dir`.
    pub fn new(dir: Option<PathBuf>) -> Self {
        Self {
            dir,
            ..Self::default()
        }
    }

    /// Register a new connection. Returns its agent id, and the tool areas
    /// its previous session had enabled, if it is resuming one.
    pub fn register(&self, hello: &ClientHello) -> (u32, Option<Vec<String>>) {
        let dir = hello
            .cwd
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "agent".into());
        let key = process_key(hello.pid);
        let saved = key
            .as_deref()
            .and_then(|k| self.load(k))
            .unwrap_or_default();

        let mut inner = self.inner.lock().unwrap();
        // Keep a saved name unless someone took it in the meantime.
        let name = saved
            .name
            .filter(|n| !inner.agents.values().any(|a| &a.name == n));
        let agent = Agent {
            id: hello.pid,
            key,
            named: name.is_some(),
            name: name.unwrap_or(dir),
            client: None,
            pid: hello.ppid,
            cwd: hello.cwd.clone(),
            task: saved.task,
            connected_at: unix_now(),
            task_updated_at: saved.task_updated_at,
            areas: saved.areas.clone(),
            inbox: saved.inbox.into(),
            notify: Arc::new(Notify::new()),
        };
        if !agent.inbox.is_empty() {
            agent.notify.notify_one();
        }
        inner.agents.insert(hello.pid, agent);
        (hello.pid, saved.areas)
    }

    /// Record the MCP client from the `initialize` handshake.
    pub fn set_client(&self, id: u32, name: &str, version: &str) {
        if let Some(a) = self.inner.lock().unwrap().agents.get_mut(&id) {
            a.client = Some(format!("{name} {version}"));
            if !a.named {
                // e.g. "claude-code@carl"
                a.name = format!("{name}@{}", a.name);
            }
        }
    }

    /// Forget a session that ended. Its saved state goes too, unless the
    /// daemon is exiting (the shim will resume it with the next daemon).
    pub fn unregister(&self, id: u32) {
        let removed = self.inner.lock().unwrap().agents.remove(&id);
        if let Some(agent) = removed
            && !self.shutting_down.load(Ordering::SeqCst)
            && let (Some(dir), Some(key)) = (&self.dir, &agent.key)
        {
            let _ = fs::remove_file(dir.join(format!("{key}.json")));
        }
    }

    /// The daemon is exiting: keep the state of sessions ending from now on.
    pub fn shutting_down(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
    }

    /// Delete saved state of shims that no longer exist.
    pub fn prune(&self) {
        let Some(entries) = self.dir.as_ref().and_then(|d| fs::read_dir(d).ok()) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(key) = name.strip_suffix(".json") else {
                continue;
            };
            let live = key
                .split_once('-')
                .and_then(|(pid, _)| pid.parse().ok())
                .and_then(process_key)
                .is_some_and(|k| k == key);
            if !live {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    /// Remember the session's enabled tool areas.
    pub fn set_areas(&self, id: u32, areas: Vec<String>) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(agent) = inner.agents.get_mut(&id) {
            agent.areas = Some(areas);
            self.save(agent);
        }
    }

    /// Set what agent `id` is working on, and optionally its name.
    pub fn describe(&self, id: u32, task: &str, name: Option<&str>) -> Result<Value> {
        let mut inner = self.inner.lock().unwrap();
        if let Some(name) = name.map(str::trim) {
            if name.is_empty() || name.len() > 64 || name.contains(char::is_whitespace) {
                bail!("a name must be 1-64 characters without spaces");
            }
            if inner.agents.values().any(|a| a.id != id && a.name == name) {
                bail!("another agent is already called {name}");
            }
        }
        let agent = inner
            .agents
            .get_mut(&id)
            .ok_or_else(|| anyhow!("not registered"))?;
        agent.task = Some(clip(task.trim(), MAX_TASK_CHARS));
        agent.task_updated_at = Some(unix_now());
        if let Some(name) = name {
            agent.name = name.trim().to_string();
            agent.named = true;
        }
        self.save(agent);
        Ok(agent.describe(id))
    }

    pub fn whoami(&self, id: u32) -> Result<Value> {
        let inner = self.inner.lock().unwrap();
        let agent = inner
            .agents
            .get(&id)
            .ok_or_else(|| anyhow!("not registered"))?;
        let mut me = agent.describe(id);
        me["unread"] = json!(agent.inbox.len());
        Ok(me)
    }

    /// Every connected agent, as seen by `viewer`.
    pub fn list(&self, viewer: u32) -> Value {
        let inner = self.inner.lock().unwrap();
        let mut agents: Vec<&Agent> = inner.agents.values().collect();
        agents.sort_by_key(|a| (a.connected_at, a.id));
        let unread = inner.agents.get(&viewer).map_or(0, |a| a.inbox.len());
        json!({
            "agents": agents.iter().map(|a| a.describe(viewer)).collect::<Vec<_>>(),
            "your_unread_messages": unread,
        })
    }

    /// Queue `text` from `from` for the agent(s) `to` names: an id, a name, or
    /// `all` for every other agent. Returns the recipients' ids.
    pub fn send(&self, from: u32, to: &str, text: &str) -> Result<Vec<u32>> {
        if text.trim().is_empty() {
            bail!("empty message");
        }
        if text.chars().count() > MAX_MESSAGE_CHARS {
            bail!("messages are limited to {MAX_MESSAGE_CHARS} characters");
        }
        let mut inner = self.inner.lock().unwrap();
        let sender = inner
            .agents
            .get(&from)
            .ok_or_else(|| anyhow!("not registered"))?;
        let (from_name, from_cwd) = (sender.name.clone(), sender.cwd.clone());

        let to = to.trim();
        let recipients: Vec<u32> = if to == "all" {
            inner
                .agents
                .keys()
                .copied()
                .filter(|&id| id != from)
                .collect()
        } else if let Ok(id) = to.parse::<u32>()
            && inner.agents.contains_key(&id)
        {
            vec![id]
        } else {
            let named: Vec<u32> = inner
                .agents
                .values()
                .filter(|a| a.name == to)
                .map(|a| a.id)
                .collect();
            match named.as_slice() {
                [] => bail!("no connected agent is {to}; see agent_list"),
                [id] => vec![*id],
                _ => bail!("several agents are called {to}; use its id"),
            }
        };
        if recipients.contains(&from) {
            bail!("that's you");
        }
        if recipients.is_empty() {
            bail!("no other agent is connected");
        }

        inner.next_message += 1;
        let message = json!({
            "id": inner.next_message,
            "from": { "id": from, "name": from_name,
                      "cwd": from_cwd.map(|p| p.display().to_string()) },
            "sent_at": rfc3339(unix_now()),
            "broadcast": to == "all",
            "text": text,
        });
        for id in &recipients {
            let agent = inner.agents.get_mut(id).expect("checked above");
            if agent.inbox.len() >= INBOX_LIMIT {
                agent.inbox.pop_front();
            }
            agent.inbox.push_back(message.clone());
            // Stores a permit if nobody waits yet, so the next wait returns.
            agent.notify.notify_one();
            self.save(agent);
        }
        Ok(recipients)
    }

    /// Take `id`'s unread messages, plus a handle to wait for more.
    pub fn take_inbox(&self, id: u32) -> Result<(Vec<Value>, Arc<Notify>)> {
        let mut inner = self.inner.lock().unwrap();
        let agent = inner
            .agents
            .get_mut(&id)
            .ok_or_else(|| anyhow!("not registered"))?;
        let messages: Vec<Value> = agent.inbox.drain(..).collect();
        if !messages.is_empty() {
            self.save(agent);
        }
        Ok((messages, agent.notify.clone()))
    }

    fn load(&self, key: &str) -> Option<Saved> {
        let path = self.dir.as_ref()?.join(format!("{key}.json"));
        serde_json::from_slice(&fs::read(path).ok()?).ok()
    }

    /// Save `agent`'s state (0600, atomically). Best effort: a failure only
    /// means a reset after the next daemon restart.
    fn save(&self, agent: &Agent) {
        let (Some(dir), Some(key)) = (&self.dir, &agent.key) else {
            return;
        };
        let write = || -> std::io::Result<()> {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(dir)?;
            let tmp = dir.join(format!(".{key}.tmp"));
            let mut file = fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            file.write_all(&serde_json::to_vec(&agent.saved())?)?;
            fs::rename(&tmp, dir.join(format!("{key}.json")))
        };
        if let Err(e) = write() {
            tracing::warn!(agent = agent.id, error = %e, "saving session state failed");
        }
    }
}

/// `<pid>-<start time>` of a live process: its start time (in clock ticks
/// since boot, from `/proc/<pid>/stat`) tells a reused pid apart.
fn process_key(pid: u32) -> Option<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Fields after the parenthesized command name; start time is field 22.
    let start = stat.rsplit_once(") ")?.1.split_whitespace().nth(19)?;
    Some(format!("{pid}-{start}"))
}

fn clip(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(pid: u32, cwd: &str) -> ClientHello {
        ClientHello {
            carl: 1,
            version: "0".into(),
            pid,
            ppid: Some(pid - 1),
            cwd: Some(cwd.into()),
            areas: None,
        }
    }

    #[test]
    fn register_describe_and_address() {
        let agents = Agents::default();
        let (a, _) = agents.register(&hello(10, "/src/carl"));
        let (b, _) = agents.register(&hello(20, "/src/web"));
        agents.set_client(a, "claude-code", "2.1");
        assert_eq!(agents.whoami(a).unwrap()["name"], "claude-code@carl");

        agents
            .describe(b, "fixing the login page", Some("web-fixer"))
            .unwrap();
        assert!(
            agents.describe(a, "x", Some("web-fixer")).is_err(),
            "names are unique"
        );
        assert!(agents.describe(a, "x", Some("has space")).is_err());

        let list = agents.list(a);
        assert_eq!(list["agents"].as_array().unwrap().len(), 2);
        let web = &list["agents"][1];
        assert_eq!(web["task"], "fixing the login page");
        assert_eq!(web["cwd"], "/src/web");
        assert_eq!(web["you"], false);

        assert_eq!(agents.send(a, "web-fixer", "hi").unwrap(), [b]);
        assert_eq!(agents.send(a, "20", "again").unwrap(), [b]);
        assert!(agents.send(a, "10", "me").is_err());
        assert!(agents.send(a, "nobody", "x").is_err());
        assert!(agents.send(a, "web-fixer", " ").is_err());

        let (messages, _) = agents.take_inbox(b).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["from"]["name"], "claude-code@carl");
        assert_eq!(messages[0]["from"]["cwd"], "/src/carl");
        assert!(agents.take_inbox(b).unwrap().0.is_empty());
    }

    #[test]
    fn broadcast_reaches_everyone_else() {
        let agents = Agents::default();
        let (a, _) = agents.register(&hello(10, "/a"));
        assert!(agents.send(a, "all", "anyone?").is_err());
        let (b, _) = agents.register(&hello(20, "/b"));
        let (c, _) = agents.register(&hello(30, "/c"));
        let mut to = agents.send(a, "all", "heads up").unwrap();
        to.sort();
        assert_eq!(to, [b, c]);
        assert_eq!(agents.take_inbox(c).unwrap().0[0]["broadcast"], true);
        assert!(agents.take_inbox(a).unwrap().0.is_empty());
        agents.unregister(b);
        assert_eq!(agents.list(a)["agents"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn session_state_survives_a_daemon_restart() {
        let dir = std::env::temp_dir().join(format!("carl-sessions-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        // A real live pid, so the key can be computed from /proc.
        let me = hello(std::process::id(), "/src/carl");

        let old = Agents::new(Some(dir.clone()));
        let (id, restored) = old.register(&me);
        assert!(restored.is_none());
        old.describe(id, "shipping 0.1.3", Some("shipper")).unwrap();
        old.set_areas(id, vec!["agents".into(), "google.mail".into()]);
        let (other, _) = old.register(&hello(1, "/elsewhere"));
        old.send(other, "shipper", "ping me when done").unwrap();

        // The daemon exits for an update: state is kept.
        old.shutting_down();
        old.unregister(id);

        let new = Agents::new(Some(dir.clone()));
        let (id, restored) = new.register(&me);
        assert_eq!(restored.unwrap(), ["agents", "google.mail"]);
        new.set_client(id, "claude-code", "2");
        let whoami = new.whoami(id).unwrap();
        assert_eq!(whoami["name"], "shipper");
        assert_eq!(whoami["task"], "shipping 0.1.3");
        assert_eq!(whoami["unread"], 1);

        // A clean disconnect forgets it.
        new.unregister(id);
        let fresh = Agents::new(Some(dir.clone()));
        assert!(fresh.register(&me).1.is_none());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn process_keys_tell_reused_pids_apart() {
        let key = process_key(std::process::id()).unwrap();
        assert!(
            key.starts_with(&format!("{}-", std::process::id())),
            "{key}"
        );
        assert_eq!(process_key(std::process::id()), Some(key));
        assert!(process_key(u32::MAX).is_none());
    }
}
