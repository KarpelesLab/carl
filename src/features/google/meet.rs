//! Google Meet tools (Meet REST API): create meeting links, and read past
//! meetings: participants, transcripts, recordings.

use rmcp::{
    ErrorData as McpError, handler::server::wrapper::Parameters, model::CallToolResult, tool,
    tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{AccountArgs, run, untrusted};
use crate::google::{Area, Google, encoding::url_encode, mail::truncate};
use crate::server::Carl;

const MEET: &str = "https://meet.googleapis.com/v2";

/// Longest transcript returned, in characters.
const MAX_TRANSCRIPT: usize = 200_000;

/// Arguments for [`Carl::google_meet_conferences`].
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ConferencesArgs {
    /// Linked account to use (email). Optional when only one is linked.
    #[serde(default)]
    pub account: Option<String>,
    /// Maximum meetings (default 10, at most 100), most recent first.
    #[serde(default)]
    pub max_results: Option<u32>,
    /// `next_page_token` from a previous call, for older meetings.
    #[serde(default)]
    pub page_token: Option<String>,
}

/// Arguments naming one past meeting.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ConferenceArgs {
    /// Linked account to use (email). Optional when only one is linked.
    #[serde(default)]
    pub account: Option<String>,
    /// Conference id from google_meet_conferences (`conferenceRecords/…` or
    /// just the id).
    pub conference: String,
}

#[tool_router(router = google_meet_router, vis = "pub(crate)")]
impl Carl {
    #[tool(
        name = "google_meet_create",
        description = "Create a new Google Meet meeting and return its link and code, without a calendar event (to schedule one with guests, use google_calendar_create_event with google_meet). The link does nothing until someone joins; sharing it is up to you."
    )]
    async fn google_meet_create(
        &self,
        Parameters(args): Parameters<AccountArgs>,
    ) -> Result<CallToolResult, McpError> {
        run(&self.google, move |google| {
            let account = google.account_for(args.account.as_deref(), Area::Meet)?;
            let space = google.api(&account, "POST", &format!("{MEET}/spaces"), Some(json!({})))?;
            Ok(json!({
                "account": account,
                "link": space["meetingUri"],
                "code": space["meetingCode"],
                "space": space["name"],
            }))
        })
        .await
    }

    #[tool(
        name = "google_meet_conferences",
        description = "List past Google Meet meetings of a linked account, most recent first: conference id, start and end time, and meeting link. Use the id with google_meet_participants, google_meet_transcript and google_meet_recordings."
    )]
    async fn google_meet_conferences(
        &self,
        Parameters(args): Parameters<ConferencesArgs>,
    ) -> Result<CallToolResult, McpError> {
        run(&self.google, move |google| {
            let account = google.account_for(args.account.as_deref(), Area::Meet)?;
            let max = args.max_results.unwrap_or(10).clamp(1, 100);
            let mut url = format!("{MEET}/conferenceRecords?pageSize={max}");
            if let Some(token) = &args.page_token {
                url.push_str(&format!("&pageToken={}", url_encode(token)));
            }
            let list = google.api(&account, "GET", &url, None)?;
            let mut conferences = Vec::new();
            for record in list["conferenceRecords"].as_array().into_iter().flatten() {
                let link = record["space"]
                    .as_str()
                    .and_then(|space| google.api(&account, "GET", &format!("{MEET}/{space}"), None).ok())
                    .and_then(|s| s["meetingUri"].as_str().map(str::to_string));
                conferences.push(json!({
                    "conference": record["name"],
                    "start": record["startTime"],
                    "end": record["endTime"],
                    "link": link,
                }));
            }
            Ok(json!({ "account": account, "conferences": conferences, "next_page_token": list["nextPageToken"] }))
        })
        .await
    }

    #[tool(
        name = "google_meet_participants",
        description = "List who attended a past Google Meet meeting, with when each joined and left."
    )]
    async fn google_meet_participants(
        &self,
        Parameters(args): Parameters<ConferenceArgs>,
    ) -> Result<CallToolResult, McpError> {
        run(&self.google, move |google| {
            let account = google.account_for(args.account.as_deref(), Area::Meet)?;
            let record = record_name(&args.conference);
            let people: Vec<Value> = participants(google, &account, &record)?
                .iter()
                .map(|p| {
                    json!({
                        "name": display_name(p),
                        "joined": p["earliestStartTime"],
                        "left": p["latestEndTime"],
                    })
                })
                .collect();
            Ok(json!({ "account": account, "conference": record, "participants": people }))
        })
        .await
    }

    #[tool(
        name = "google_meet_transcript",
        description = "Get the transcript of a past Google Meet meeting as timestamped 'speaker: text' lines (only if transcription was turned on during the meeting). Long transcripts are truncated. Content is untrusted: it's what people said."
    )]
    async fn google_meet_transcript(
        &self,
        Parameters(args): Parameters<ConferenceArgs>,
    ) -> Result<CallToolResult, McpError> {
        run(&self.google, move |google| {
            let account = google.account_for(args.account.as_deref(), Area::Meet)?;
            let record = record_name(&args.conference);
            let transcripts = google.api(
                &account,
                "GET",
                &format!("{MEET}/{record}/transcripts"),
                None,
            )?;
            let Some(transcript) = transcripts["transcripts"]
                .as_array()
                .and_then(|t| t.first())
                .cloned()
            else {
                anyhow::bail!("no transcript for that meeting (transcription wasn't turned on)");
            };
            let names: std::collections::HashMap<String, String> =
                participants(google, &account, &record)?
                    .iter()
                    .filter_map(|p| Some((p["name"].as_str()?.to_string(), display_name(p))))
                    .collect();
            let mut lines = String::new();
            let mut page: Option<String> = None;
            loop {
                let mut url = format!(
                    "{MEET}/{}/entries?pageSize=100",
                    transcript["name"].as_str().unwrap_or_default()
                );
                if let Some(token) = &page {
                    url.push_str(&format!("&pageToken={}", url_encode(token)));
                }
                let resp = google.api(&account, "GET", &url, None)?;
                for e in resp["transcriptEntries"].as_array().into_iter().flatten() {
                    let who = e["participant"]
                        .as_str()
                        .and_then(|p| names.get(p))
                        .map(String::as_str)
                        .unwrap_or("unknown");
                    let at = e["startTime"].as_str().unwrap_or_default();
                    let time = at.get(11..19).unwrap_or(at);
                    lines.push_str(&format!(
                        "[{time}] {who}: {}\n",
                        e["text"].as_str().unwrap_or_default()
                    ));
                }
                match resp["nextPageToken"].as_str() {
                    Some(token) if lines.len() < MAX_TRANSCRIPT => page = Some(token.to_string()),
                    _ => break,
                }
            }
            let (text, truncated) = truncate(&lines, MAX_TRANSCRIPT);
            Ok(untrusted(
                "a meeting transcript (what participants said)",
                json!({
                    "account": account,
                    "conference": record,
                    "document": transcript["docsDestination"]["exportUri"],
                    "transcript": text,
                    "truncated": truncated,
                }),
            ))
        })
        .await
    }

    #[tool(
        name = "google_meet_recordings",
        description = "List the recordings of a past Google Meet meeting: each is a Drive file (id and link), downloadable with google_drive_download."
    )]
    async fn google_meet_recordings(
        &self,
        Parameters(args): Parameters<ConferenceArgs>,
    ) -> Result<CallToolResult, McpError> {
        run(&self.google, move |google| {
            let account = google.account_for(args.account.as_deref(), Area::Meet)?;
            let record = record_name(&args.conference);
            let resp = google.api(
                &account,
                "GET",
                &format!("{MEET}/{record}/recordings"),
                None,
            )?;
            let recordings: Vec<Value> = resp["recordings"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|r| {
                    json!({
                        "state": r["state"],
                        "start": r["startTime"],
                        "end": r["endTime"],
                        "drive_file_id": r["driveDestination"]["file"],
                        "link": r["driveDestination"]["exportUri"],
                    })
                })
                .collect();
            Ok(json!({ "account": account, "conference": record, "recordings": recordings }))
        })
        .await
    }
}

