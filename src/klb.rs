//! KarpelesLab / AtOnline platform access (`hub.atonline.com`) through the
//! [`klbfw`] client: login, REST calls, and uploads.
//!
//! Login works like `shells-support/login.js`: an OAuth2 polltoken flow (the
//! user opens a URL, Carl polls until they approve), and the token is shared
//! with those tools in `~/.config/atonline/auth-<profile>.json`, in the same
//! format. klbfw renews expired access tokens; Carl saves each renewal back
//! to that file.

use std::{
    fs,
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use klbfw::{Client, Config, RestError, Token};
use serde_json::{Map, Value, json};

pub const CLIENT_ID: &str = "oaap-p6rktp-uzaf-adle-djqw-g27ghobe";
const HOST: &str = "hub.atonline.com";

/// How long a login started with `klb_login` waits for the user.
const LOGIN_TTL: Duration = Duration::from_secs(10 * 60);

pub struct Klb {
    /// `~/.config/atonline/auth-<profile>.json`.
    path: PathBuf,
    /// The token file's JSON, as last read or written.
    token: Mutex<Option<Map<String, Value>>>,
    /// A login in progress: when it gives up.
    pending: Mutex<Option<Instant>>,
}

impl Klb {
    /// Uses `$CARL_KLB_PROFILE` (default `default`), like login.js's
    /// `$SHELLS_PROFILE`.
    pub fn new() -> Self {
        let profile = std::env::var("CARL_KLB_PROFILE").unwrap_or_else(|_| "default".into());
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        Self::at(
            home.join(".config/atonline")
                .join(format!("auth-{profile}.json")),
        )
    }

    pub fn at(path: PathBuf) -> Self {
        Self {
            path,
            token: Mutex::new(None),
            pending: Mutex::new(None),
        }
    }

    /// The stored token, read from disk on first use (and after logins made
    /// by other tools, when we have none yet).
    fn stored(&self) -> Option<Map<String, Value>> {
        let mut token = self.token.lock().unwrap();
        if token.is_none() {
            *token = fs::read(&self.path)
                .ok()
                .and_then(|b| serde_json::from_slice::<Map<String, Value>>(&b).ok())
                .filter(|t| t.get("access_token").is_some_and(Value::is_string));
        }
        token.clone()
    }

    fn save(&self, token: Map<String, Value>) -> Result<()> {
        let dir = self
            .path
            .parent()
            .ok_or_else(|| anyhow!("bad token path"))?;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        let tmp = self.path.with_extension("json.tmp");
        let mut file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(&serde_json::to_vec_pretty(&token)?)?;
        file.sync_all()?;
        fs::rename(&tmp, &self.path)?;
        *self.token.lock().unwrap() = Some(token);
        Ok(())
    }

    pub fn logged_in(&self) -> bool {
        self.stored().is_some()
    }

    pub fn login_pending(&self) -> bool {
        self.pending
            .lock()
            .unwrap()
            .is_some_and(|until| until > Instant::now())
    }

    /// A client without credentials (for the login flow itself).
    fn anonymous() -> Client {
        Client::with_config(Config::for_host(HOST))
    }

    /// Run `f` with a client carrying the stored token. klbfw renews an
    /// expired access token by itself; the renewed token is merged into the
    /// stored one (keeping fields klbfw doesn't know, like `id_token`) and
    /// saved, so the next call and the shells-support tools get it too.
    pub fn with_client<T>(self: &Arc<Self>, f: impl Fn(&Client) -> klbfw::Result<T>) -> Result<T> {
        let token = self
            .stored()
            .ok_or_else(|| anyhow!("not logged in to the platform; use klb_login first"))?;
        let field = |k: &str| {
            token
                .get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let client_id = token
            .get("ClientID")
            .and_then(Value::as_str)
            .unwrap_or(CLIENT_ID)
            .to_string();
        let expires = token
            .get("expires_in")
            .and_then(Value::as_i64)
            .unwrap_or(3600) as i32;
        let klb = self.clone();
        let client = Self::anonymous()
            .with_token(Token::new(
                field("access_token"),
                field("refresh_token"),
                client_id,
                expires,
            ))
            .on_token_renewed(move |renewed| klb.renewed(renewed));
        f(&client).map_err(describe)
    }

    /// Save a token klbfw renewed.
    fn renewed(&self, renewed: &Token) {
        let mut merged = self.stored().unwrap_or_default();
        merged.insert("access_token".into(), json!(renewed.access_token));
        merged.insert("refresh_token".into(), json!(renewed.refresh_token));
        merged.insert("token_type".into(), json!(renewed.token_type));
        merged.insert("expires_in".into(), json!(renewed.expires_in));
        match self.save(merged) {
            Ok(()) => tracing::info!("platform token renewed"),
            Err(e) => tracing::warn!(error = %e, "saving the renewed platform token failed"),
        }
    }

    /// Start a login: returns the URL for the user, and polls for their
    /// approval in the background.
    pub fn start_login(self: &Arc<Self>) -> Result<String> {
        let created = Self::anonymous()
            .do_request(
                &format!("OAuth2/App/{CLIENT_ID}:token_create"),
                "POST",
                json!({}),
            )
            .map_err(describe)?;
        let data = created.data.unwrap_or_default();
        let polltoken = data["polltoken"]
            .as_str()
            .ok_or_else(|| anyhow!("the platform returned no polltoken"))?
            .to_string();
        let url = data["xox"].as_str().map(str::to_string).unwrap_or_else(|| {
            let redirect = crate::google::encoding::url_encode(&format!("polltoken:{polltoken}"));
            format!(
                "https://{HOST}/_rest/OAuth2:auth?response_type=code&client_id={CLIENT_ID}\
                 &redirect_uri={redirect}&scope=profile"
            )
        });
        let until = Instant::now() + LOGIN_TTL;
        *self.pending.lock().unwrap() = Some(until);
        let klb = self.clone();
        thread::spawn(move || {
            if let Err(e) = klb.poll_login(&polltoken, until) {
                tracing::warn!(error = format!("{e:#}"), "platform login failed");
            }
            *klb.pending.lock().unwrap() = None;
        });
        Ok(url)
    }

    fn poll_login(&self, polltoken: &str, until: Instant) -> Result<()> {
        let poll = format!("OAuth2/App/{CLIENT_ID}:token_poll");
        while Instant::now() < until {
            let resp = Self::anonymous()
                .do_request(&poll, "POST", json!({ "polltoken": polltoken }))
                .map_err(describe)?;
            let data = resp.data.unwrap_or_default();
            let Some(code) = data["response"]["code"].as_str() else {
                thread::sleep(Duration::from_secs(1));
                continue;
            };
            let token = Self::anonymous()
                .do_request(
                    "OAuth2:token",
                    "POST",
                    json!({
                        "client_id": CLIENT_ID,
                        "grant_type": "authorization_code",
                        "code": code,
                        "noraw": true,
                    }),
                )
                .map_err(describe)?;
            let mut token = token
                .data
                .and_then(|d| d.as_object().cloned())
                .filter(|t| t.get("access_token").is_some_and(Value::is_string))
                .ok_or_else(|| anyhow!("the platform returned no access token"))?;
            token.insert("ClientID".into(), json!(CLIENT_ID));
            self.save(token)?;
            tracing::info!("platform login completed");
            return Ok(());
        }
        bail!("login not approved in time")
    }
}

/// A readable klbfw error.
pub fn describe(e: RestError) -> anyhow::Error {
    match e {
        RestError::NoRefreshToken | RestError::LoginRequired => {
            anyhow!("the platform login expired; use klb_login again")
        }
        other => anyhow!("{other}"),
    }
}

/// Resolve `path` for upload: it must be a regular file inside one of
/// `roots` once symlinks are resolved. Carl only reads files an agent could
/// already read in its own working directory (or Carl's downloads), so it
/// can't be used to exfiltrate, say, `~/.ssh`.
pub fn confine(path: &Path, roots: &[PathBuf]) -> Result<PathBuf> {
    let real = fs::canonicalize(path).with_context(|| format!("can't open {}", path.display()))?;
    if !real.is_file() {
        bail!("{} is not a regular file", path.display());
    }
    let allowed = roots
        .iter()
        .filter_map(|r| fs::canonicalize(r).ok())
        .any(|root| real.starts_with(&root));
    if !allowed {
        bail!(
            "{} is outside the directories Carl may upload from (your working directory, \
             and ~/Downloads/carl); pass small files as content instead",
            path.display()
        );
    }
    Ok(real)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uploads_are_confined_to_allowed_roots() {
        let base = std::env::temp_dir().join(format!("carl-confine-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let (root, outside) = (base.join("project"), base.join("secret"));
        fs::create_dir_all(root.join("dist")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(root.join("dist/app.zip"), b"ok").unwrap();
        fs::write(outside.join("id_rsa"), b"no").unwrap();
        std::os::unix::fs::symlink(outside.join("id_rsa"), root.join("sneaky")).unwrap();
        let roots = [root.clone()];

        assert!(confine(&root.join("dist/app.zip"), &roots).is_ok());
        assert!(confine(&root.join("dist/../dist/app.zip"), &roots).is_ok());
        assert!(confine(&outside.join("id_rsa"), &roots).is_err());
        assert!(
            confine(&root.join("sneaky"), &roots).is_err(),
            "symlink escape"
        );
        assert!(confine(&root.join("../secret/id_rsa"), &roots).is_err());
        assert!(confine(&root.join("dist"), &roots).is_err(), "directory");
        assert!(confine(&root.join("missing"), &roots).is_err());
        fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn token_file_round_trip_keeps_unknown_fields() {
        let path =
            std::env::temp_dir().join(format!("carl-klb-{}/auth-test.json", std::process::id()));
        let klb = Klb::at(path.clone());
        assert!(!klb.logged_in());
        let mut token = Map::new();
        token.insert("access_token".into(), json!("a"));
        token.insert("refresh_token".into(), json!("r"));
        token.insert("id_token".into(), json!("keep me"));
        klb.save(token).unwrap();
        let again = Klb::at(path.clone());
        assert!(again.logged_in());
        assert_eq!(again.stored().unwrap()["id_token"], "keep me");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
