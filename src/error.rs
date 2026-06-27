//! Error helpers.
//!
//! Tool handlers return [`rmcp::ErrorData`] (`McpError`) on failure. To keep
//! feature code terse, this module provides small constructors for the cases
//! Manu hits most often.

use rmcp::ErrorData as McpError;

/// Error returned by features that are scaffolded but not yet implemented.
///
/// This keeps the tool surface honest: the tool exists and is discoverable, but
/// calling it tells the agent (and the user) exactly what is missing.
pub fn not_implemented(feature: &str) -> McpError {
    McpError::internal_error(format!("{feature} is not implemented yet"), None)
}
