# CLAUDE.md

Guidance for Claude Code (and other agents) working in this repository.

## What this is

Carl is a multipurpose MCP **server** (stdio, JSON-RPC) that gives an AI agent
"hands" — real actions like holding/moving crypto and managing email. Built on
the official Rust SDK, `rmcp`.

Read [`ARCHITECTURE.md`](ARCHITECTURE.md) before making structural changes.

## Build / run / check

```sh
cargo build              # debug build
cargo clippy             # lint — keep it clean
cargo test               # unit tests
./target/debug/carl      # run (reads CARL_DATA_DIR, RUST_LOG from env)
```

Quick MCP smoke test (handshake + a tool call) is in `README.md`.

## Conventions

- **Never write to stdout.** It is the MCP transport. All logging goes through
  `tracing` to stderr.
- **One feature per file** under `src/features/`, each contributing a named tool
  router (`#[tool_router(router = <name>_router, vis = "pub(crate)")]`) that is
  summed into the composed router in `src/server.rs` (`Carl::new`). The recipe
  for adding a feature is in `ARCHITECTURE.md`.
- **Tool naming:** prefix with the feature area — `wallet_*`, `email_*`,
  `carl_*` for system tools. Set an explicit `#[tool(name = "…", description =
  "…")]`; descriptions are read by the agent, so make them actionable.
- **Arguments:** use `Parameters<T>` with `T: Deserialize + schemars::JsonSchema`.
  Field doc-comments become the JSON Schema descriptions the agent sees.
- **Unimplemented handlers** return `error::not_implemented("<tool>")` — keep the
  tool discoverable and typed rather than removing it.
- `Carl` is `Clone` (cloned per request); hold all state behind `Arc`.

## Security posture

Carl is the trust boundary between the model and the real world. Sensitive
actions (spending funds, sending mail) must be authorized and audited **inside**
Carl — never blindly executed because the model asked. Secrets live under
`CARL_DATA_DIR` and must never be written to stdout, logs, or tool output. When
implementing the wallet, treat key material as the highest-sensitivity asset in
the codebase (see `docs/wallet.md`).

## Current state

- `system` — implemented (`carl_status`, `carl_ping`).
- `wallet`, `email` — scaffolded; tool surfaces exist, handlers return
  not-implemented. Design notes in `docs/`.

## Git

Remote is `origin` (`github.com:KarpelesLab/carl`). Commit or push only when
asked. Default branch for PRs is `main`.
