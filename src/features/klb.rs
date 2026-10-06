//! KarpelesLab / AtOnline platform tools (`klb_*`): log in, call the REST
//! API, upload files. See [`crate::klb`].

use std::{collections::HashMap, io::Cursor, path::PathBuf, sync::Arc};

use rmcp::{
    ErrorData as McpError,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, Content},
    tool, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::google::encoding::b64_std_decode;
use crate::klb::{Klb, confine};
use crate::server::Carl;

/// Run blocking platform work off the async runtime; failures become tool
/// errors the agent can read.
async fn run<F>(klb: &Arc<Klb>, f: F) -> Result<CallToolResult, McpError>
where
    F: FnOnce(&Arc<Klb>) -> anyhow::Result<Value> + Send + 'static,
{
    let klb = klb.clone();
    let result = tokio::task::spawn_blocking(move || f(&klb))
        .await
        .map_err(|e| McpError::internal_error(format!("platform task failed: {e}"), None))?;
    Ok(match result {
        Ok(v) => CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string()),
        )]),
        Err(e) => CallToolResult::error(vec![Content::text(format!("{e:#}"))]),
    })
}

/// Arguments for [`Carl::klb_login`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct LoginArgs {
    /// Log in again even if a login is already stored.
    #[serde(default)]
    pub force: bool,
}

/// Arguments for [`Carl::klb_api`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ApiArgs {
    /// REST path, e.g. `User/@`, `Cloud/Aws/Bucket`, `Shell:list`.
    pub path: String,
    /// HTTP method: GET (default), POST, PATCH, PUT or DELETE.
    #[serde(default)]
    pub method: Option<String>,
    /// Parameters (query for GET, JSON body otherwise).
    #[serde(default)]
    pub params: Option<Value>,
}

/// Arguments for [`Carl::klb_upload`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct UploadArgs {
    /// The REST method that accepts the upload, e.g. `Misc/Debug:testUpload`
    /// or an object's `:upload` endpoint.
    pub endpoint: String,
    /// HTTP method of that endpoint (default POST).
    #[serde(default)]
    pub method: Option<String>,
    /// Extra parameters for the endpoint. `filename` defaults to the file's
    /// name.
    #[serde(default)]
    pub params: Option<serde_json::Map<String, Value>>,
    /// A local file to upload: must be inside your working directory (or
    /// ~/Downloads/carl).
    #[serde(default)]
    pub path: Option<String>,
    /// Text content to upload instead of a file.
    #[serde(default)]
    pub content: Option<String>,
    /// Binary content to upload instead of a file, base64-encoded.
    #[serde(default)]
    pub content_base64: Option<String>,
    /// File name (required with content; defaults to the path's name).
    #[serde(default)]
    pub filename: Option<String>,
    /// MIME type; guessed from the file name when omitted.
    #[serde(default)]
    pub mime_type: Option<String>,
}

