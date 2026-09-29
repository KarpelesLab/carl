//! Carl — a multipurpose MCP helper that gives AI agents hands.
//!
//! Runs as a local process speaking the Model Context Protocol over stdio. The
//! protocol owns stdout, so all logging goes to stderr (configurable via the
//! `RUST_LOG` environment variable, e.g. `RUST_LOG=debug`).

mod config;
mod error;
mod features;
mod server;

use anyhow::Result;
use rmcp::{ServiceExt, transport::stdio};
use tracing_subscriber::EnvFilter;

use crate::{config::Config, server::Carl};

#[tokio::main]
async fn main() -> Result<()> {
    // Log to stderr — stdout is reserved for the MCP wire protocol.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let config = Config::from_env();
    tracing::info!(data_dir = %config.data_dir.display(), "starting carl");

    let service = Carl::new(config)
        .serve(stdio())
        .await
        .inspect_err(|e| tracing::error!(error = ?e, "failed to start MCP server"))?;

    service.waiting().await?;
    Ok(())
}
