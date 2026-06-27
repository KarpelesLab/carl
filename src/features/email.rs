//! Email feature (scaffold).
//!
//! Goal: let an agent create and manage email identities and send/receive mail,
//! backed by Karpelès Lab email APIs.
//!
//! Status: the tool surface below is intentional, but every handler currently
//! returns "not implemented". The API client, authentication, and address
//! lifecycle are still to be designed — see `docs/email.md`.

use rmcp::{
    ErrorData as McpError, handler::server::wrapper::Parameters, model::CallToolResult, tool,
    tool_router,
};
use serde::Deserialize;

use crate::{error::not_implemented, server::Manu};

/// Arguments for [`Manu::email_create`].
// Fields define the tool's JSON schema (via serde/schemars); they are wired to
// real logic when the handler is implemented.
#[allow(dead_code)]
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct CreateAddressArgs {
    /// Desired local part / mailbox name. If omitted, one is generated.
    #[serde(default)]
    pub name: Option<String>,
}

/// Arguments for [`Manu::email_send`].
#[allow(dead_code)]
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SendEmailArgs {
    /// Sending address (must be one Manu manages).
    pub from: String,
    /// Recipient address.
    pub to: String,
    /// Subject line.
    pub subject: String,
    /// Plain-text body.
    pub body: String,
}

#[tool_router(router = email_router, vis = "pub(crate)")]
impl Manu {
    /// Create a new email address Manu can send and receive from.
    #[tool(
        name = "email_create",
        description = "Create a new managed email address. (Not yet implemented.)"
    )]
    fn email_create(
        &self,
        Parameters(_args): Parameters<CreateAddressArgs>,
    ) -> Result<CallToolResult, McpError> {
        Err(not_implemented("email_create"))
    }

    /// List the email addresses Manu currently manages.
    #[tool(
        name = "email_list",
        description = "List managed email addresses. (Not yet implemented.)"
    )]
    fn email_list(&self) -> Result<CallToolResult, McpError> {
        Err(not_implemented("email_list"))
    }

    /// Send an email from a managed address.
    #[tool(
        name = "email_send",
        description = "Send an email from a managed address. (Not yet implemented.)"
    )]
    fn email_send(
        &self,
        Parameters(_args): Parameters<SendEmailArgs>,
    ) -> Result<CallToolResult, McpError> {
        Err(not_implemented("email_send"))
    }
}
