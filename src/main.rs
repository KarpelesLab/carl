//! Carl — a multipurpose MCP helper that gives AI agents hands.
//!
//! `carl` is what an MCP client launches: a thin shim relaying stdio to a
//! per-user daemon (`carl daemon`), which it starts on demand. The daemon runs
//! the actual server, so every agent on the machine shares one Carl.
//! `carl standalone` serves MCP over stdio in-process, without a daemon.
//!
//! The protocol owns stdout, so all logging goes to stderr (configurable via
//! the `RUST_LOG` environment variable, e.g. `RUST_LOG=debug`).

mod agents;
mod areas;
mod config;
mod daemon;
mod error;
mod features;
mod google;
mod ipc;
mod server;
mod shim;
mod update;

use std::process::ExitCode;

use anyhow::Result;
use rmcp::{ServiceExt, transport::stdio};
use tracing_subscriber::EnvFilter;

use crate::{config::Config, server::Carl};

fn main() -> ExitCode {
    // Log to stderr — stdout is reserved for the MCP wire protocol.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let config = Config::from_env();
    let result = match std::env::args().nth(1).as_deref() {
        None => shim::run(config),
        Some("daemon") => daemon::run(config),
        Some("standalone") => standalone(config),
        Some("--version" | "-V" | "version") => {
            // Only when run by hand: never in MCP mode, where stdout is the wire.
            let git = env!("RSUPD_GIT_TAG");
            if git.is_empty() {
                println!("carl {}", env!("CARGO_PKG_VERSION"));
            } else {
                println!("carl {} ({git})", env!("CARGO_PKG_VERSION"));
            }
            return ExitCode::SUCCESS;
        }
        Some(other) => {
            tracing::error!(
                "unknown command {other:?}; usage: carl [daemon | standalone | --version]"
            );
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = format!("{e:#}"), "carl failed");
            ExitCode::FAILURE
        }
    }
}

/// The daemon's updater (see [`update`]). The fingerprint is the trust anchor:
/// a hash of Carl's release signing key (`rsupd id export --project carl`).
/// Only releases signed by that key are installed, wherever they come from.
#[cfg(feature = "auto-update")]
fn rsupd_updater() -> rsupd::Result<rsupd::Updater> {
    rsupd::Updater::builder(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
        .fingerprint_hex("d634418a64960aac1580b5f50c0adf5f9b88fafb192f08ea52442ac9ba9f1c9a")
        .channel(env!("RSUPD_CHANNEL"))
        .git_tag(env!("RSUPD_GIT_TAG"))
        .date_tag(rsupd::date_tag_from_unix(env!("RSUPD_BUILD_UNIX")))
        // The daemon restarts by exiting and letting its shims start the new
        // build, not by rsupd re-executing it.
        .auto_restart(false)
        .build()
}

/// Serve MCP over stdio in this process, bypassing the daemon.
fn standalone(config: Config) -> Result<()> {
    tracing::info!(data_dir = %config.data_dir.display(), "starting carl (standalone)");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let service = Carl::new(config)
            .serve(stdio())
            .await
            .inspect_err(|e| tracing::error!(error = ?e, "failed to start MCP server"))?;
        service.waiting().await?;
        Ok(())
    })
}
