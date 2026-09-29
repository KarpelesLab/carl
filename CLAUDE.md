# CLAUDE.md

Guidance for Claude Code (and other agents) working in this repository.

## What this is

Carl is a multipurpose MCP **server** (stdio, JSON-RPC) that gives an AI agent
"hands" — real actions like holding/moving crypto and managing email. Built on
the official Rust SDK, `rmcp`. The `carl` an MCP client launches is a shim; one
per-user `carl daemon` runs the server for every agent on the machine.

Read [`ARCHITECTURE.md`](ARCHITECTURE.md) before making structural changes.

## Build / run / check

```sh
cargo build              # debug build
cargo clippy             # lint — keep it clean
cargo test               # unit tests
./target/debug/carl      # run the shim (reads CARL_DATA_DIR, CARL_SOCKET, CARL_IDLE_TIMEOUT, RUST_LOG)
./target/debug/carl standalone   # in-process server, no daemon
```

Quick MCP smoke test (handshake + a tool call) is in `README.md`.

## Conventions

- **Never write to stdout.** It is the MCP transport. All logging goes through
  `tracing` to stderr.
- **One feature per file** under `src/features/`, each contributing a named tool
  router (`#[tool_router(router = <name>_router, vis = "pub(crate)")]`) that is
  summed into the composed router in `src/server.rs` (`Carl::new`). The recipe
  for adding a feature is in `ARCHITECTURE.md`.
- **Tool naming:** prefix with the feature area — `wallet_*`, `email_*`, `agent_*`,
  `carl_*` for system tools. Set an explicit `#[tool(name = "…", description =
  "…")]`; descriptions are read by the agent, so make them actionable.
- **Areas:** every non-`carl_*` tool belongs to an area (`src/areas.rs`, by
  name prefix); sessions only see enabled areas. A new feature needs a prefix
  in `area_of` and an entry in `AREAS`.
- **Arguments:** use `Parameters<T>` with `T: Deserialize + schemars::JsonSchema`.
  Field doc-comments become the JSON Schema descriptions the agent sees.
- **Unimplemented handlers** return `error::not_implemented("<tool>")` — keep the
  tool discoverable and typed rather than removing it.
- `Carl` is `Clone` (cloned per request); hold all state behind `Arc`. The
  daemon shares one `Carl` across **all** agents' sessions, so state is global
  unless keyed by session.
- The shim ⇄ daemon hello (`src/ipc.rs`) must stay backward compatible: after an
  update, old shims talk to the new daemon. Add fields only as
  `#[serde(default)]`.
- Code must also build for fullrust (static, libc-free): use `std`/`rustix`,
  never `libc` or C dependencies.

## Releases

Push a `v*` tag (after bumping the version in `Cargo.toml`): CI builds the
fullrust binary with `--features auto-update`, signs it (`RSUPD_IDENTITY`
secret), and publishes it via rsupd; running daemons pick it up within the
hour. Never change the fingerprint in `rsupd_updater()` (`src/main.rs`)
unless rotating the signing key on purpose — it is what installed copies trust.

## Security posture

Carl is the trust boundary between the model and the real world. Sensitive
actions (spending funds, sending mail) must be authorized and audited **inside**
Carl — never blindly executed because the model asked. Secrets live under
`CARL_DATA_DIR` and must never be written to stdout, logs, or tool output. When
implementing the wallet, treat key material as the highest-sensitivity asset in
the codebase (see `docs/wallet.md`).

## Current state

- `system` — implemented (`carl_status`, `carl_ping`).
- `agents` — implemented: agents on the machine describe themselves, list
  each other, and exchange messages (`agent_*`, daemon only).
- `google` — implemented: account linking, Gmail/Calendar/Drive/Contacts
  read, and writes that only affect the user. See `docs/google.md`. HTTP goes
  through rsurl; crypto helpers come from purecrypto (no extra crates).
- `wallet`, `email` — scaffolded; tool surfaces exist, handlers return
  not-implemented. Design notes in `docs/`.

## Git

Remote is `origin` (`github.com:KarpelesLab/carl`). Commit or push only when
asked. The default branch is `master`.
