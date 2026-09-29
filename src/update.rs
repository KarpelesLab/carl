//! Self-update from signed releases, via [rsupd](https://github.com/KarpelesLab/rsupd).
//!
//! Only official builds update (the `auto-update` feature, enabled in CI), and
//! only the daemon: a shim restarting would look like the server dying to its
//! MCP client. Once a new build is installed over the binary, the daemon just
//! exits. Its shims reconnect and start the new build (see [`crate::shim`]).
//! `CARL_NO_UPDATE=1` turns it off at runtime.

/// How long after startup the first check runs, so it never slows a start.
#[cfg(feature = "auto-update")]
const FIRST_CHECK: std::time::Duration = std::time::Duration::from_secs(60);

/// Check for updates in the background, calling `installed` once a new build
/// has replaced our binary on disk. A no-op in builds without auto-update.
#[cfg(feature = "auto-update")]
pub fn spawn(installed: impl FnOnce() + Send + 'static) {
    if std::env::var_os("CARL_NO_UPDATE").is_some() {
        tracing::info!("auto-update disabled by CARL_NO_UPDATE");
        return;
    }
    let updater = match crate::rsupd_updater() {
        Ok(updater) => updater,
        Err(e) => {
            tracing::warn!(error = %e, "auto-update unavailable");
            return;
        }
    };

    std::thread::spawn(move || {
        std::thread::sleep(FIRST_CHECK);
        loop {
            match check_and_install(&updater) {
                Ok(Some(version)) => {
                    tracing::info!(version, "installed an update, restarting");
                    installed();
                    return;
                }
                Ok(None) => tracing::debug!("up to date"),
                Err(e) => tracing::warn!(error = %e, "update check failed"),
            }
            std::thread::sleep(rsupd::update::DEFAULT_INTERVAL);
        }
    });
}

#[cfg(feature = "auto-update")]
fn check_and_install(updater: &rsupd::Updater) -> rsupd::Result<Option<String>> {
    let Some(available) = updater.check()? else {
        return Ok(None);
    };
    updater.install(&available)?;
    Ok(Some(available.version().to_string()))
}

#[cfg(not(feature = "auto-update"))]
pub fn spawn(_installed: impl FnOnce() + Send + 'static) {}
