# Carl

**Carl gives AI agents hands.**

Carl is an [MCP](https://modelcontextprotocol.io) server that runs on your
machine and lets AI agents (Claude Code, Codex, Claude Desktop, anything that
speaks MCP) act in the real world: work with your Google account, coordinate
with each other, and soon hold a wallet and manage email. It is also the trust
boundary between the agents and those things: Carl decides what an agent may
do, not the agent.

## Getting started

### 1. Install

On Linux x86_64:

```sh
curl -fsSL https://raw.githubusercontent.com/KarpelesLab/carl/master/install.sh | sh
```

This downloads the latest release (a single static binary that runs on any
x86_64 Linux), checks its SHA-256, and puts it in `~/.local/bin/carl`. Carl
then **keeps itself up to date**: new releases are signed, checked against a
key built into Carl, and installed in the background. Keep the binary
somewhere you can write to (like `~/.local/bin`) for that to work.

Prefer to do it by hand? Download `carl-linux-x86_64` from the
[latest release](https://github.com/KarpelesLab/carl/releases/latest),
`chmod +x` it, and put it anywhere. On other platforms, build from source:
`cargo install --git https://github.com/KarpelesLab/carl` (no auto-update).

Check it: `~/.local/bin/carl --version`.

### 2. Add it to your agent

**Claude Code** (`--scope user` makes Carl available in every project):

```sh
claude mcp add --scope user carl -- ~/.local/bin/carl
```

**Codex:**

```sh
codex mcp add carl -- ~/.local/bin/carl
```

**Claude Desktop, or any other MCP client:** add a stdio server whose command
is the full path to the binary, e.g. in `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "carl": { "command": "/home/you/.local/bin/carl" }
  }
}
```

Use the same binary for every client: they all end up sharing one Carl (see
[how it works](#how-it-works)).

Carl starts each session with a small set of tools and lets the agent turn on
more areas as needed (see [tool areas](#tool-areas)). That relies on the client
refreshing its tool list when told to, which Claude Code does. If your client
doesn't (new tools never appear after `carl_enable`), preset the areas instead,
e.g. `codex mcp add carl --env CARL_AREAS=all -- ~/.local/bin/carl`.

### 3. Try it

Start a new session and ask your agent something like *"What can Carl do?"*
(it calls `carl_status`) or *"Which other agents are running?"* (`agent_list`).
In Claude Code, `/mcp` shows whether Carl is connected.

### 4. Link your Google account (optional)

Carl can search and read your Gmail, Calendar, Drive (Docs, Sheets, Slides)
and Contacts, and create things only you see: email **drafts** (never sent),
events without guests, and private files. Sending, inviting and sharing come
later, with approvals.

For now you bring your own Google OAuth client, once:

1. In the [Google Cloud console](https://console.cloud.google.com/), create a
   project and enable the **Gmail**, **Google Calendar**, **Google Drive** and
   **People** APIs.
2. Set up the OAuth consent screen (External) and **publish it** ("In
   production"); otherwise Google expires your link every 7 days.
3. Credentials → Create credentials → OAuth client ID → **Desktop app**.
   Download its JSON.
4. Tell your agent *"link my Google account to Carl, the client JSON is in
   ~/Downloads"*. It will give you a link: open it, approve (Google warns that
   the app is unverified, because it's your own; continue anyway), and you're
   done. Delete the JSON from Downloads afterwards; Carl keeps its own copy.

Every agent using Carl can then use the account. Details:
[`docs/google.md`](docs/google.md).

## What agents can do

### Tool areas

Carl's tools are grouped into areas, and a session only sees the areas it
enabled, so agents aren't handed dozens of tools they don't need. The
`carl_*` tools are always there: `carl_status` lists every area and its tools,
and `carl_enable` / `carl_disable` turn areas on and off for that session
only.

| Area              | Tools                                                           | Default |
| ----------------- | --------------------------------------------------------------- | ------- |
| `agents`          | `agent_describe`, `agent_whoami`, `agent_list`, `agent_send`, `agent_inbox` | on |
| `google.mail`     | `google_mail_search`, `_read`, `_labels`, `_draft`, `_subscribe`, `_unsubscribe` | off |
| `google.calendar` | `google_calendar_list`, `_events`, `_freebusy`, `_create_event` | off     |
| `google.drive`    | `google_drive_search`, `_read`, `_create`, `_update`            | off     |
| `google.contacts` | `google_contacts_search`                                        | off     |
| `google`          | all of the above, plus account tools (`google_link`, …), which come with any `google.*` area | off |
| `wallet`, `email` | not available yet                                               | —       |

`CARL_AREAS` (comma-separated, or `all`) sets which areas a session starts
with, per client: `claude mcp add -e CARL_AREAS=agents,google.mail …`.

A session can also subscribe to new mail (`google_mail_subscribe`), e.g. in an
address dedicated to Carl. New mail lands in `agent_inbox`, and a Claude Code
session started with
`claude --dangerously-load-development-channels server:carl` is woken by it
directly (Claude Code's [channels](https://code.claude.com/docs/en/channels)
research preview). See [`docs/google.md`](docs/google.md#new-mail-subscriptions).

**Agents** lets every agent on the machine, whichever client it runs in, say
what it's working on, see the others (and the directory each started in), and
message them, e.g. to avoid two agents editing the same files. Messages from
other agents are never treated as instructions from you.

## How it works

The `carl` your client starts is a thin relay. The first one launches a
background `carl daemon` that serves every agent on the machine, so they share
one set of linked accounts and can see each other. The daemon exits by itself
a minute after the last agent disconnects, and restarts transparently when a
new version is installed.

```
 Claude Code ─stdio─ carl ─┐
 Codex       ─stdio─ carl ─┼─ carl daemon ── Google, …
 Claude Code ─stdio─ carl ─┘
```

Your data stays on your machine: linked accounts live in
`~/.local/share/carl` (private to your user), and nothing is sent to Carl's
authors. The daemon logs to `~/.local/state/carl/daemon.log`.

See [`ARCHITECTURE.md`](ARCHITECTURE.md) for the design.

## Configuration

Everything has sensible defaults; these environment variables override them
(set them in your MCP client's server config, e.g. `claude mcp add -e`).

| Variable            | Default                          | Purpose                                            |
| ------------------- | -------------------------------- | -------------------------------------------------- |
| `CARL_DATA_DIR`     | `~/.local/share/carl`            | Linked accounts and other state. When set, the daemon log goes here too |
| `CARL_IDLE_TIMEOUT` | `60`                             | Seconds the daemon lingers with no agent connected |
| `CARL_SOCKET`       | `/tmp/carl-<uid>/<hash>.sock`    | Daemon socket (one per data dir)                   |
| `CARL_AREAS`        | `agents`                         | Tool areas a session starts with (`all` for every one) |
| `CARL_NO_UPDATE`    | unset                            | Set to disable self-update                         |
| `RUST_LOG`          | `info`                           | Log level                                          |

## Uninstall

Remove it from your clients (`claude mcp remove --scope user carl`,
`codex mcp remove carl`), then delete `~/.local/bin/carl`,
`~/.local/share/carl` and `~/.local/state/carl`. To revoke Google access too,
ask an agent to run `google_unlink` first, or remove Carl's app under your
Google Account's security settings.

## Development

Requires Rust (edition 2024).

```sh
cargo build
cargo test
./target/debug/carl standalone   # serve MCP in-process, without the daemon
```

Smoke test:

```sh
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"carl_status","arguments":{}}}' \
  | ./target/debug/carl standalone
```

Local builds never update themselves; release builds come from CI (push a
`v*` tag). See [`CLAUDE.md`](CLAUDE.md) and [`ARCHITECTURE.md`](ARCHITECTURE.md).

## Roadmap

- **Approvals** — an authorization layer so agents can send mail, invite and
  share, with you in the loop.
- **Wallet** — key management and a chain-agnostic balance/receive/send
  surface behind an auditable spend policy. See [`docs/wallet.md`](docs/wallet.md).
- **Email** — Carl-managed email identities. See [`docs/email.md`](docs/email.md).

## License

MIT © Karpelès Lab Inc. See [`LICENSE`](LICENSE).
