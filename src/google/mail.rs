//! Gmail message handling: turning Gmail's MIME tree into readable text, and
//! building RFC 5322 messages for drafts.

use anyhow::{Result, bail};
use serde_json::{Value, json};

use super::encoding::{b64_std_encode, b64url_decode};

/// Longest body text returned per message, in characters.
pub const MAX_BODY_CHARS: usize = 20_000;

/// Value of header `name` in a Gmail `payload.headers` list.
pub fn header<'a>(payload: &'a Value, name: &str) -> Option<&'a str> {
    payload["headers"]
        .as_array()?
        .iter()
        .find(|h| {
            h["name"]
                .as_str()
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
        })
        .and_then(|h| h["value"].as_str())
}

/// A message summary (from a `format=metadata` or `format=full` fetch).
pub fn summary(message: &Value) -> Value {
    let p = &message["payload"];
    json!({
        "id": message["id"],
        "thread_id": message["threadId"],
        "date": header(p, "Date"),
        "from": header(p, "From"),
        "to": header(p, "To"),
        "subject": header(p, "Subject"),
        "snippet": message["snippet"],
        "labels": message["labelIds"],
    })
}

/// A full message: headers, body text, and attachment descriptions.
pub fn full(message: &Value) -> Value {
    let p = &message["payload"];
    let (text, truncated) = truncate(&body_text(p), MAX_BODY_CHARS);
    let mut attachments = Vec::new();
    collect_attachments(p, &mut attachments);
    json!({
        "id": message["id"],
        "thread_id": message["threadId"],
        "labels": message["labelIds"],
        "date": header(p, "Date"),
        "from": header(p, "From"),
        "to": header(p, "To"),
        "cc": header(p, "Cc"),
        "subject": header(p, "Subject"),
        "body": text,
        "body_truncated": truncated,
        "attachments": attachments,
    })
}

/// The readable body: the first `text/plain` part, else the first `text/html`
/// part stripped of markup.
pub fn body_text(payload: &Value) -> String {
    if let Some(text) = find_part(payload, "text/plain") {
        return text;
    }
    find_part(payload, "text/html")
        .map(|html| html_to_text(&html))
        .unwrap_or_default()
}

fn find_part(part: &Value, mime: &str) -> Option<String> {
    let is_attachment = part["filename"].as_str().is_some_and(|f| !f.is_empty());
    if part["mimeType"].as_str() == Some(mime) && !is_attachment {
        let data = part["body"]["data"].as_str()?;
        return Some(String::from_utf8_lossy(&b64url_decode(data).ok()?).into_owned());
    }
    part["parts"]
        .as_array()?
        .iter()
        .find_map(|p| find_part(p, mime))
}

fn collect_attachments(part: &Value, out: &mut Vec<Value>) {
    if let Some(name) = part["filename"].as_str().filter(|f| !f.is_empty()) {
        out.push(json!({
            "filename": name,
            "mime_type": part["mimeType"],
            "size": part["body"]["size"],
        }));
    }
    for p in part["parts"].as_array().into_iter().flatten() {
        collect_attachments(p, out);
    }
}

/// Crude but safe HTML → text: drops tags, `<script>`/`<style>` content and
/// comments, turns block ends into newlines, decodes common entities.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len() / 2);
    let lower = html.to_ascii_lowercase();
    let mut i = 0;
    while i < html.len() {
        let rest = &lower[i..];
        if rest.starts_with("<!--") {
            i += rest.find("-->").map_or(rest.len(), |e| e + 3);
        } else if rest.starts_with("<script") || rest.starts_with("<style") {
            let close = if rest.starts_with("<script") {
                "</script>"
            } else {
                "</style>"
            };
            i += rest.find(close).map_or(rest.len(), |e| e + close.len());
        } else if rest.starts_with('<') {
            let end = rest.find('>').map_or(rest.len(), |e| e + 1);
            let tag = &rest[1..end.saturating_sub(1).max(1)];
            let name = tag
                .trim_start_matches('/')
                .split(|c: char| !c.is_ascii_alphanumeric())
                .next()
                .unwrap_or("");
            if matches!(
                name,
                "br" | "p" | "div" | "tr" | "li" | "h1" | "h2" | "h3" | "h4" | "table"
            ) {
                out.push('\n');
            }
            i += end;
        } else {
            let ch = html[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    let text = out
        .replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&");
    // Collapse runs of blank lines and trailing spaces.
    let mut lines: Vec<&str> = Vec::new();
    for line in text.lines().map(str::trim_end) {
        if line.trim().is_empty() && lines.last().is_none_or(|l| l.trim().is_empty()) {
            continue;
        }
        lines.push(line);
    }
    lines.join("\n").trim().to_string()
}

/// `s` cut to at most `max` characters, and whether it was cut.
pub fn truncate(s: &str, max: usize) -> (String, bool) {
    match s.char_indices().nth(max) {
        Some((i, _)) => (s[..i].to_string(), true),
        None => (s.to_string(), false),
    }
}

/// A draft to build.
pub struct Draft<'a> {
    pub to: &'a [String],
    pub cc: &'a [String],
    pub subject: &'a str,
    pub body: &'a str,
    /// `Message-ID` of the message this replies to.
    pub in_reply_to: Option<&'a str>,
    /// `References` of the message this replies to.
    pub references: Option<&'a str>,
}

/// An RFC 5322 message for `draft`. `From` is left to Gmail (the account's
/// default address).
pub fn build_message(draft: &Draft) -> Result<Vec<u8>> {
    if draft.to.is_empty() {
        bail!("a draft needs at least one recipient");
    }
    if draft.subject.contains(['\r', '\n']) {
        bail!("Subject must be a single line");
    }
    let mut headers = vec![
        ("To", draft.to.join(", ")),
        ("Subject", encode_word(draft.subject)),
        ("MIME-Version", "1.0".to_string()),
        ("Content-Type", "text/plain; charset=UTF-8".to_string()),
        ("Content-Transfer-Encoding", "base64".to_string()),
    ];
    if !draft.cc.is_empty() {
        headers.push(("Cc", draft.cc.join(", ")));
    }
    if let Some(id) = draft.in_reply_to {
        headers.push(("In-Reply-To", id.to_string()));
        let refs = match draft.references {
            Some(r) if !r.trim().is_empty() => format!("{} {id}", r.trim()),
            _ => id.to_string(),
        };
        headers.push(("References", refs));
    }

    let mut out = String::new();
    for (name, value) in &headers {
        // A newline in a header value would let the caller add headers (Bcc,
        // say) or start the body early.
        if value.contains(['\r', '\n']) {
            bail!("{name} must be a single line");
        }
        out.push_str(&format!("{name}: {value}\r\n"));
    }
    out.push_str("\r\n");
    let body = b64_std_encode(
        draft
            .body
            .replace("\r\n", "\n")
            .replace('\n', "\r\n")
            .as_bytes(),
    );
    for chunk in body.as_bytes().chunks(76) {
        out.push_str(std::str::from_utf8(chunk).unwrap());
        out.push_str("\r\n");
    }
    Ok(out.into_bytes())
}

/// `s` as an RFC 2047 encoded-word if it isn't plain ASCII.
fn encode_word(s: &str) -> String {
    if s.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        s.to_string()
    } else {
        format!("=?UTF-8?B?{}?=", b64_std_encode(s.as_bytes()))
    }
}

