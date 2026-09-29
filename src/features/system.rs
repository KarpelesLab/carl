//! System feature: always-visible tools to discover Carl and choose which
//! areas of tools this session exposes (see [`crate::areas`]).

use rmcp::{
    ErrorData as McpError, RoleServer,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content},
    service::RequestContext,
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::areas::{self, AREAS};
use crate::server::Carl;

/// Arguments for [`Carl::carl_enable`] and [`Carl::carl_disable`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AreasArgs {
    /// Area ids from carl_status, e.g. `["google.mail"]`. `google` means all
    /// Google areas.
    pub areas: Vec<String>,
}

fn text(value: Value) -> CallToolResult {
    CallToolResult::success(vec![Content::text(
        serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()),
    )])
}

impl Carl {
    /// The tool names in `area`.
    fn tools_in(&self, area: &str) -> Vec<String> {
        let mut names: Vec<String> = self
            .tool_router
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .filter(|name| areas::area_of(name) == Some(area))
            .collect();
        names.sort();
        names
    }

    /// Apply `change` to this session's areas, tell the client its tool list
    /// changed, and report the result.
    async fn change_areas(
        &self,
        ids: &[String],
        enable: bool,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
        let requested = match areas::expand(&ids) {
            Ok(a) => a,
            Err(e) => return Ok(CallToolResult::error(vec![Content::text(e)])),
        };
        let (changed, enabled) = {
            let mut enabled = self.areas.lock().unwrap();
            let before = enabled.clone();
            if enable {
                enabled.extend(requested.iter().copied());
            } else {
                for area in &requested {
                    enabled.remove(area);
                }
                // The account tools go with the last Google area.
                if !enabled.iter().any(|a| a.starts_with("google.")) {
                    enabled.remove("google");
                }
            }
            let changed: Vec<&str> = before.symmetric_difference(&enabled).copied().collect();
            (changed, enabled.clone())
        };
        if !changed.is_empty() {
            // Clients refetch tools/list on this. Best effort: a client that
            // doesn't support it simply keeps its old list.
            let _ = context.peer.notify_tool_list_changed().await;
        }
        let tools: Vec<String> = changed.iter().flat_map(|a| self.tools_in(a)).collect();
        Ok(text(json!({
            "enabled_areas": enabled,
            if enable { "added_tools" } else { "removed_tools" }: tools,
            "note": if enable && !tools.is_empty() {
                "Your client was told the tool list changed. If these tools don't appear, \
                 it doesn't support that: ask the user to set CARL_AREAS (e.g. \
                 CARL_AREAS=all) in Carl's MCP server config instead."
            } else { "" },
        })))
    }
}

#[tool_router(router = system_router, vis = "pub(crate)")]
impl Carl {
    #[tool(
        name = "carl_status",
        description = "Show Carl's version and its tool areas (agents, google.mail, google.calendar, google.drive, …): what each does, its tools, and whether it's enabled in this session. Enable the ones your task needs with carl_enable."
    )]
    async fn carl_status(&self) -> Result<CallToolResult, McpError> {
        let enabled = self.areas.lock().unwrap().clone();
        let areas: Vec<Value> = AREAS
            .iter()
            .map(|a| {
                json!({
                    "id": a.id,
                    "description": a.description,
                    "available": a.available,
                    "enabled": enabled.contains(a.id),
                    "tools": if a.available { self.tools_in(a.id) } else { Vec::new() },
                })
            })
            .collect();
        Ok(text(json!({
            "name": env!("CARGO_PKG_NAME"),
            "version": env!("CARGO_PKG_VERSION"),
            "data_dir": self.config.data_dir.display().to_string(),
            "areas": areas,
        })))
    }

    #[tool(
        name = "carl_enable",
        description = "Turn on tool areas for this session, e.g. [\"google.mail\"] to get the Gmail tools, [\"google\"] for all Google tools. Their tools then appear in your tool list. See carl_status for the areas; enable only what the task needs."
    )]
    async fn carl_enable(
        &self,
        Parameters(args): Parameters<AreasArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.change_areas(&args.areas, true, context).await
    }

    #[tool(
        name = "carl_disable",
        description = "Turn off tool areas in this session, removing their tools from your list (e.g. once you're done with email)."
    )]
    async fn carl_disable(
        &self,
        Parameters(args): Parameters<AreasArgs>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.change_areas(&args.areas, false, context).await
    }

    /// Liveness check.
    #[tool(name = "carl_ping", description = "Health check; returns \"pong\".")]
    fn carl_ping(&self) -> Result<CallToolResult, McpError> {
        Ok(CallToolResult::success(vec![Content::text("pong")]))
    }
}
