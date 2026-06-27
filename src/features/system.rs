//! System feature: always-on introspection tools.
//!
//! These tools have no side effects and let an agent discover what Manu can do
//! and confirm the connection is healthy.

use rmcp::{ErrorData as McpError, model::CallToolResult, model::Content, tool, tool_router};
use serde_json::json;

use crate::server::Manu;

#[tool_router(router = system_router, vis = "pub(crate)")]
impl Manu {
    /// Report Manu's version and the status of each feature area.
    #[tool(
        name = "manu_status",
        description = "Report Manu's version and which feature areas are available. Call this first to learn what actions you can take."
    )]
    fn manu_status(&self) -> Result<CallToolResult, McpError> {
        let status = json!({
            "name": env!("CARGO_PKG_NAME"),
            "version": env!("CARGO_PKG_VERSION"),
            "data_dir": self.config.data_dir.display().to_string(),
            "features": {
                "system": "available",
                "wallet": "scaffolded",
                "email": "scaffolded",
            },
        });
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&status).unwrap_or_else(|_| status.to_string()),
        )]))
    }

    /// Liveness check.
    #[tool(name = "manu_ping", description = "Health check; returns \"pong\".")]
    fn manu_ping(&self) -> Result<CallToolResult, McpError> {
        Ok(CallToolResult::success(vec![Content::text("pong")]))
    }
}
