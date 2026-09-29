//! OAuth 2.0 for installed apps (authorization code + PKCE, loopback
//! redirect), as Google recommends for desktop clients. The device-code flow
//! is not an option: it doesn't allow Gmail or Drive scopes.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::encoding::{b64url_decode, b64url_encode, form_encode, url_encode};
use super::store::Client;

const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const REVOKE_URL: &str = "https://oauth2.googleapis.com/revoke";

/// Needed on every link, to learn which account was linked.
const IDENTITY_SCOPES: [&str; 2] = ["openid", "email"];

/// A part of the Google account Carl can be given access to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Area {
    /// Gmail: read, search, label, draft, send.
    Mail,
    /// Google Calendar.
    Calendar,
    /// Google Drive, including Docs, Sheets and Slides.
    Drive,
    /// Google Contacts (read-only).
    Contacts,
    /// Google Meet: create meetings, read past meetings and transcripts.
    Meet,
}

impl Area {
    pub const ALL: [Area; 5] = [
        Area::Mail,
        Area::Calendar,
        Area::Drive,
        Area::Contacts,
        Area::Meet,
    ];

    /// The OAuth scopes granting this area. Broader than what Carl's tools
    /// use today: Carl, not the token, is what limits the agent, and later
    /// tools won't need the user to link again.
    pub fn scopes(self) -> &'static [&'static str] {
        match self {
            Area::Mail => &["https://www.googleapis.com/auth/gmail.modify"],
            Area::Calendar => &["https://www.googleapis.com/auth/calendar"],
            Area::Drive => &["https://www.googleapis.com/auth/drive"],
            Area::Contacts => &["https://www.googleapis.com/auth/contacts.readonly"],
            Area::Meet => &[
                "https://www.googleapis.com/auth/meetings.space.created",
                "https://www.googleapis.com/auth/meetings.space.readonly",
            ],
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Area::Mail => "mail",
            Area::Calendar => "calendar",
            Area::Drive => "drive",
            Area::Contacts => "contacts",
            Area::Meet => "meet",
        }
    }

    /// Whether `granted` scopes cover this area.
    pub fn granted_by(self, granted: &[String]) -> bool {
        self.scopes().iter().all(|s| granted.iter().any(|g| g == s))
    }
}

/// The areas covered by a set of granted scopes.
pub fn areas_of(scopes: &[String]) -> Vec<&'static str> {
    Area::ALL
        .into_iter()
        .filter(|a| a.granted_by(scopes))
        .map(Area::name)
        .collect()
}

/// PKCE S256 challenge for `verifier`.
pub fn challenge(verifier: &str) -> String {
    b64url_encode(&purecrypto::hash::sha256(verifier.as_bytes()))
}

/// The URL the user opens to grant access.
pub fn auth_url(
    client_id: &str,
    redirect_uri: &str,
    areas: &[Area],
    state: &str,
    verifier: &str,
    login_hint: Option<&str>,
) -> String {
    let scopes: Vec<&str> = IDENTITY_SCOPES
        .into_iter()
        .chain(areas.iter().flat_map(|a| a.scopes().iter().copied()))
        .collect();
    let scope = scopes.join(" ");
    let challenge = challenge(verifier);
    let mut params = vec![
        ("client_id", client_id),
        ("redirect_uri", redirect_uri),
        ("response_type", "code"),
        ("scope", &scope),
        ("state", state),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        // A refresh token, every time (Google only sends one on consent).
        ("access_type", "offline"),
        ("prompt", "consent"),
        ("include_granted_scopes", "true"),
    ];
    if let Some(hint) = login_hint {
        params.push(("login_hint", hint));
    }
    format!("{AUTH_URL}?{}", form_encode(&params))
}

/// A token endpoint response.
#[derive(Debug, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    #[serde(default)]
    pub expires_in: u64,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub id_token: Option<String>,
}

