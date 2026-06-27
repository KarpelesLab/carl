# Manu

**Manu gives AI agents hands.**

Manu is a multipurpose [Model Context Protocol](https://modelcontextprotocol.io)
(MCP) server: a local helper process that an AI agent (such as Claude) connects
to in order to *do* things rather than merely talk about them.

The name comes from the Latin *manus* — "hand." Where an agent can reason and
converse, Manu lets it act: hold and move value, create and manage email, and
more as the project grows.

> **Status: early scaffold.** The server runs, speaks MCP over stdio, and
> exposes a working `system` feature. The `wallet` and `email` features are
> scaffolded — their tools are discoverable and fully typed, but the handlers
> currently report "not implemented." See the [roadmap](#roadmap).

## How it works

Manu runs as a local subprocess of the agent's MCP client and communicates over
**stdio** using JSON-RPC. stdout carries the protocol; all logs go to stderr.
Because it runs locally and holds its own state (eventually including key
material), Manu is the trust boundary between the agent and the real world.

```
┌──────────────┐   stdio / JSON-RPC   ┌──────────────┐   APIs / chains
│  AI agent    │ ───────────────────► │     Manu     │ ─────────────────►  …
│ (MCP client) │ ◄─────────────────── │ (MCP server) │
└──────────────┘                      └──────────────┘
```

## Features

| Area     | Tools                                                   | Status      |
| -------- | ------------------------------------------------------- | ----------- |
| `system` | `manu_status`, `manu_ping`                              | ✅ available |
| `wallet` | `wallet_balance`, `wallet_address`, `wallet_send`       | 🚧 scaffold |
| `email`  | `email_create`, `email_list`, `email_send`              | 🚧 scaffold |

Call **`manu_status`** first — it reports the version and which feature areas are
live.

## Build & run

Requires a recent Rust toolchain (edition 2024; tested with 1.96).

```sh
cargo build --release
./target/release/manu
```

Run it directly only to smoke-test — normally the agent's MCP client launches it.

### Smoke test

```sh
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}' \
  '{"jsonrpc":"2.0","method":"notifications/initialized"}' \
  '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"manu_status","arguments":{}}}' \
  | ./target/release/manu
```

## Connecting an MCP client

Point any MCP client at the built binary as a stdio server. Example
(`claude_desktop_config.json` / Claude Code `mcp` config):

```json
{
  "mcpServers": {
    "manu": {
      "command": "/absolute/path/to/manu/target/release/manu",
      "env": { "RUST_LOG": "info" }
    }
  }
}
```

With Claude Code:

```sh
claude mcp add manu -- /absolute/path/to/manu/target/release/manu
```

## Configuration

Manu reads its configuration from the environment:

| Variable        | Default               | Purpose                                      |
| --------------- | --------------------- | -------------------------------------------- |
| `MANU_DATA_DIR` | `~/.manu`             | Where Manu persists state (keystore, caches) |
| `RUST_LOG`      | `info`                | Log filter (logs go to **stderr**)           |

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
