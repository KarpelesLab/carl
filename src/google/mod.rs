//! Google account access: linking (OAuth), token storage and refresh, and an
//! authenticated REST helper for the `google_*` tools.
//!
//! The daemon holds one [`Google`] shared by every agent, so an account linked
//! by one agent is usable by all. Everything here blocks (rsurl); tool
//! handlers call it through `spawn_blocking`.

pub mod encoding;
pub mod mail;
pub mod oauth;
pub mod store;

use std::{
    collections::{HashMap, HashSet},
    io::{BufRead, BufReader, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use encoding::{parse_query, random_token, unix_now};
pub use oauth::Area;
pub use store::Owner;
use store::{Account, Client, Store};

/// A shared OAuth client compiled into Carl, used when the user configured
/// none. Empty until Google verifies one: Gmail and Drive are restricted
/// scopes, which need verification and a yearly security assessment before a
/// shared client can serve more than 100 users without warnings.
const BUILTIN_CLIENT: Option<(&str, &str)> = None;

/// How long a link started by `google_link` stays valid.
const LINK_TTL: Duration = Duration::from_secs(10 * 60);

/// Refresh access tokens this long before they expire.
const TOKEN_MARGIN: Duration = Duration::from_secs(60);

/// Waits before retrying a rate-limited call, as Google asks.
const RATE_LIMIT_BACKOFF: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(3)];

/// Per-request limit for Google API calls.
const API_TIMEOUT: Duration = Duration::from_secs(60);

pub struct Google {
    store: Store,
    /// Access tokens by account email.
    tokens: Mutex<HashMap<String, (String, Instant)>>,
    /// Links awaiting the user's consent, by OAuth `state`.
    pending: Mutex<HashMap<String, PendingLink>>,
    /// Accounts whose contacts search cache has been warmed up.
    pub(crate) contacts_warmed: Mutex<HashSet<String>>,
}

struct PendingLink {
    client: Client,
    verifier: String,
    redirect_uri: String,
    expires: Instant,
}

/// What `google_link` hands back to the agent.
pub struct LinkStart {
    pub url: String,
    pub redirect_uri: String,
}

/// Where the OAuth client in use comes from.
pub enum ClientSource {
    Configured(Client),
    Builtin(Client),
}

impl Google {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            store: Store::new(data_dir),
            tokens: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            contacts_warmed: Mutex::new(HashSet::new()),
        }
    }

    /// The OAuth client for new links: the configured one, else the built-in.
    pub fn client(&self) -> Result<Option<ClientSource>> {
        if let Some(client) = self.store.client()? {
            return Ok(Some(ClientSource::Configured(client)));
        }
        Ok(BUILTIN_CLIENT.map(|(id, secret)| {
            ClientSource::Builtin(Client {
                client_id: id.into(),
                client_secret: secret.into(),
            })
        }))
    }

    /// Configure the user's own OAuth client.
    pub fn set_client(&self, client_id: &str, client_secret: &str) -> Result<()> {
        if !client_id.ends_with(".apps.googleusercontent.com") {
            bail!("that doesn't look like a Google OAuth client ID (…apps.googleusercontent.com)");
        }
        self.store.set_client(&Client {
            client_id: client_id.trim().into(),
            client_secret: client_secret.trim().into(),
        })
    }

    pub fn accounts(&self) -> Result<Vec<Value>> {
        Ok(self
            .store
            .accounts()?
            .into_iter()
            .map(|a| {
                json!({
                    "email": a.email,
                    "owner": a.owner.as_str(),
                    "areas": oauth::areas_of(&a.scopes),
                    "linked_at": encoding::rfc3339(a.linked_at),
                })
            })
            .collect())
    }

    pub fn pending_links(&self) -> usize {
        let mut pending = self.pending.lock().unwrap();
        pending.retain(|_, p| p.expires > Instant::now());
        pending.len()
    }

    /// Start linking an account: returns the consent URL and serves the
    /// loopback redirect in the background until the link completes or
    /// expires.
    pub fn start_link(
        self: &Arc<Self>,
        areas: &[Area],
        login_hint: Option<&str>,
    ) -> Result<LinkStart> {
        let client = match self.client()? {
            Some(ClientSource::Configured(c) | ClientSource::Builtin(c)) => c,
            None => bail!(
                "no Google OAuth client configured. Have the user create one \
                 (Google Cloud console → APIs & Services → Credentials → Create \
                 OAuth client ID → Desktop app; enable the Gmail, Calendar, Drive, \
                 Sheets, Slides, Meet REST and People APIs), then pass its JSON file to google_set_client"
            ),
        };
        let listener = TcpListener::bind("127.0.0.1:0").context("opening a loopback port")?;
        let redirect_uri = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
        let state = random_token(16);
        let verifier = random_token(32);
        let url = oauth::auth_url(
            &client.client_id,
            &redirect_uri,
            areas,
            &state,
            &verifier,
            login_hint,
        );
        self.pending.lock().unwrap().insert(
            state.clone(),
            PendingLink {
                client,
                verifier,
                redirect_uri: redirect_uri.clone(),
                expires: Instant::now() + LINK_TTL,
            },
        );

        let google = self.clone();
        thread::spawn(move || google.serve_redirect(listener, &state));
        Ok(LinkStart { url, redirect_uri })
    }

    /// Finish a link from the address the browser was redirected to, for when
    /// the loopback page can't be reached (Carl on another machine).
    pub fn complete_link(&self, redirect_url: &str) -> Result<String> {
        let query = redirect_url
            .split_once('?')
            .map(|(_, q)| q)
            .unwrap_or(redirect_url);
        let query = query.split('#').next().unwrap_or_default();
        self.finish_link(&parse_query(query))
    }

    /// Set whose account `email` is (human-only: called from the CLI).
    pub fn set_owner(&self, email: &str, owner: Owner) -> Result<bool> {
        self.store.set_owner(email, owner)
    }

    /// Unlink an account, revoking Carl's access at Google.
    pub fn unlink(&self, email: &str) -> Result<bool> {
        let Some(account) = self.store.account(email)? else {
            return Ok(false);
        };
        // Revoke first: if that fails, keep the account so it can be retried.
        oauth::revoke(&account.refresh_token)?;
        self.store.remove_account(&account.email)?;
        self.tokens.lock().unwrap().remove(&account.email);
        Ok(true)
    }

    /// Whose account `email` is.
    pub fn owner(&self, email: &str) -> Result<Owner> {
        Ok(self
            .store
            .account(email)?
            .ok_or_else(|| anyhow!("{email} is not linked"))?
            .owner)
    }

    /// Until the approvals layer exists, actions that reach other people
    /// (sending, inviting, sharing…) are only allowed from accounts dedicated
    /// to Carl: from the user's own account they would speak for the user.
    pub fn require_carl_owned(&self, email: &str, action: &str) -> Result<()> {
        if self.owner(email)? == Owner::Carl {
            return Ok(());
        }
        bail!(
            "{action} from {email} would act in the user's name, which needs the approvals \
             layer (not available yet). For now only accounts dedicated to Carl (owner \"carl\" \
             in google_accounts) can do this; on the user's account, prepare it for them \
             instead (e.g. a draft they send themselves)."
        )
    }

    /// Save downloaded content as `filename` in Carl's downloads directory
    /// (`~/Downloads/carl`, or `$CARL_DOWNLOADS_DIR`), never overwriting.
    /// Carl only ever writes files there: an agent must not be able to have
    /// it write anywhere else.
    pub fn save_download(&self, filename: &str, data: &[u8]) -> Result<std::path::PathBuf> {
        let dir = std::env::var_os("CARL_DOWNLOADS_DIR")
            .map(std::path::PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| Path::new(&h).join("Downloads/carl")))
            .ok_or_else(|| anyhow!("no home directory to download into"))?;
        save_in(&dir, filename, data)
    }

    /// Resolve which linked account a tool call is for, and check it granted
    /// `area`.
    pub fn account_for(&self, requested: Option<&str>, area: Area) -> Result<String> {
        let accounts = self.store.accounts()?;
        let account = match requested {
            Some(email) => accounts
                .iter()
                .find(|a| a.email.eq_ignore_ascii_case(email.trim()))
                .ok_or_else(|| {
                    anyhow!(
                        "{email} is not linked (linked: {}); use google_link",
                        list(&accounts)
                    )
                })?,
            None => match accounts.as_slice() {
                [] => bail!("no Google account is linked yet; use google_link first"),
                [only] => only,
                _ => bail!(
                    "several Google accounts are linked ({}); pass `account`",
                    list(&accounts)
                ),
            },
        };
        if !area.granted_by(&account.scopes) {
            bail!(
                "{} hasn't granted Carl {} access; call google_link with areas [\"{}\"] \
                 (and login_hint \"{}\") to add it",
                account.email,
                area.name(),
                area.name(),
                account.email
            );
        }
        Ok(account.email.clone())
    }

    /// Call a Google API as `account`, returning the parsed JSON response.
    pub fn api(
        &self,
        account: &str,
        method: &str,
        url: &str,
        body: Option<Value>,
    ) -> Result<Value> {
        let body = body.map(|b| ("application/json".to_string(), b.to_string().into_bytes()));
        let resp = self.send(account, method, url, body)?;
        if resp.body.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&resp.body).context("parsing Google's response")
    }

    /// Call a Google API as `account` with a raw body, returning raw bytes.
    pub fn api_raw(
        &self,
        account: &str,
        method: &str,
        url: &str,
        body: Option<(String, Vec<u8>)>,
    ) -> Result<Vec<u8>> {
        Ok(self.send(account, method, url, body)?.body)
    }

    fn send(
        &self,
        account: &str,
        method: &str,
        url: &str,
        body: Option<(String, Vec<u8>)>,
    ) -> Result<rsurl::Response> {
        let mut refreshed = false;
        let mut backoff = RATE_LIMIT_BACKOFF.iter();
        loop {
            let token = self.access_token(account)?;
            let mut req = rsurl::Request::new(method, url)?
                .header("Authorization", &format!("Bearer {token}"))
                .max_time(API_TIMEOUT);
            if let Some((content_type, data)) = &body {
                req = req.header("Content-Type", content_type).body(data.clone());
            } else if method != "GET" {
                req = req.header("Content-Length", "0");
            }
            let resp = req.send().context("contacting Google")?;
            if resp.status == 401 && !refreshed {
                // Revoked early, or our clock is off: refresh once and retry.
                self.tokens.lock().unwrap().remove(account);
                refreshed = true;
                continue;
            }
            if is_rate_limited(&resp)
                && let Some(wait) = backoff.next()
            {
                tracing::debug!(account, ?wait, "rate limited by Google, retrying");
                std::thread::sleep(*wait);
                continue;
            }
            if !(200..300).contains(&resp.status) {
                return Err(api_error(&resp));
            }
            return Ok(resp);
        }
    }

    fn access_token(&self, email: &str) -> Result<String> {
        if let Some((token, expires)) = self.tokens.lock().unwrap().get(email)
            && *expires > Instant::now() + TOKEN_MARGIN
        {
            return Ok(token.clone());
        }
        let account = self
            .store
            .account(email)?
            .ok_or_else(|| anyhow!("{email} is not linked"))?;
        let client = self.client_for(&account)?;
        let tokens =
            oauth::refresh(&client, &account.refresh_token).map_err(|e| match e
                .downcast_ref::<oauth::TokenError>()
            {
                Some(t) if t.error == "invalid_grant" => anyhow!(
                    "Google access for {email} was revoked or has expired; \
                     link it again with google_link"
                ),
                _ => e,
            })?;
        let expires = Instant::now() + Duration::from_secs(tokens.expires_in.max(60));
        self.tokens.lock().unwrap().insert(
            account.email.clone(),
            (tokens.access_token.clone(), expires),
        );
        Ok(tokens.access_token)
    }

    /// The client that issued `account`'s refresh token.
    fn client_for(&self, account: &Account) -> Result<Client> {
        match self.client()? {
            Some(ClientSource::Configured(c) | ClientSource::Builtin(c))
                if c.client_id == account.client_id =>
            {
                Ok(c)
            }
            _ => match BUILTIN_CLIENT {
                Some((id, secret)) if id == account.client_id => Ok(Client {
                    client_id: id.into(),
                    client_secret: secret.into(),
                }),
                _ => bail!(
                    "{} was linked with an OAuth client that is no longer configured; \
                     link it again with google_link",
                    account.email
                ),
            },
        }
    }

    /// Serve the loopback redirect for the link identified by `state`.
    fn serve_redirect(&self, listener: TcpListener, state: &str) {
        if listener.set_nonblocking(true).is_err() {
            return;
        }
        loop {
            let live = {
                let mut pending = self.pending.lock().unwrap();
                pending.retain(|_, p| p.expires > Instant::now());
                pending.contains_key(state)
            };
            if !live {
                return; // completed via google_link_complete, or expired
            }
            match listener.accept() {
                Ok((stream, _)) => {
                    if self.handle_redirect(stream) {
                        return;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(250));
                }
                Err(_) => thread::sleep(Duration::from_millis(250)),
            }
        }
    }

    /// Handle one request on the loopback port. `true` once the link is done
    /// (successfully or not).
    fn handle_redirect(&self, mut stream: TcpStream) -> bool {
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut request_line = String::new();
        if BufReader::new(&stream)
            .read_line(&mut request_line)
            .is_err()
        {
            return false;
        }
        // "GET /?state=…&code=… HTTP/1.1"
        let target = request_line.split_whitespace().nth(1).unwrap_or("/");
        let params = parse_query(target.split_once('?').map(|(_, q)| q).unwrap_or(""));
        if !params.contains_key("state") {
            respond(&mut stream, 404, "Not found.");
            return false; // e.g. /favicon.ico
        }
        match self.finish_link(&params) {
            Ok(email) => {
                respond(
                    &mut stream,
                    200,
                    &format!("{email} is now linked to Carl. You can close this tab."),
                );
                true
            }
            Err(e) => {
                respond(&mut stream, 400, &format!("Linking failed: {e:#}"));
                true
            }
        }
    }

    /// Complete a link from the redirect's query parameters.
    fn finish_link(&self, params: &HashMap<String, String>) -> Result<String> {
        let state = params
            .get("state")
            .ok_or_else(|| anyhow!("the address has no `state`; copy the full URL"))?;
        let pending = self
            .pending
            .lock()
            .unwrap()
            .remove(state)
            .filter(|p| p.expires > Instant::now())
            .ok_or_else(|| {
                anyhow!("this link attempt expired or was already used; call google_link again")
            })?;
        if let Some(error) = params.get("error") {
            bail!("Google returned `{error}` (access not granted)");
        }
        let code = params
            .get("code")
            .ok_or_else(|| anyhow!("the address has no `code`; copy the full URL"))?;

        let tokens = oauth::exchange_code(
            &pending.client,
            code,
            &pending.verifier,
            &pending.redirect_uri,
        )?;
        let refresh_token = tokens
            .refresh_token
            .ok_or_else(|| anyhow!("Google sent no refresh token"))?;
        let email = oauth::email_from_id_token(
            tokens
                .id_token
                .as_deref()
                .ok_or_else(|| anyhow!("Google sent no id_token"))?,
        )?;
        self.store.upsert_account(Account {
            email: email.clone(),
            // New accounts are the user's until a human says otherwise.
            owner: Owner::User,
            refresh_token,
            client_id: pending.client.client_id,
            scopes: tokens
                .scope
                .split_whitespace()
                .map(str::to_string)
                .collect(),
            linked_at: unix_now(),
        })?;
        let expires = Instant::now() + Duration::from_secs(tokens.expires_in.max(60));
        self.tokens
            .lock()
            .unwrap()
            .insert(email.clone(), (tokens.access_token, expires));
        tracing::info!(account = email, "google account linked");
        Ok(email)
    }
}

