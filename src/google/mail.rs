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
            "attachment_id": part["body"]["attachmentId"],
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

/// A file attached to a message.
pub struct Attachment {
    pub filename: String,
    pub mime_type: String,
    pub data: Vec<u8>,
}

/// A message to build (a draft, or one to send).
pub struct Draft<'a> {
    pub to: &'a [String],
    pub cc: &'a [String],
    pub bcc: &'a [String],
    pub subject: &'a str,
    pub body: &'a str,
    pub attachments: &'a [Attachment],
    /// `Message-ID` of the message this replies to.
    pub in_reply_to: Option<&'a str>,
    /// `References` of the message this replies to.
    pub references: Option<&'a str>,
}

/// An RFC 5322 message for `draft`. `From` is left to Gmail (the account's
/// default address). With attachments it is `multipart/mixed`.
pub fn build_message(draft: &Draft) -> Result<Vec<u8>> {
    if draft.to.is_empty() && draft.cc.is_empty() && draft.bcc.is_empty() {
        bail!("a message needs at least one recipient");
    }
    if draft.subject.contains(['\r', '\n']) {
        bail!("Subject must be a single line");
    }
    let boundary = format!("carl-{}", super::encoding::random_token(12));
    let mut headers = vec![("Subject", encode_word(draft.subject))];
    for (name, list) in [("To", draft.to), ("Cc", draft.cc), ("Bcc", draft.bcc)] {
        if !list.is_empty() {
            let encoded: Vec<String> = list.iter().map(|a| encode_address(a)).collect();
            headers.push((name, encoded.join(", ")));
        }
    }
    headers.push(("MIME-Version", "1.0".to_string()));
    if draft.attachments.is_empty() {
        headers.push(("Content-Type", "text/plain; charset=UTF-8".to_string()));
        headers.push(("Content-Transfer-Encoding", "base64".to_string()));
    } else {
        headers.push((
            "Content-Type",
            format!("multipart/mixed; boundary=\"{boundary}\""),
        ));
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
    let text = draft.body.replace("\r\n", "\n").replace('\n', "\r\n");
    if draft.attachments.is_empty() {
        push_base64(&mut out, text.as_bytes());
        return Ok(out.into_bytes());
    }

    out.push_str(&format!(
        "--{boundary}\r\nContent-Type: text/plain; charset=UTF-8\r\n\
         Content-Transfer-Encoding: base64\r\n\r\n"
    ));
    push_base64(&mut out, text.as_bytes());
    for a in draft.attachments {
        let mime = if a.mime_type.contains(['\r', '\n', '"', ';']) || a.mime_type.is_empty() {
            "application/octet-stream"
        } else {
            a.mime_type.as_str()
        };
        let name = filename_param(&a.filename);
        out.push_str(&format!(
            "--{boundary}\r\nContent-Type: {mime}\r\n\
             Content-Disposition: attachment; {name}\r\n\
             Content-Transfer-Encoding: base64\r\n\r\n"
        ));
        push_base64(&mut out, &a.data);
    }
    out.push_str(&format!("--{boundary}--\r\n"));
    Ok(out.into_bytes())
}

/// Append `data` as base64 in 76-character lines.
fn push_base64(out: &mut String, data: &[u8]) {
    let encoded = b64_std_encode(data);
    for chunk in encoded.as_bytes().chunks(76) {
        out.push_str(std::str::from_utf8(chunk).expect("base64 is ASCII"));
        out.push_str("\r\n");
    }
}

/// `filename="…"` for plain names, else RFC 2231 `filename*=UTF-8''…`.
fn filename_param(name: &str) -> String {
    let name = super::safe_filename(name);
    if name
        .bytes()
        .all(|b| (0x20..0x7f).contains(&b) && b != b'"' && b != b'\\')
    {
        format!("filename=\"{name}\"")
    } else {
        format!("filename*=UTF-8''{}", super::encoding::url_encode(&name))
    }
}

/// A MIME type guessed from a file name's extension.
pub fn mime_for(filename: &str) -> &'static str {
    let ext = filename
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("txt" | "log") => "text/plain",
        Some("md") => "text/markdown",
        Some("csv") => "text/csv",
        Some("html" | "htm") => "text/html",
        Some("json") => "application/json",
        Some("xml") => "application/xml",
        Some("pdf") => "application/pdf",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("zip") => "application/zip",
        Some("doc") => "application/msword",
        Some("docx") => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        Some("xls") => "application/vnd.ms-excel",
        Some("xlsx") => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        Some("ppt") => "application/vnd.ms-powerpoint",
        Some("pptx") => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        _ => "application/octet-stream",
    }
}

/// `s` as an RFC 2047 encoded-word if it isn't plain ASCII.
/// Split into chunks of at most 45 bytes (60 base64 characters, keeping
/// each word within RFC 2047's 75-character limit), never inside a
/// character; decoders join adjacent encoded-words.
fn encode_word(s: &str) -> String {
    if s.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        return s.to_string();
    }
    let mut words = Vec::new();
    let mut chunk = String::new();
    for c in s.chars() {
        if chunk.len() + c.len_utf8() > 45 {
            words.push(format!("=?UTF-8?B?{}?=", b64_std_encode(chunk.as_bytes())));
            chunk.clear();
        }
        chunk.push(c);
    }
    if !chunk.is_empty() {
        words.push(format!("=?UTF-8?B?{}?=", b64_std_encode(chunk.as_bytes())));
    }
    words.join(" ")
}

