//! Wallet feature (scaffold).
//!
//! Goal: let an agent custody value and move it — check balances, derive
//! receiving addresses, and send funds — under explicit, auditable policy.
//!
//! Status: the tool surface below is intentional and stable-ish, but every
//! handler currently returns "not implemented". Key management, the chain
//! abstraction, and the spend-authorization policy are still to be designed —
//! see `docs/wallet.md`. Nothing here touches real key material yet.

use rmcp::{
    ErrorData as McpError, handler::server::wrapper::Parameters, model::CallToolResult, tool,
    tool_router,
};
use serde::Deserialize;

use crate::{error::not_implemented, server::Carl};

/// Arguments for [`Carl::wallet_send`].
// Fields define the tool's JSON schema (via serde/schemars); they are wired to
// real logic when the handler is implemented.
#[allow(dead_code)]
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SendArgs {
    /// Destination address.
    pub to: String,
    /// Amount to send, as a decimal string to avoid float rounding.
    pub amount: String,
    /// Asset/currency symbol (e.g. "BTC", "ETH", "USDC").
    pub asset: String,
}

/// Arguments for balance/address queries scoped to a single asset.
#[allow(dead_code)]
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AssetArgs {
    /// Asset/currency symbol (e.g. "BTC", "ETH", "USDC").
    pub asset: String,
}

#[tool_router(router = wallet_router, vis = "pub(crate)")]
impl Carl {
    /// Return the spendable balance for an asset.
    #[tool(
        name = "wallet_balance",
        description = "Get the wallet balance for a given asset. (Not yet implemented.)"
    )]
    fn wallet_balance(
        &self,
        Parameters(_args): Parameters<AssetArgs>,
    ) -> Result<CallToolResult, McpError> {
        Err(not_implemented("wallet_balance"))
    }

    /// Return a receiving address for an asset.
    #[tool(
        name = "wallet_address",
        description = "Get a receiving address for a given asset. (Not yet implemented.)"
    )]
    fn wallet_address(
        &self,
        Parameters(_args): Parameters<AssetArgs>,
    ) -> Result<CallToolResult, McpError> {
        Err(not_implemented("wallet_address"))
    }

    /// Send funds. Will require explicit spend authorization once implemented.
    #[tool(
        name = "wallet_send",
        description = "Send funds to an address. (Not yet implemented.)"
    )]
    fn wallet_send(
        &self,
        Parameters(_args): Parameters<SendArgs>,
    ) -> Result<CallToolResult, McpError> {
        Err(not_implemented("wallet_send"))
    }
}