/// Save `data` as `filename` in `dir`, adding " (n)" rather than overwriting.
fn save_in(dir: &Path, filename: &str, data: &[u8]) -> Result<std::path::PathBuf> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::create_dir_all(dir)?;
    let name = safe_filename(filename);
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s.to_string(), format!(".{e}")),
        _ => (name.clone(), String::new()),
    };
    for n in 0..1000 {
        let candidate = if n == 0 {
            dir.join(&name)
        } else {
            dir.join(format!("{stem} ({n}){ext}"))
        };
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&candidate)
        {
            Ok(mut file) => {
                file.write_all(data)?;
                return Ok(candidate);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    bail!("too many files named {name} in {}", dir.display())
}

/// A file name safe to create: no directories, no hidden files, no control
/// characters, bounded length.
pub fn safe_filename(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = base
        .chars()
        .map(|c| if c.is_control() || c == ':' { '_' } else { c })
        .collect::<String>()
        .trim()
        .trim_start_matches('.')
        .chars()
        .take(200)
        .collect();
    if cleaned.is_empty() {
        "download".into()
    } else {
        cleaned
    }
}

fn list(accounts: &[Account]) -> String {
    if accounts.is_empty() {
        return "none".into();
    }
    accounts
        .iter()
        .map(|a| a.email.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Google's "slow down" answers: 429, or 403 with a rate-limit reason.
fn is_rate_limited(resp: &rsurl::Response) -> bool {
    if resp.status == 429 {
        return true;
    }
    resp.status == 403 && {
        let body = String::from_utf8_lossy(&resp.body);
        body.contains("rateLimitExceeded")
            || body.contains("userRateLimitExceeded")
            || body.contains("Rate Limit Exceeded")
    }
}

/// Turn a Google API error response into a readable error.
fn api_error(resp: &rsurl::Response) -> anyhow::Error {
    let body: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
    let message = body["error"]["message"]
        .as_str()
        .or_else(|| body["error_description"].as_str())
        .unwrap_or("no details");
    anyhow!("Google API error (HTTP {}): {message}", resp.status)
}

/// A minimal HTML page for the browser tab that completed the redirect.
fn respond(stream: &mut TcpStream, status: u16, message: &str) {
    let escaped = message
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>Carl</title>\
         <body style=\"font-family:sans-serif;margin:3em\"><p>{escaped}</p>"
    );
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Bad Request",
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn google() -> Arc<Google> {
        let dir = std::env::temp_dir().join(format!(
            "carl-google-{}-{:?}",
            std::process::id(),
            thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let google = Arc::new(Google::new(&dir));
        google
            .set_client("123.apps.googleusercontent.com", "secret")
            .unwrap();
        google
    }

    #[test]
    fn link_needs_a_client() {
        let dir = std::env::temp_dir().join(format!("carl-google-noclient-{}", std::process::id()));
        let google = Arc::new(Google::new(&dir));
        let err = google.start_link(&Area::ALL, None).err().unwrap();
        assert!(err.to_string().contains("google_set_client"), "{err}");
        assert!(google.set_client("nope", "x").is_err());
    }

    #[test]
    fn loopback_redirect_is_served_and_checked() {
        let google = google();
        let link = google.start_link(&[Area::Mail], None).unwrap();
        assert!(link.url.starts_with("https://accounts.google.com/"));
        assert_eq!(google.pending_links(), 1);

        let port = link.redirect_uri.rsplit(':').next().unwrap();
        let get = |path: &str| {
            let mut s = TcpStream::connect(format!("127.0.0.1:{port}")).unwrap();
            write!(s, "GET {path} HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).unwrap();
            out
        };
        // Stray requests don't end the link.
        assert!(get("/favicon.ico").starts_with("HTTP/1.1 404"));
        // A redirect with an unknown state is rejected (and ends this server).
        let resp = get("/?state=forged&code=x");
        assert!(resp.starts_with("HTTP/1.1 400"), "{resp}");
        assert!(resp.contains("expired or was already used"), "{resp}");
        // The genuine link is still pending.
        assert_eq!(google.pending_links(), 1);
    }

    #[test]
    fn denied_consent_is_reported() {
        let google = google();
        let link = google.start_link(&[Area::Drive], None).unwrap();
        let state = parse_query(link.url.split_once('?').unwrap().1)["state"].clone();
        let err = google
            .complete_link(&format!(
                "{}/?state={state}&error=access_denied",
                link.redirect_uri
            ))
            .unwrap_err();
        assert!(err.to_string().contains("access_denied"), "{err}");
        assert_eq!(google.pending_links(), 0);
    }

    #[test]
    fn account_selection() {
        let google = google();
        let err = google.account_for(None, Area::Mail).unwrap_err();
        assert!(err.to_string().contains("google_link"), "{err}");

        for email in ["a@x.com", "b@x.com"] {
            google
                .store
                .upsert_account(Account {
                    email: email.into(),
                    owner: Owner::User,
                    refresh_token: "rt".into(),
                    client_id: "123.apps.googleusercontent.com".into(),
                    scopes: vec![Area::Mail.scopes()[0].into()],
                    linked_at: 0,
                })
                .unwrap();
        }
        assert!(google.account_for(None, Area::Mail).is_err());
        assert_eq!(
            google.account_for(Some("B@X.com"), Area::Mail).unwrap(),
            "b@x.com"
        );
        let err = google
            .account_for(Some("a@x.com"), Area::Drive)
            .unwrap_err();
        assert!(err.to_string().contains("areas [\"drive\"]"), "{err}");
        assert!(google.account_for(Some("c@x.com"), Area::Mail).is_err());
    }

    #[test]
    fn filenames_are_confined() {
        assert_eq!(safe_filename("../../.bashrc"), "bashrc");
        assert_eq!(safe_filename("/etc/passwd"), "passwd");
        assert_eq!(safe_filename("a\\b\nc.pdf"), "b_c.pdf");
        assert_eq!(safe_filename(""), "download");
        assert_eq!(safe_filename("..."), "download");
    }

    #[test]
    fn downloads_never_overwrite() {
        let dir = std::env::temp_dir().join(format!("carl-dl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let a = save_in(&dir, "../report.pdf", b"one").unwrap();
        let b = save_in(&dir, "report.pdf", b"two").unwrap();
        assert_eq!(a, dir.join("report.pdf"));
        assert_eq!(b, dir.join("report (1).pdf"));
        assert_eq!(std::fs::read(&a).unwrap(), b"one");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_carl_owned_accounts_reach_others() {
        let g = google();
        for (email, owner) in [("me@x.com", Owner::User), ("carl@x.com", Owner::Carl)] {
            g.store
                .upsert_account(Account {
                    email: email.into(),
                    owner: Owner::User,
                    refresh_token: "rt".into(),
                    client_id: "123.apps.googleusercontent.com".into(),
                    scopes: vec![],
                    linked_at: 0,
                })
                .unwrap();
            g.store.set_owner(email, owner).unwrap();
        }
        assert!(g.require_carl_owned("carl@x.com", "Sending mail").is_ok());
        let err = g
            .require_carl_owned("me@x.com", "Sending mail")
            .unwrap_err()
            .to_string();
        assert!(err.contains("draft"), "{err}");
    }
}