#[tool_router(router = klb_router, vis = "pub(crate)")]
impl Carl {
    #[tool(
        name = "klb_login",
        description = "Log in to the KarpelesLab / AtOnline platform (hub.atonline.com). Returns a URL: show it to the user to open and approve; the login completes by itself (check with klb_whoami). Shares its login with the shells-support tools (~/.config/atonline), so it may already be logged in."
    )]
    async fn klb_login(
        &self,
        Parameters(args): Parameters<LoginArgs>,
    ) -> Result<CallToolResult, McpError> {
        run(&self.klb, move |klb| {
            if klb.logged_in() && !args.force {
                return Ok(json!({ "already_logged_in": true, "next": "klb_whoami shows the account; pass force to log in again" }));
            }
            let url = klb.start_login()?;
            Ok(json!({
                "url": url,
                "expires_in_minutes": 10,
                "instructions": "Show the user this URL to open and approve, then call klb_whoami to confirm.",
            }))
        })
        .await
    }

    #[tool(
        name = "klb_whoami",
        description = "Show which KarpelesLab / AtOnline platform account Carl is logged in as (id, email, name), or whether a login is pending."
    )]
    async fn klb_whoami(&self) -> Result<CallToolResult, McpError> {
        run(&self.klb, |klb| {
            if !klb.logged_in() {
                return Ok(json!({ "logged_in": false, "login_pending": klb.login_pending() }));
            }
            let user = klb.with_client(|c| c.do_request("User/@", "GET", json!({})))?;
            let u = user.data.unwrap_or_default();
            Ok(json!({
                "logged_in": true,
                "id": u["User__"],
                "email": u["Email"],
                "name": u["Display_Name"],
                "login": u["Login"],
            }))
        })
        .await
    }

    #[tool(
        name = "klb_api",
        description = "Call the KarpelesLab / AtOnline platform REST API as the logged-in account: GET to read, POST/PATCH/PUT/DELETE to change things (these act with the user's account, so only make changes the user asked for). Returns the response data (and paging when listing)."
    )]
    async fn klb_api(
        &self,
        Parameters(args): Parameters<ApiArgs>,
    ) -> Result<CallToolResult, McpError> {
        run(&self.klb, move |klb| {
            let method = args.method.as_deref().unwrap_or("GET").to_ascii_uppercase();
            if !matches!(method.as_str(), "GET" | "POST" | "PATCH" | "PUT" | "DELETE") {
                anyhow::bail!("method must be GET, POST, PATCH, PUT or DELETE");
            }
            let params = args.params.clone().unwrap_or_else(|| json!({}));
            if method != "GET" {
                tracing::info!(path = args.path, method, "platform API change");
            }
            let resp = klb.with_client(|c| c.do_request(&args.path, &method, params.clone()))?;
            Ok(json!({ "data": resp.data, "paging": resp.paging, "job": resp.job }))
        })
        .await
    }

    #[tool(
        name = "klb_upload",
        description = "Upload a file to the KarpelesLab / AtOnline platform through an endpoint that accepts uploads (klbfw handles direct, multipart and S3 uploads for large files). Give a local `path` inside your working directory (or ~/Downloads/carl), or small `content`/`content_base64` with a `filename`. Returns the endpoint's response."
    )]
    async fn klb_upload(
        &self,
        Parameters(args): Parameters<UploadArgs>,
    ) -> Result<CallToolResult, McpError> {
        // Where this agent may upload from: its own working directory, and
        // Carl's downloads.
        let mut roots: Vec<PathBuf> = self
            .session
            .and_then(|id| self.agents.cwd(id))
            .into_iter()
            .collect();
        roots.extend(crate::google::downloads_dir());
        run(&self.klb, move |klb| {
            let method = args
                .method
                .as_deref()
                .unwrap_or("POST")
                .to_ascii_uppercase();
            let (data, name): (UploadSource, String) =
                match (&args.path, &args.content, &args.content_base64) {
                    (Some(p), None, None) => {
                        let real = confine(std::path::Path::new(p), &roots)?;
                        let name = args.filename.clone().unwrap_or_else(|| {
                            real.file_name()
                                .map(|n| n.to_string_lossy().into_owned())
                                .unwrap_or_default()
                        });
                        (UploadSource::File(real), name)
                    }
                    (None, Some(text), None) => (
                        UploadSource::Bytes(text.clone().into_bytes()),
                        required_name(&args)?,
                    ),
                    (None, None, Some(b64)) => (
                        UploadSource::Bytes(b64_std_decode(b64)?),
                        required_name(&args)?,
                    ),
                    _ => anyhow::bail!("pass exactly one of path, content or content_base64"),
                };
            let mime = args
                .mime_type
                .clone()
                .unwrap_or_else(|| crate::google::mail::mime_for(&name).to_string());
            let mut params: HashMap<String, Value> = args
                .params
                .clone()
                .unwrap_or_default()
                .into_iter()
                .collect();
            params
                .entry("filename".into())
                .or_insert_with(|| json!(name));

            let resp = klb.with_client(|c| match &data {
                UploadSource::File(path) => {
                    let file = std::fs::File::open(path)?;
                    klbfw::upload(
                        c,
                        &args.endpoint,
                        &method,
                        params.clone(),
                        file,
                        &mime,
                        None,
                    )
                }
                UploadSource::Bytes(bytes) => klbfw::upload(
                    c,
                    &args.endpoint,
                    &method,
                    params.clone(),
                    Cursor::new(bytes.clone()),
                    &mime,
                    None,
                ),
            })?;
            tracing::info!(endpoint = args.endpoint, filename = name, "platform upload");
            Ok(json!({ "uploaded": name, "data": resp.data }))
        })
        .await
    }
}

enum UploadSource {
    File(PathBuf),
    Bytes(Vec<u8>),
}

fn required_name(args: &UploadArgs) -> anyhow::Result<String> {
    args.filename
        .clone()
        .filter(|f| !f.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("content uploads need a filename"))
}
