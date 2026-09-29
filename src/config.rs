//! Runtime configuration for Carl.
//!
//! Configuration is intentionally minimal at this stage. Values are resolved
//! from environment variables with sensible defaults so Carl can be launched by
//! an MCP client (over stdio) with zero required setup. As features land, each
//! feature module is expected to own its own typed config section here.

use std::path::PathBuf;

/// Top-level configuration shared across all features.
#[derive(Debug, Clone)]
pub struct Config {
    /// Directory where Carl persists state (wallet keystore, caches, etc.).
    ///
    /// Defaults to `$CARL_DATA_DIR`, falling back to `~/.carl`. This directory
    /// may hold sensitive material once the wallet feature is implemented, so it
    /// is created with restrictive permissions by the features that use it.
    pub data_dir: PathBuf,
}

impl Config {
    /// Build a [`Config`] from the process environment.
    pub fn from_env() -> Self {
        let data_dir = std::env::var_os("CARL_DATA_DIR")
            .map(PathBuf::from)
            .or_else(default_data_dir)
            .unwrap_or_else(|| PathBuf::from(".carl"));

        Self { data_dir }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::from_env()
    }
}

/// `~/.carl` if a home directory can be determined.
fn default_data_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".carl"))
}
