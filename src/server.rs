//! The Manu MCP server.
//!
//! [`Manu`] is the single [`ServerHandler`] for the process. Each feature
//! (system, wallet, email, …) contributes a *tool router* via an `impl Manu`
//! block in its own module under [`crate::features`]. Those routers are merged
//! in [`Manu::new`] using [`std::ops::Add`] on [`ToolRouter`], which is the
//! idiomatic rmcp pattern for composing a multi-feature server.
//!
//! To add a feature:
//!   1. Create `src/features/<name>.rs`.
//!   2. Add `#[tool_router(router = <name>_router, vis = "pub(crate)")]` to an
//!      `impl Manu` block holding that feature's `#[tool]` methods.
//!   3. Add `Self::<name>_router()` to the sum in [`Manu::new`].
//!   4. Register the module in `src/features/mod.rs`.

use std::sync::Arc;

use rmcp::{ServerHandler, handler::server::router::tool::ToolRouter, model::*, tool_handler};

use crate::config::Config;

/// Instructions surfaced to the agent during MCP initialization. Keep this in
/// sync with the set of routers composed in [`Manu::new`].
const INSTRUCTIONS: &str = "\
Manu gives you hands: it exposes real-world actions as MCP tools. \
Use `manu_status` to see which feature areas are available. \
Wallet and email features are scaffolded and will report when an action is \
not yet implemented — prefer checking status before relying on them.";

/// The Manu server. Cloned per request by rmcp, so all state lives behind
/// [`Arc`]. Feature-specific state will be added as additional `Arc` fields.
#[derive(Clone)]
pub struct Manu {
    pub config: Arc<Config>,
    tool_router: ToolRouter<Manu>,
}

impl Manu {
    /// Construct the server, composing every feature's tool router.
    pub fn new(config: Config) -> Self {
        Self {
            config: Arc::new(config),
            tool_router: Self::system_router() + Self::wallet_router() + Self::email_router(),
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Manu {
    fn get_info(&self) -> ServerInfo {
        // Identify as `manu` (not the rmcp crate, which `from_build_env` reports).
        let implementation = Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
            .with_title("Manu")
            .with_website_url(env!("CARGO_PKG_REPOSITORY"));

        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(implementation)
            .with_instructions(INSTRUCTIONS.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn composes_every_feature_tool() {
        let manu = Manu::new(Config::default());
        let names: Vec<String> = manu
            .tool_router
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();

        let expected = [
            "manu_status",
            "manu_ping",
            "wallet_balance",
            "wallet_address",
            "wallet_send",
            "email_create",
            "email_list",
            "email_send",
        ];
        for tool in expected {
            assert!(names.contains(&tool.to_string()), "missing tool {tool}");
        }
        assert_eq!(
            names.len(),
            expected.len(),
            "unexpected tool count: {names:?}"
        );
    }
}
