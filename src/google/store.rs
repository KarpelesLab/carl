//! On-disk state of the Google feature, under `<data dir>/google/` (0700):
//!
//! - `client.json`: the OAuth client the user configured (bring-your-own).
//! - `accounts.json`: linked accounts and their refresh tokens.
//!
//! Files are 0600 and replaced atomically. Refresh tokens grant lasting access
//! to the account, so treat them like the wallet keys: they move into the
//! encrypted keystore once there is one.

use std::{
    fs,
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::Mutex,
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// A Google OAuth client ("Desktop app" type).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Client {
    pub client_id: String,
    pub client_secret: String,
}

/// A linked Google account.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub email: String,
    pub refresh_token: String,
    /// The client that issued `refresh_token`; only it can refresh it.
    pub client_id: String,
    /// Scopes granted so far (linking again adds to them).
    pub scopes: Vec<String>,
    /// Unix time of the last link.
    pub linked_at: u64,
}

pub struct Store {
    dir: PathBuf,
    /// Serializes read-modify-write of `accounts.json`.
    lock: Mutex<()>,
}

impl Store {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            dir: data_dir.join("google"),
            lock: Mutex::new(()),
        }
    }

    pub fn client(&self) -> Result<Option<Client>> {
        self.read("client.json")
    }

    pub fn set_client(&self, client: &Client) -> Result<()> {
        self.write("client.json", client)
    }

    pub fn accounts(&self) -> Result<Vec<Account>> {
        Ok(self.read("accounts.json")?.unwrap_or_default())
    }

    pub fn account(&self, email: &str) -> Result<Option<Account>> {
        Ok(self
            .accounts()?
            .into_iter()
            .find(|a| a.email.eq_ignore_ascii_case(email)))
    }

    /// Add or replace an account, keeping scopes granted by earlier links.
    pub fn upsert_account(&self, mut account: Account) -> Result<()> {
        let _guard = self.lock.lock().unwrap();
        let mut accounts = self.accounts()?;
        if let Some(pos) = accounts
            .iter()
            .position(|a| a.email.eq_ignore_ascii_case(&account.email))
        {
            let old = accounts.remove(pos);
            // Earlier grants survive only if the same client still holds them.
            if old.client_id == account.client_id {
                for scope in old.scopes {
                    if !account.scopes.contains(&scope) {
                        account.scopes.push(scope);
                    }
                }
            }
        }
        account.scopes.sort();
        accounts.push(account);
        accounts.sort_by(|a, b| a.email.cmp(&b.email));
        self.write("accounts.json", &accounts)
    }

    pub fn remove_account(&self, email: &str) -> Result<Option<Account>> {
        let _guard = self.lock.lock().unwrap();
        let mut accounts = self.accounts()?;
        let Some(pos) = accounts
            .iter()
            .position(|a| a.email.eq_ignore_ascii_case(email))
        else {
            return Ok(None);
        };
        let removed = accounts.remove(pos);
        self.write("accounts.json", &accounts)?;
        Ok(Some(removed))
    }

    fn read<T: DeserializeOwned>(&self, name: &str) -> Result<Option<T>> {
        let path = self.dir.join(name);
        match fs::read(&path) {
            Ok(bytes) => Ok(Some(
                serde_json::from_slice(&bytes)
                    .with_context(|| format!("parsing {}", path.display()))?,
            )),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Write `value` to `name` (mode 0600) via a temporary file and a rename,
    /// so a crash never leaves a half-written token file.
    fn write<T: Serialize>(&self, name: &str, value: &T) -> Result<()> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)?;
        let path = self.dir.join(name);
        let tmp = self.dir.join(format!(".{name}.tmp"));
        let mut file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(&serde_json::to_vec_pretty(value)?)?;
        file.sync_all()?;
        fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn account(email: &str, client: &str, scopes: &[&str]) -> Account {
        Account {
            email: email.into(),
            refresh_token: "rt".into(),
            client_id: client.into(),
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
            linked_at: 0,
        }
    }

    #[test]
    fn relinking_merges_scopes_from_the_same_client_only() {
        let dir = std::env::temp_dir().join(format!("carl-store-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let store = Store::new(&dir);

        store
            .upsert_account(account("a@x.com", "c1", &["mail"]))
            .unwrap();
        store
            .upsert_account(account("A@x.com", "c1", &["drive"]))
            .unwrap();
        let a = store.account("a@x.com").unwrap().unwrap();
        assert_eq!(a.scopes, ["drive", "mail"]);

        store
            .upsert_account(account("a@x.com", "c2", &["calendar"]))
            .unwrap();
        let a = store.account("a@x.com").unwrap().unwrap();
        assert_eq!(a.scopes, ["calendar"]);
        assert_eq!(store.accounts().unwrap().len(), 1);

        let meta = fs::metadata(dir.join("google/accounts.json")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        fs::remove_dir_all(&dir).unwrap();
    }
}
