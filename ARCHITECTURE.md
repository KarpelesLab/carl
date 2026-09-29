# Architecture

Carl is an MCP **server** built on the official Rust SDK
([`rmcp`](https://crates.io/crates/rmcp)). An MCP client (the AI agent) launches
`carl` as a subprocess and speaks JSON-RPC to it over **stdio**, but that process
is only a shim: the server itself runs in a single per-user **daemon** shared by
every agent on the machine (Claude, Codex, or anything else that speaks MCP).

## Principles

1. **One server, many features.** A single [`ServerHandler`] (`Carl`) backs the
   whole process. Each capability area is a *feature module* that contributes a
   set of tools. Features are independent and additive.
2. **Honest tool surface.** A tool that exists is discoverable and fully typed,
   even before its logic lands; unfinished handlers return a clear
   "not implemented" error rather than being hidden. Agents can plan against the
   shape of the API and find out precisely what's missing.
3. **Carl is the trust boundary.** The agent proposes; Carl disposes. Sensitive
   actions (spending funds, sending mail) are authorized and audited inside
   Carl, never delegated blindly to the model. Secrets live in `CARL_DATA_DIR`,
   never in the conversation.
4. **stdout is sacred.** The MCP wire protocol owns stdout. All diagnostics go to
   stderr via `tracing`.

## Process model

```
 agent A ──stdio── carl (shim) ─┐
 agent B ──stdio── carl (shim) ─┼── /tmp/carl-<uid>/<hash>.sock ── carl daemon
 agent C ──stdio── carl (shim) ─┘                                 └─ one Carl, one
                                                                    rmcp session per shim
```

`carl` has three modes (`src/main.rs`):

| command           | role                                                            |
| ----------------- | --------------------------------------------------------------- |
| `carl`            | **Shim** (`shim.rs`), what MCP clients launch. Stateless relay. |
| `carl daemon`     | **Daemon** (`daemon.rs`), which actually runs Carl.             |
| `carl standalone` | Serves MCP over stdio in-process, with no daemon (debugging).   |

Having a single runner is what makes shared state possible: one wallet (so two
agents never race on nonces or the keystore), and one place where agents can
find and message each other.

### Shim

1. Connects to the daemon socket. If nobody is listening, it spawns `carl daemon` in
   the background (stderr to `daemon.log` under `$XDG_STATE_HOME/carl`) and
   retries for up to 10s.
2. Exchanges a one-line JSON hello (`ipc.rs`): the shim sends its protocol
   version, pid, parent pid (the agent), and cwd; the daemon answers with its
   version and pid.
3. Relays newline-delimited JSON-RPC both ways, byte for byte, while tracking
   the client's `initialize` / `notifications/initialized` and unanswered
   request ids.
4. **If the daemon goes away** (an update, a crash), it reconnects, starting a
   new daemon if needed, and replays `initialize` + `initialized`, swallowing
   the duplicate reply. Requests that were in flight get a JSON-RPC error
   telling the agent to retry. It then sends the client
   `notifications/tools/list_changed`, since the new daemon may expose other
   tools. The client never sees the session drop.
5. Exits when the client closes stdin.

### Daemon

- **Detaches** with `setsid`, so a Ctrl-C or closed terminal in the agent that
  happened to start it doesn't take it down for everyone.
- **Single instance:** holds an exclusive lock on `daemon.lock` in the data dir
  (the file contains its pid) for its whole life. When several shims start at
  once, every daemon but one fails the lock and exits. Only the lock holder
  binds the socket, so a leftover socket file is always stale and safe to
  replace. The lock lives next to the keystore it protects, so whatever
  environment agents run in, one keystore never gets two daemons.
- **Socket:** `/tmp/carl-<uid>/<hash of data dir>.sock` (or `CARL_SOCKET`),
  tmux-style. It is the same path for every agent whatever its `XDG_*`/`TMPDIR`
  settings, short enough for the 108-byte socket path limit, and on tmpfs.
- **Health check** every `CARL_HEALTH_INTERVAL` (default 30s). If the lock
  file was deleted or replaced, the daemon re-takes the lock on the new file,
  or exits if another daemon already holds it (its shims then reconnect to
  that one). If the socket was deleted (a `/tmp` cleaner, say), it rebinds.
  Until that check runs, new shims can't reach it. They keep retrying for up
  to twice the interval, starting daemons that lose the lock and exit.
- **One connection = one rmcp session.** Each accepted socket runs on its own
  threads, with blocking socket I/O bridged into the tokio runtime through an
  in-memory duplex pipe.
- **Idle exit:** once no shim has been connected for `CARL_IDLE_TIMEOUT`
  (default 60s), it removes the socket and exits. The grace period absorbs an
  agent restarting. Nothing polls: the main thread sleeps until a session
  closes, the idle deadline passes, or the next health check is due.

### Agents

Every shim connection is an **agent** in the daemon's registry (`agents.rs`),
whatever MCP client it comes from (Claude Code, Codex, …):

- **Identity:** the id is the shim's pid, so it survives the shim
  reconnecting after a daemon restart. The registry records the directory the
  agent was started in and the agent's pid (from the hello), plus the MCP
  client's name and version (from `initialize`). The default name is
  `<client>@<directory>`.
- **Self-description:** `agent_describe` sets what the agent is working on and
  optionally a unique name. The server instructions ask agents to call it when
  they start a task.
- **Messaging:** `agent_send` queues a message for an id, a name, or `all`.
  `agent_inbox` drains the caller's inbox and can long-poll up to 120s. MCP
  can't push into a model's context, so receiving is always a tool call.
- **Trust:** messages are returned wrapped as untrusted. Another agent is not
  the user, and a message never authorizes anything.
- The session's `Carl` knows its agent id (`Carl::for_session`); in
  `carl standalone` there is no registry and the agent tools say so.
- **Resuming:** what a session set up (enabled areas, chosen name, task,
  unread messages) is saved to `<data dir>/sessions/<pid>-<start>.json`
  (0600), keyed by the shim's pid and process start time (from
  `/proc/<pid>/stat`, so a reused pid can't inherit it). When the shim
  reconnects to a new daemon, the session is restored. A clean disconnect
  deletes the file; a daemon exiting (update, crash) leaves it; a starting
  daemon prunes files of shims that are gone.

### Tool areas

Every tool belongs to an area (`areas.rs`, by name prefix): `agents`,
`google`, `google.mail`, `google.calendar`, `google.drive`,
`google.contacts`, `wallet`, `email`. `carl_*` tools belong to none and are
always visible.

- Each session has its own set of enabled areas (`Carl.areas`, created in
  `Carl::for_session`), starting from the shim's `CARL_AREAS` (sent in the
  hello) or the default, `agents`.
- `ServerHandler::{list_tools, call_tool, get_tool}` are written by hand in
  `server.rs` (instead of by `#[tool_handler]`) to filter by those areas. A
  hidden tool can't be called either.
- `carl_enable` / `carl_disable` change the set and send
  `notifications/tools/list_changed` (advertised in the capabilities), and
  the client refetches `tools/list`. Claude Code does. For clients that
  don't, `CARL_AREAS` presets the areas.
- Scaffolded areas (`available: false`) can't be enabled, so their
  not-implemented tools stay out of sight.

### Updates

Releases are static, libc-free x86_64 Linux binaries built with
[fullrust](https://github.com/KarpelesLab/fullrust), signed and delivered by
[rsupd](https://github.com/KarpelesLab/rsupd) (`src/update.rs`).

- **Releasing:** push a `v*` tag. `.github/workflows/build.yml` tests, builds
  with fullrust, signs the binary with the `RSUPD_IDENTITY` secret, and
  uploads it on the `master` channel. The fingerprint of that key is compiled
  into Carl (`rsupd_updater()` in `main.rs`) and is the only thing an update
  is trusted by. The private key lives in `~/.config/rsupd/carl/` and in that
  secret, nowhere else.
- **Official builds only:** the updater is behind the `auto-update` cargo
  feature, which only CI enables, so a local build never replaces itself.
  `CARL_NO_UPDATE=1` disables it at runtime.
- **Only the daemon runs the updater.** A shim restarting would look like the
  server dying to its MCP client, and ten shims would race to swap one binary.
  It checks 60s after start, then hourly.
- **Restart by exit:** once rsupd has verified and swapped in the new binary,
  the daemon simply exits (removing its socket). Its shims reconnect, start the
  new binary from the path they were launched from (captured at startup,
  because the old file is renamed away and deleted), and replay (above), so
  agents carry on. If no shim is connected, nothing needs restarting.
- Running shims keep the old code until their agent exits. That is why the
  hello is versioned and only gains optional fields: **a daemon must accept
  hellos from older shims.**
- Daemon memory does not survive an update. Anything that must (queued
  messages, pending approvals) has to be persisted in `CARL_DATA_DIR`.
- The binary must be writable by the user running it (e.g. `~/.local/bin`),
  or the update fails and is retried hourly (logged in `daemon.log`).

### Local security

- The data dir (`$XDG_DATA_HOME/carl`, or `CARL_DATA_DIR`) holds the keystore
  and the lock. It is deliberately **not** a cache directory
  (those get wiped), and the lock sits next to the keystore it protects rather
  than in `$XDG_RUNTIME_DIR`, which not every agent's environment has set.
- The data and log dirs are created 0700 and must be owned by the current user.
  The socket, lock, and log are 0600.
- `/tmp` is shared, and another user could create `/tmp/carl-<uid>` first. The
  daemon refuses a socket dir that is a symlink, isn't ours, or is writable by
  anyone else. The worst a squatter can do is keep Carl from starting (with a
  clear error in `daemon.log`), never intercept it.
- Both ends check the peer's uid (`SO_PEERCRED`): the daemon so no other user
  can drive it, the shim so it never hands an agent's traffic to an impostor.

## Source layout

```
src/
├── main.rs              Entry point: tracing, config, mode dispatch.
├── shim.rs              What MCP clients launch: stdio ⇄ daemon relay.
├── daemon.rs            The single per-user server process.
├── ipc.rs               Shim ⇄ daemon hello and peer checks.
├── config.rs            Config struct, resolved from the environment.
├── error.rs             Small McpError constructors (e.g. not_implemented).
├── server.rs            `Carl` ServerHandler; composes feature tool routers.
├── update.rs            rsupd self-update (daemon, official builds).
├── agents.rs            Registry of connected agents and their inboxes.
├── areas.rs             Tool areas: which tools a session exposes.
├── google/              Google plumbing: OAuth linking, token store, REST.
│   ├── mod.rs           `Google`: accounts, token refresh, loopback link flow.
│   ├── oauth.rs         Scopes per area, PKCE, token endpoint calls.
│   ├── store.rs         client.json / accounts.json under the data dir.
│   ├── mail.rs          Gmail MIME → text; RFC 5322 drafts.
│   └── encoding.rs      URL/base64/time helpers (purecrypto underneath).
└── features/
    ├── mod.rs           Declares the feature modules.
    ├── system.rs        Introspection tools (status, ping). Always available.
    ├── agents.rs        agent_* tools: describe, list, message other agents.
    ├── wallet.rs        Crypto wallet (scaffold).
    ├── email.rs         Email management (scaffold).
    └── google/          google_* tools, one router per area (mail, calendar,
                         drive, contacts; account tools in mod.rs).
```

## How features compose

`rmcp` generates a `ToolRouter<Carl>` from each `impl Carl` block annotated with
`#[tool_router(...)]`. `ToolRouter` implements `std::ops::Add`, so the routers
are merged into one in `Carl::new`:

```rust
tool_router: Self::system_router() + Self::wallet_router() + Self::email_router(),
```

Each feature lives in its own file and declares a **named** router so the blocks
don't collide:

```rust
#[tool_router(router = wallet_router, vis = "pub(crate)")]
impl Carl {
    #[tool(name = "wallet_balance", description = "…")]
    fn wallet_balance(&self, Parameters(args): Parameters<AssetArgs>)
        -> Result<CallToolResult, McpError> { … }
}
```

The composed router is stored in the `Carl.tool_router` field, which
`#[tool_handler(router = self.tool_router)]` on the `ServerHandler` impl uses to
dispatch every `tools/list` and `tools/call`.

## Adding a feature

1. Create `src/features/<name>.rs`.
2. Add `pub mod <name>;` to `src/features/mod.rs`.
3. Write `#[tool_router(router = <name>_router, vis = "pub(crate)")] impl Carl { … }`
   with one `#[tool]` method per action. Use `Parameters<T>` (where `T:
   Deserialize + schemars::JsonSchema`) for structured arguments.
4. Add `+ Self::<name>_router()` to the sum in `Carl::new`.
5. If the feature needs state, add an `Arc<…>` field to `Carl` and initialize it
   in `Carl::new` (the struct is cloned per request, so state must be shared).
6. Give it an area: add its tool prefix to `area_of` and an entry to `AREAS`
   in `areas.rs` (a unit test fails for tools in unknown areas), and update
   the table in `README.md`.

## State & configuration

`Carl` is `Clone` (rmcp clones it per request), so all mutable or owned state is
held behind `Arc`. The daemon builds **one** `Carl` and clones it into every
session, so that state is shared by **all agents** on the machine. Anything
per-agent must be keyed by session, and anything one agent sends another is
untrusted input. Configuration is centralized in `config.rs` and resolved once
at startup from the environment; each feature is expected to own its typed config
section there as it matures.

## Dependencies

- **rmcp** — MCP server, macros, and the stdio transport (`transport-io`).
- **tokio / tokio-util** — async runtime; `SyncIoBridge` for the socket bridge.
- **rsupd** (optional, `auto-update` feature) — signed self-update.
- **rsurl / purecrypto** — pure-Rust HTTP(S) client and crypto (SHA-256,
  randomness, base64url) for the Google feature; rsupd uses them too.
- **rustix** — typed syscalls (`setsid`, `getuid`, `SO_PEERCRED`) with no libc,
  so the same code builds for fullrust.
- **serde / serde_json / schemars** — tool argument (de)serialization and JSON
  Schema generation for tool inputs.
- **tracing / tracing-subscriber** — structured logging to stderr.
- **anyhow / thiserror** — error plumbing.
