//! Registry of the agents connected to the daemon, and their message inboxes.
//!
//! Each shim connection is one agent. Its id is the shim's pid, so it stays
//! the same when the shim reconnects after a daemon restart; what the agent
//! said about itself and its unread messages live in daemon memory and do not
//! survive a restart.

use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use anyhow::{Result, anyhow, bail};
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
}

#[derive(Default)]
struct Inner {
    agents: HashMap<u32, Agent>,
    next_message: u64,
}

struct Agent {
    id: u32,
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
    inbox: VecDeque<Value>,
    /// Wakes a pending `agent_inbox` wait.
    notify: Arc<Notify>,
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
}

impl Agents {
    /// Register a new connection; returns its agent id.
    pub fn register(&self, hello: &ClientHello) -> u32 {
        let dir = hello
            .cwd
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "agent".into());
        let agent = Agent {
            id: hello.pid,
            name: dir,
            named: false,
            client: None,
            pid: hello.ppid,
            cwd: hello.cwd.clone(),
            task: None,
            connected_at: unix_now(),
            task_updated_at: None,
            inbox: VecDeque::new(),
            notify: Arc::new(Notify::new()),
        };
        self.inner.lock().unwrap().agents.insert(hello.pid, agent);
        hello.pid
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

    pub fn unregister(&self, id: u32) {
        self.inner.lock().unwrap().agents.remove(&id);
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
        Ok((agent.inbox.drain(..).collect(), agent.notify.clone()))
    }
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
        }
    }

    #[test]
    fn register_describe_and_address() {
        let agents = Agents::default();
        let a = agents.register(&hello(10, "/src/carl"));
        let b = agents.register(&hello(20, "/src/web"));
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
        let a = agents.register(&hello(10, "/a"));
        assert!(agents.send(a, "all", "anyone?").is_err());
        let b = agents.register(&hello(20, "/b"));
        let c = agents.register(&hello(30, "/c"));
        let mut to = agents.send(a, "all", "heads up").unwrap();
        to.sort();
        assert_eq!(to, [b, c]);
        assert_eq!(agents.take_inbox(c).unwrap().0[0]["broadcast"], true);
        assert!(agents.take_inbox(a).unwrap().0.is_empty());
        agents.unregister(b);
        assert_eq!(agents.list(a)["agents"].as_array().unwrap().len(), 2);
    }
}
