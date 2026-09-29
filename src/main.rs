//! Carl — a multipurpose MCP helper that gives AI agents hands.
//!
//! `carl` is what an MCP client launches: a thin shim relaying stdio to a
//! per-user daemon (`carl daemon`), which it starts on demand. The daemon runs
//! the actual server, so every agent on the machine shares one Carl.
//! `carl standalone` serves MCP over stdio in-process, without a daemon.
//!
//! The protocol owns stdout, so all logging goes to stderr (configurable via
//! the `RUST_LOG` environment variable, e.g. `RUST_LOG=debug`).

mod config;
mod daemon;
mod error;
mod features;
mod ipc;
mod server;
mod shim;

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
        Some(other) => {
            tracing::error!("unknown command {other:?}; usage: carl [daemon | standalone]");
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
