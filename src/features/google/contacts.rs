//! Google Contacts search (read-only).

use rmcp::{
    ErrorData as McpError, handler::server::wrapper::Parameters, model::CallToolResult, tool,
    tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use super::run;
use crate::google::{Area, encoding::form_encode};
use crate::server::Carl;

const SEARCH: &str = "https://people.googleapis.com/v1/people:searchContacts";
const READ_MASK: &str = "names,emailAddresses,phoneNumbers,organizations";

/// Arguments for [`Carl::google_contacts_search`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchArgs {
    /// Linked account to use (email). Optional when only one is linked.
    #[serde(default)]
    pub account: Option<String>,
    /// Name, email, phone or company to look for (prefix match).
    pub query: String,
    /// Maximum contacts (default 10, at most 30).
    #[serde(default)]
    pub max_results: Option<u32>,
}

#[tool_router(router = google_contacts_router, vis = "pub(crate)")]
impl Carl {
    #[tool(
        name = "google_contacts_search",
        description = "Look up people in a linked account's Google Contacts by name, email, phone or company. Returns names, email addresses, phone numbers and organizations, e.g. to find someone's address for google_mail_draft."
    )]
    async fn google_contacts_search(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, McpError> {
        run(&self.google, move |google| {
            let account = google.account_for(args.account.as_deref(), Area::Contacts)?;
            // Google asks for an empty warm-up search before the first real
            // one, or results can come back stale or empty.
            let warm = google.contacts_warmed.lock().unwrap().contains(&account);
            if !warm {
                google.api(
                    &account,
                    "GET",
                    &format!(
                        "{SEARCH}?{}",
                        form_encode(&[("query", ""), ("readMask", "names")])
                    ),
                    None,
                )?;
                google
                    .contacts_warmed
                    .lock()
                    .unwrap()
                    .insert(account.clone());
            }
            let size = args.max_results.unwrap_or(10).clamp(1, 30).to_string();
            let resp = google.api(
                &account,
                "GET",
                &format!(
                    "{SEARCH}?{}",
                    form_encode(&[
                        ("query", &args.query),
                        ("readMask", READ_MASK),
                        ("pageSize", &size)
                    ])
                ),
                None,
            )?;
            let contacts: Vec<Value> = resp["results"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|r| contact(&r["person"]))
                .collect();
            Ok(json!({ "account": account, "contacts": contacts }))
        })
        .await
    }
}

fn contact(p: &Value) -> Value {
    let values = |field: &str, key: &str| -> Vec<Value> {
        p[field]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v[key].as_str().map(|s| json!(s)))
            .collect()
    };
    json!({
        "name": p["names"][0]["displayName"],
        "emails": values("emailAddresses", "value"),
        "phones": values("phoneNumbers", "value"),
        "organization": p["organizations"][0]["name"],
        "title": p["organizations"][0]["title"],
    })
}
