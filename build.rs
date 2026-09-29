//! Captures this build's git identity for the rsupd updater (newer-build
//! detection). Based on the template from `rsupd publish --setup-ci`.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    // CI builds run in the fullrust container as root over a checkout owned by
    // the runner user; without this, git refuses the "dubious ownership" repo
    // and the stamps come out empty.
    let out = Command::new("git")
        .args(["-c", "safe.directory=*"])
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn main() {
    // Empty outside a git checkout (e.g. a crates.io build); the updater then
    // falls back to a plain semver comparison.
    let git_tag = git(&["rev-parse", "--short=7", "HEAD"]).unwrap_or_default();
    let build_unix = git(&["log", "-1", "--format=%ct", "HEAD"]).unwrap_or_default();
    println!("cargo:rustc-env=RSUPD_GIT_TAG={git_tag}");
    println!("cargo:rustc-env=RSUPD_BUILD_UNIX={build_unix}");

    // The branch this build came from is the release channel: `master` (or empty,
    // outside a repo / detached at a tag) means the default channel, anything else
    // (e.g. `beta`) tracks that channel and folds into the reported version. This
    // is the same branch `rsupd publish` reads, so consumer and manifest agree.
    let channel = git(&["rev-parse", "--abbrev-ref", "HEAD"])
        .filter(|b| b != "HEAD")
        .unwrap_or_default();
    println!("cargo:rustc-env=RSUPD_CHANNEL={channel}");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=.git/HEAD");
    if let Ok(head) = std::fs::read_to_string(".git/HEAD")
        && let Some(reference) = head.strip_prefix("ref:")
    {
        // Only the first line, and only if it looks like a real git ref path
        // (no whitespace or control chars) — otherwise an embedded newline
        // could inject extra `cargo:` directives.
        let reference = reference.lines().next().unwrap_or("").trim();
        if !reference.is_empty()
            && !reference
                .chars()
                .any(|c| c.is_whitespace() || c.is_control())
        {
            println!("cargo:rerun-if-changed=.git/{reference}");
        }
    }
}
