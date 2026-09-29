//! Google feature: link Google accounts, then use Gmail, Calendar, Drive and
//! Contacts through them.
//!
//! This version reads everything but only writes what affects the user alone
//! (drafts, events without guests, files nobody else can see). Anything
//! reaching other people (sending mail, inviting, sharing, deleting) waits for
//! the authorization layer. Content read from Google is untrusted: anyone can
//! email the user or share a document with them.

mod calendar;
mod contacts;
mod drive;
mod mail;
mod meet;

use std::sync::Arc;

use rmcp::{
    ErrorData as McpError,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content},
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::features::untrusted;
use crate::google::{Area, ClientSource, Google};
use crate::server::Carl;

/// Run blocking Google work off the async runtime. Failures become tool
/// errors (visible to the agent) rather than protocol errors.
async fn run<F>(google: &Arc<Google>, f: F) -> Result<CallToolResult, McpError>
where
    F: FnOnce(&Google) -> anyhow::Result<Value> + Send + 'static,
{
    let google = google.clone();
    let result = tokio::task::spawn_blocking(move || f(&google))
        .await
        .map_err(|e| McpError::internal_error(format!("google task failed: {e}"), None))?;
    Ok(match result {
        Ok(value) => CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()),
        )]),
        Err(e) => CallToolResult::error(vec![Content::text(format!("{e:#}"))]),
    })
}

/// Arguments for [`Carl::google_set_client`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetClientArgs {
    /// Path to the client JSON downloaded from the Google Cloud console
    /// (`client_secret_….json`). Alternatively pass `client_id` and
    /// `client_secret`.
    #[serde(default)]
    pub json_path: Option<String>,
    /// OAuth client ID (`….apps.googleusercontent.com`).
    #[serde(default)]
    pub client_id: Option<String>,
    /// OAuth client secret.
    #[serde(default)]
    pub client_secret: Option<String>,
}

/// Arguments for [`Carl::google_link`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct LinkArgs {
    /// Which parts of the account to grant. Defaults to all of them.
    #[serde(default)]
    pub areas: Option<Vec<Area>>,
    /// Email of the account to link, to preselect it on Google's page.
    #[serde(default)]
    pub login_hint: Option<String>,
}

/// Arguments for [`Carl::google_link_complete`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct LinkCompleteArgs {
    /// The full address the browser ended up on after approving
    /// (`http://127.0.0.1:…/?state=…&code=…`), copied from its address bar.
    pub redirect_url: String,
}

/// Arguments for [`Carl::google_unlink`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct UnlinkArgs {
    /// Email of the linked account to remove.
    pub account: String,
}

/// Arguments naming just an account.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct AccountArgs {
    /// Linked account to use (email). Optional when only one is linked.
    #[serde(default)]
    pub account: Option<String>,
}