/// Exchange an authorization code for tokens.
pub fn exchange_code(
    client: &Client,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<Tokens> {
    post_token(&[
        ("grant_type", "authorization_code"),
        ("code", code),
        ("code_verifier", verifier),
        ("redirect_uri", redirect_uri),
        ("client_id", &client.client_id),
        ("client_secret", &client.client_secret),
    ])
}

/// Get a fresh access token.
pub fn refresh(client: &Client, refresh_token: &str) -> Result<Tokens> {
    post_token(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", &client.client_id),
        ("client_secret", &client.client_secret),
    ])
}

/// Revoke a token (and with it the whole grant).
pub fn revoke(token: &str) -> Result<()> {
    let resp = rsurl::Request::new("POST", REVOKE_URL)?
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(format!("token={}", url_encode(token)))
        .max_time(Duration::from_secs(30))
        .send()?;
    match resp.status {
        200 => Ok(()),
        // Already invalid: as good as revoked.
        400 => Ok(()),
        status => bail!("revoking the token failed (HTTP {status})"),
    }
}

/// The email address in an ID token. The token came straight from Google's
/// token endpoint over TLS, so its signature needs no checking here.
pub fn email_from_id_token(id_token: &str) -> Result<String> {
    let payload = id_token
        .split('.')
        .nth(1)
        .ok_or_else(|| anyhow!("malformed id_token"))?;
    let claims: Value = serde_json::from_slice(&b64url_decode(payload)?)?;
    claims["email"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("id_token has no email"))
}

fn post_token(params: &[(&str, &str)]) -> Result<Tokens> {
    let resp = rsurl::Request::new("POST", TOKEN_URL)?
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(form_encode(params))
        .max_time(Duration::from_secs(30))
        .send()
        .context("contacting Google's token endpoint")?;
    let body: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
    if resp.status != 200 {
        let error = body["error"].as_str().unwrap_or("unknown_error");
        let description = body["error_description"].as_str().unwrap_or("");
        return Err(TokenError {
            error: error.to_string(),
            description: description.to_string(),
        }
        .into());
    }
    Ok(serde_json::from_value(body)?)
}

/// An OAuth error from the token endpoint, e.g. `invalid_grant` once the user
/// revoked access.
#[derive(Debug, thiserror::Error)]
#[error("Google refused: {error} {description}")]
pub struct TokenError {
    pub error: String,
    pub description: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::google::encoding::parse_query;

    #[test]
    fn pkce_challenge_matches_rfc7636() {
        // RFC 7636 appendix B.
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn auth_url_carries_scopes_and_pkce() {
        let url = auth_url(
            "cid",
            "http://127.0.0.1:1234",
            &[Area::Mail, Area::Drive],
            "st",
            "ver",
            Some("a@b.c"),
        );
        let (base, query) = url.split_once('?').unwrap();
        assert_eq!(base, AUTH_URL);
        let q = parse_query(query);
        assert_eq!(
            q["scope"],
            "openid email https://www.googleapis.com/auth/gmail.modify https://www.googleapis.com/auth/drive"
        );
        assert_eq!(q["redirect_uri"], "http://127.0.0.1:1234");
        assert_eq!(q["code_challenge"], challenge("ver"));
        assert_eq!(q["access_type"], "offline");
        assert_eq!(q["login_hint"], "a@b.c");
    }

    #[test]
    fn email_from_unsigned_id_token() {
        let payload = b64url_encode(br#"{"email":"a@b.c","sub":"1"}"#);
        assert_eq!(
            email_from_id_token(&format!("h.{payload}.s")).unwrap(),
            "a@b.c"
        );
        assert!(email_from_id_token("garbage").is_err());
    }

    #[test]
    fn areas_from_granted_scopes() {
        let scopes = vec![
            "openid".to_string(),
            Area::Drive.scopes()[0].to_string(),
            Area::Mail.scopes()[0].to_string(),
            Area::Meet.scopes()[0].to_string(),
        ];
        assert_eq!(areas_of(&scopes), ["mail", "drive"]);
    }
}