/// `Re: subject`, unless it already is one.
pub fn reply_subject(subject: &str) -> String {
    if subject.len() >= 3 && subject[..3].eq_ignore_ascii_case("re:") {
        subject.to_string()
    } else {
        format!("Re: {subject}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::google::encoding::b64url_encode;

    #[test]
    fn body_prefers_plain_text_and_skips_attachments() {
        let payload = json!({
            "mimeType": "multipart/mixed",
            "parts": [
                {"mimeType": "text/plain", "filename": "notes.txt",
                 "body": {"data": b64url_encode(b"attached"), "size": 8}},
                {"mimeType": "multipart/alternative", "parts": [
                    {"mimeType": "text/html", "body": {"data": b64url_encode(b"<p>Hi</p>")}},
                    {"mimeType": "text/plain", "body": {"data": b64url_encode("Hi é".as_bytes())}},
                ]},
            ],
        });
        assert_eq!(body_text(&payload), "Hi é");
        let mut attachments = Vec::new();
        collect_attachments(&payload, &mut attachments);
        assert_eq!(attachments.len(), 1);
        assert_eq!(attachments[0]["filename"], "notes.txt");
    }

    #[test]
    fn html_fallback_is_stripped() {
        let html = "<html><style>p{color:red}</style><script>evil()</script>\
                    <p>Hello&nbsp;<b>there</b></p><!-- hidden --><div>a &amp; b</div>";
        assert_eq!(html_to_text(html), "Hello there\n\na & b");
    }

    #[test]
    fn draft_message_is_well_formed() {
        let to = vec!["Bob <bob@x.com>".to_string()];
        let msg = build_message(&Draft {
            to: &to,
            cc: &[],
            subject: "Café",
            body: "line1\nline2",
            in_reply_to: Some("<a@x>"),
            references: Some("<z@x>"),
        })
        .unwrap();
        let msg = String::from_utf8(msg).unwrap();
        assert!(
            msg.starts_with("To: Bob <bob@x.com>\r\nSubject: =?UTF-8?B?Q2Fmw6k=?=\r\n"),
            "{msg}"
        );
        assert!(
            msg.contains("In-Reply-To: <a@x>\r\nReferences: <z@x> <a@x>\r\n"),
            "{msg}"
        );
        let body = msg.split("\r\n\r\n").nth(1).unwrap().trim();
        assert_eq!(body, b64_std_encode(b"line1\r\nline2"));
    }

    #[test]
    fn header_injection_is_refused() {
        let to = vec!["bob@x.com\r\nBcc: eve@x.com".to_string()];
        let draft = Draft {
            to: &to,
            cc: &[],
            subject: "s",
            body: "b",
            in_reply_to: None,
            references: None,
        };
        assert!(build_message(&draft).is_err());
        let to = vec!["bob@x.com".to_string()];
        let draft = Draft {
            to: &to,
            cc: &[],
            subject: "a\nb",
            body: "",
            in_reply_to: None,
            references: None,
        };
        assert!(build_message(&draft).is_err());
    }

    #[test]
    fn helpers() {
        assert_eq!(reply_subject("hello"), "Re: hello");
        assert_eq!(reply_subject("RE: hello"), "RE: hello");
        assert_eq!(truncate("héllo", 2), ("hé".to_string(), true));
        assert_eq!(truncate("hé", 5), ("hé".to_string(), false));
    }
}