/// An address for To/Cc/Bcc: a non-ASCII display name (`Mark Karpelès
/// <mark@x>`) becomes an encoded-word, as raw UTF-8 in headers gets
/// garbled by mail systems.
fn encode_address(addr: &str) -> String {
    let addr = addr.trim();
    match addr.rfind('<') {
        Some(i) if addr.ends_with('>') => {
            let name = addr[..i].trim().trim_matches('"').trim();
            let email = &addr[i..];
            if name.is_empty() {
                email.to_string()
            } else if name.is_ascii() {
                addr.to_string()
            } else {
                format!("{} {email}", encode_word(name))
            }
        }
        _ => addr.to_string(),
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
            bcc: &[],
            attachments: &[],
            subject: "Café",
            body: "line1\nline2",
            in_reply_to: Some("<a@x>"),
            references: Some("<z@x>"),
        })
        .unwrap();
        let msg = String::from_utf8(msg).unwrap();
        assert!(
            msg.starts_with("Subject: =?UTF-8?B?Q2Fmw6k=?=\r\nTo: Bob <bob@x.com>\r\n"),
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
            bcc: &[],
            attachments: &[],
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
            bcc: &[],
            attachments: &[],
            subject: "a\nb",
            body: "",
            in_reply_to: None,
            references: None,
        };
        assert!(build_message(&draft).is_err());
    }

    #[test]
    fn attachments_make_a_multipart_message() {
        let to = vec!["bob@x.com".to_string()];
        let bcc = vec!["eve@x.com".to_string()];
        let attachments = [
            Attachment {
                filename: "../notes.txt".into(),
                mime_type: mime_for("notes.txt").into(),
                data: b"hi".to_vec(),
            },
            Attachment {
                filename: "résumé.pdf".into(),
                mime_type: "x\r\nBcc: z".into(),
                data: vec![0, 1, 2],
            },
        ];
        let msg = build_message(&Draft {
            to: &to,
            cc: &[],
            bcc: &bcc,
            subject: "files",
            body: "see attached",
            attachments: &attachments,
            in_reply_to: None,
            references: None,
        })
        .unwrap();
        let msg = String::from_utf8(msg).unwrap();
        assert!(msg.contains("Bcc: eve@x.com\r\n"), "{msg}");
        let boundary = msg
            .split("boundary=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap();
        assert_eq!(
            msg.matches(&format!("--{boundary}\r\n")).count(),
            3,
            "{msg}"
        );
        assert!(msg.ends_with(&format!("--{boundary}--\r\n")));
        assert!(msg.contains("Content-Type: text/plain\r\nContent-Disposition: attachment; filename=\"notes.txt\""), "{msg}");
        // A bad MIME type can't inject headers; non-ASCII names use RFC 2231.
        assert!(msg.contains("Content-Type: application/octet-stream\r\nContent-Disposition: attachment; filename*=UTF-8''r%C3%A9sum%C3%A9.pdf"), "{msg}");
        assert!(msg.contains(&b64_std_encode(b"hi")));
    }

    #[test]
    fn helpers() {
        assert_eq!(reply_subject("hello"), "Re: hello");
        assert_eq!(reply_subject("RE: hello"), "RE: hello");
        assert_eq!(truncate("héllo", 2), ("hé".to_string(), true));
        assert_eq!(truncate("hé", 5), ("hé".to_string(), false));
    }

    #[test]
    fn non_ascii_headers_are_encoded() {
        assert_eq!(
            encode_address("Mark Karpelès <mark@klb.jp>"),
            format!(
                "=?UTF-8?B?{}?= <mark@klb.jp>",
                b64_std_encode("Mark Karpelès".as_bytes())
            )
        );
        assert_eq!(
            encode_address("\"Mark Karpelès\" <mark@klb.jp>"),
            encode_address("Mark Karpelès <mark@klb.jp>")
        );
        assert_eq!(encode_address("Bob <bob@x.com>"), "Bob <bob@x.com>");
        assert_eq!(encode_address("bob@x.com"), "bob@x.com");
        // Long subjects become several words, each within the limit, never
        // splitting a character.
        let long = "日本語の件名".repeat(10);
        let encoded = encode_word(&long);
        let words: Vec<&str> = encoded.split(' ').collect();
        assert!(words.len() > 1);
        assert!(words.iter().all(|w| w.len() <= 75), "{encoded}");
        let decoded: String = words
            .iter()
            .map(|w| {
                let b64 = w.trim_start_matches("=?UTF-8?B?").trim_end_matches("?=");
                String::from_utf8(crate::google::encoding::b64_std_decode(b64).unwrap()).unwrap()
            })
            .collect();
        assert_eq!(decoded, long);
    }
}