#[tool_router(router = google_router, vis = "pub(crate)")]
impl Carl {
    /// Configure the user's own Google OAuth client.
    #[tool(
        name = "google_set_client",
        description = "Configure the Google OAuth client Carl links accounts with. Needed once, before google_link, unless Carl ships a built-in client. The user creates it in the Google Cloud console: APIs & Services → Credentials → Create credentials → OAuth client ID → type 'Desktop app', and enables the Gmail, Google Calendar, Google Drive, Google Sheets, Google Slides, Google Meet REST and People APIs; setting the consent screen to 'In production' avoids tokens expiring weekly. Pass the downloaded JSON file's path (often in ~/Downloads, named client_secret_*.json), or the client ID and secret."
    )]
    async fn google_set_client(
        &self,
        Parameters(args): Parameters<SetClientArgs>,
    ) -> Result<CallToolResult, McpError> {
        run(&self.google, move |google| {
            let (id, secret) = match (args.json_path, args.client_id, args.client_secret) {
                (Some(path), _, _) => {
                    let json: Value = serde_json::from_slice(&std::fs::read(&path)?)?;
                    if json.get("web").is_some() {
                        anyhow::bail!(
                            "that is a 'Web application' client; create a 'Desktop app' one"
                        );
                    }
                    let c = &json["installed"];
                    match (c["client_id"].as_str(), c["client_secret"].as_str()) {
                        (Some(id), Some(secret)) => (id.to_string(), secret.to_string()),
                        _ => anyhow::bail!("no installed.client_id/client_secret in {path}"),
                    }
                }
                (None, Some(id), Some(secret)) => (id, secret),
                _ => anyhow::bail!("pass json_path, or both client_id and client_secret"),
            };
            google.set_client(&id, &secret)?;
            Ok(json!({ "configured": true, "next": "call google_link" }))
        })
        .await
    }

    /// Start linking a Google account.
    #[tool(
        name = "google_link",
        description = "Start linking a Google account to Carl. Returns a URL: show it to the user and ask them to open it and approve. Their browser then returns to Carl on 127.0.0.1 and the link completes by itself; check with google_accounts. If that final page doesn't load (Carl running on another machine), ask the user to paste the full address from the browser's address bar and pass it to google_link_complete. Links expire after 10 minutes. Linking an already-linked account again adds areas."
    )]
    async fn google_link(
        &self,
        Parameters(args): Parameters<LinkArgs>,
    ) -> Result<CallToolResult, McpError> {
        let google = self.google.clone();
        run(&self.google, move |_| {
            let areas = args.areas.unwrap_or_else(|| Area::ALL.to_vec());
            let link = google.start_link(&areas, args.login_hint.as_deref())?;
            Ok(json!({
                "url": link.url,
                "areas": areas.iter().map(|a| a.name()).collect::<Vec<_>>(),
                "redirect_uri": link.redirect_uri,
                "expires_in_minutes": 10,
                "instructions": "Show the user this URL to open and approve. Then call google_accounts to confirm; if the page after approval fails to load, get its full address from the user and call google_link_complete.",
            }))
        })
        .await
    }

    /// Finish a link from a pasted redirect address.
    #[tool(
        name = "google_link_complete",
        description = "Finish linking a Google account when the browser's final page (http://127.0.0.1:…) couldn't load: pass the full address from the browser's address bar. Not needed when that page said the account is linked."
    )]
    async fn google_link_complete(
        &self,
        Parameters(args): Parameters<LinkCompleteArgs>,
    ) -> Result<CallToolResult, McpError> {
        run(&self.google, move |google| {
            let email = google.complete_link(&args.redirect_url)?;
            Ok(json!({ "linked": email }))
        })
        .await
    }

    /// List linked accounts.
    #[tool(
        name = "google_accounts",
        description = "List the Google accounts linked to Carl and which areas (mail, calendar, drive, contacts) each granted, plus whether an OAuth client is configured and how many links are awaiting the user's approval."
    )]
    async fn google_accounts(&self) -> Result<CallToolResult, McpError> {
        run(&self.google, |google| {
            let client = match google.client()? {
                Some(ClientSource::Configured(_)) => "configured",
                Some(ClientSource::Builtin(_)) => "built-in",
                None => "missing (see google_set_client)",
            };
            Ok(json!({
                "accounts": google.accounts()?,
                "oauth_client": client,
                "pending_links": google.pending_links(),
            }))
        })
        .await
    }

    /// Unlink an account.
    #[tool(
        name = "google_unlink",
        description = "Unlink a Google account: revokes Carl's access at Google and forgets its tokens. Only do this when the user asks."
    )]
    async fn google_unlink(
        &self,
        Parameters(args): Parameters<UnlinkArgs>,
    ) -> Result<CallToolResult, McpError> {
        run(&self.google, move |google| {
            if google.unlink(&args.account)? {
                Ok(json!({ "unlinked": args.account }))
            } else {
                anyhow::bail!("{} is not linked", args.account)
            }
        })
        .await
    }
}