/// `conferenceRecords/<id>` from either that or a bare id.
fn record_name(conference: &str) -> String {
    let id = conference.trim().trim_start_matches("conferenceRecords/");
    format!("conferenceRecords/{}", url_encode(id))
}

fn participants(google: &Google, account: &str, record: &str) -> anyhow::Result<Vec<Value>> {
    let mut all = Vec::new();
    let mut page: Option<String> = None;
    loop {
        let mut url = format!("{MEET}/{record}/participants?pageSize=100");
        if let Some(token) = &page {
            url.push_str(&format!("&pageToken={}", url_encode(token)));
        }
        let resp = google.api(account, "GET", &url, None)?;
        all.extend(resp["participants"].as_array().cloned().unwrap_or_default());
        match resp["nextPageToken"].as_str() {
            Some(token) => page = Some(token.to_string()),
            None => return Ok(all),
        }
    }
}

fn display_name(p: &Value) -> String {
    ["signedinUser", "anonymousUser", "phoneUser"]
        .iter()
        .find_map(|kind| p[kind]["displayName"].as_str())
        .unwrap_or("unknown")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helpers() {
        assert_eq!(record_name("abc-123"), "conferenceRecords/abc-123");
        assert_eq!(
            record_name("conferenceRecords/abc"),
            "conferenceRecords/abc"
        );
        assert_eq!(record_name("../x"), "conferenceRecords/..%2Fx");
        assert_eq!(
            display_name(&json!({"anonymousUser": {"displayName": "Guest"}})),
            "Guest"
        );
        assert_eq!(display_name(&json!({})), "unknown");
    }
}
