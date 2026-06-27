# Architecture

Manu is a single-process MCP **server** built on the official Rust SDK
([`rmcp`](https://crates.io/crates/rmcp)). It speaks JSON-RPC over **stdio** and
is launched as a subprocess by an MCP client (the AI agent).

## Principles

1. **One server, many features.** A single [`ServerHandler`] (`Manu`) backs the
   whole process. Each capability area is a *feature module* that contributes a
   set of tools. Features are independent and additive.
2. **Honest tool surface.** A tool that exists is discoverable and fully typed,
   even before its logic lands; unfinished handlers return a clear
   "not implemented" error rather than being hidden. Agents can plan against the
   shape of the API and find out precisely what's missing.
3. **Manu is the trust boundary.** The agent proposes; Manu disposes. Sensitive
   actions (spending funds, sending mail) are authorized and audited inside
   Manu, never delegated blindly to the model. Secrets live in `MANU_DATA_DIR`,
   never in the conversation.
4. **stdout is sacred.** The MCP wire protocol owns stdout. All diagnostics go to
   stderr via `tracing`.

## Process flow

```
main.rs
  ├─ init tracing (stderr)
  ├─ Config::from_env()
  └─ Manu::new(config).serve(stdio()).await   ── rmcp drives the JSON-RPC loop
                         │
                         └─ dispatches tools/list, tools/call → composed ToolRouter
```

## Source layout

```
src/
├── main.rs              Entry point: tracing, config, serve over stdio.
├── config.rs            Config struct, resolved from the environment.
├── error.rs             Small McpError constructors (e.g. not_implemented).
├── server.rs            `Manu` ServerHandler; composes feature tool routers.
└── features/
    ├── mod.rs           Declares the feature modules.
    ├── system.rs        Introspection tools (status, ping). Always available.
    ├── wallet.rs        Crypto wallet (scaffold).
    └── email.rs         Email management (scaffold).
```

## How features compose

`rmcp` generates a `ToolRouter<Manu>` from each `impl Manu` block annotated with
`#[tool_router(...)]`. `ToolRouter` implements `std::ops::Add`, so the routers
are merged into one in `Manu::new`:

```rust
tool_router: Self::system_router() + Self::wallet_router() + Self::email_router(),
```

Each feature lives in its own file and declares a **named** router so the blocks
don't collide:

```rust
#[tool_router(router = wallet_router, vis = "pub(crate)")]
impl Manu {
    #[tool(name = "wallet_balance", description = "…")]
    fn wallet_balance(&self, Parameters(args): Parameters<AssetArgs>)
        -> Result<CallToolResult, McpError> { … }
}
```

The composed router is stored in the `Manu.tool_router` field, which
`#[tool_handler(router = self.tool_router)]` on the `ServerHandler` impl uses to
dispatch every `tools/list` and `tools/call`.

## Adding a feature

1. Create `src/features/<name>.rs`.
2. Add `pub mod <name>;` to `src/features/mod.rs`.
3. Write `#[tool_router(router = <name>_router, vis = "pub(crate)")] impl Manu { … }`
   with one `#[tool]` method per action. Use `Parameters<T>` (where `T:
   Deserialize + schemars::JsonSchema`) for structured arguments.
4. Add `+ Self::<name>_router()` to the sum in `Manu::new`.
5. If the feature needs state, add an `Arc<…>` field to `Manu` and initialize it
   in `Manu::new` (the struct is cloned per request, so state must be shared).
6. Update `manu_status` in `system.rs` and the table in `README.md`.

## State & configuration

`Manu` is `Clone` (rmcp clones it per request), so all mutable or owned state is
held behind `Arc`. Configuration is centralized in `config.rs` and resolved once
at startup from the environment; each feature is expected to own its typed config
section there as it matures.

## Dependencies

- **rmcp** — MCP server, macros, and the stdio transport (`transport-io`).
- **tokio** — async runtime.
- **serde / serde_json / schemars** — tool argument (de)serialization and JSON
  Schema generation for tool inputs.
- **tracing / tracing-subscriber** — structured logging to stderr.
- **anyhow / thiserror** — error plumbing.
