# Carl

**Carl gives AI agents hands.**

Carl is a multipurpose [Model Context Protocol](https://modelcontextprotocol.io)
(MCP) server: a local helper process that an AI agent (such as Claude) connects
to in order to *do* things rather than merely talk about them.

Where an agent can reason and converse, Carl lets it act: hold and move value, create and manage email, and
more as the project grows.

> **Status: early scaffold.** The server runs, speaks MCP over stdio, and
> exposes a working `system` feature. The `wallet` and `email` features are
> scaffolded — their tools are discoverable and fully typed, but the handlers
> currently report "not implemented." See the [roadmap](#roadmap).

## How it works

Carl runs as a local subprocess of the agent's MCP client and communicates over
**stdio** using JSON-RPC. stdout carries the protocol; all logs go to stderr.
Because it runs locally and holds its own state (eventually including key
material), Carl is the trust boundary between the agent and the real world.

```
┌──────────────┐   stdio / JSON-RPC   ┌──────────────┐   APIs / chains
│  AI agent    │ ───────────────────► │     Carl     │ ─────────────────►  …
│ (MCP client) │ ◄─────────────────── │ (MCP server) │
└──────────────┘                      └──────────────┘
```

## Features

| Area     | Tools                                                   | Status      |
| -------- | ------------------------------------------------------- | ----------- |
| `system` | `carl_status`, `carl_ping`                              | ✅ available |
| `wallet` | `wallet_balance`, `wallet_address`, `wallet_send`       | 🚧 scaffold |
| `email`  | `email_create`, `email_list`, `email_send`              | 🚧 scaffold |

Call **`carl_status`** first — it reports the version and which feature areas are
live.

## Build & run

Requires a recent Rust toolchain (edition 2024; tested with 1.96).

```sh
cargo build --release
./target/release/carl
```

Run it directly only to smoke-test — normally the agent's MCP client launches it.

The `carl` the client launches is a thin shim. The first one starts a background
`carl daemon` that serves every agent on the machine, and the daemon exits by
itself about a minute after the last agent disconnects. Its log is
`~/.local/state/carl/daemon.log`. `carl standalone` serves in-process without a
daemon. See [`ARCHITECTURE.md`](ARCHITECTURE.md#process-model).

### Smoke test

```sh
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"carl_status","arguments":{}}}' \
  | ./target/release/carl
```

## Connecting an MCP client

Point any MCP client at the built binary as a stdio server. Example
(`claude_desktop_config.json` / Claude Code `mcp` config):

```json
{
  "mcpServers": {
    "carl": {
      "command": "/absolute/path/to/carl/target/release/carl",
      "env": { "RUST_LOG": "info" }
    }
  }
}
```

With Claude Code:

```sh
claude mcp add carl -- /absolute/path/to/carl/target/release/carl
```

## Configuration

Carl reads its configuration from the environment:

| Variable        | Default               | Purpose                                      |
| --------------- | --------------------- | -------------------------------------------- |
| `CARL_DATA_DIR` | `$XDG_DATA_HOME/carl` (`~/.local/share/carl`) | Where Carl persists state (keystore). When set, the daemon log goes here too |
| `CARL_IDLE_TIMEOUT` | `60`              | Seconds the daemon lingers with no agent connected |
| `CARL_SOCKET`   | `/tmp/carl-<uid>/<hash>.sock` | Daemon socket (one per data dir) |
| `CARL_NO_UPDATE` | unset                | Set to disable self-update (official builds only) |
| `RUST_LOG`      | `info`                | Log filter (logs go to **stderr**; the daemon's to `$XDG_STATE_HOME/carl/daemon.log`, i.e. `~/.local/state/carl`) |

## Roadmap

- **Wallet** — key management and a chain-agnostic balance/receive/send surface,
  behind an explicit, auditable spend-authorization policy. See
  [`docs/wallet.md`](docs/wallet.md).
- **Email** — create and manage email identities and send/receive mail, backed
  by Karpelès Lab email APIs. See [`docs/email.md`](docs/email.md).
- **More** — additional capabilities as the project grows.

## Project layout

See [`ARCHITECTURE.md`](ARCHITECTURE.md) for the design and a step-by-step recipe
for adding a feature.

## License

MIT © Karpelès Lab Inc. See [`LICENSE`](LICENSE).
