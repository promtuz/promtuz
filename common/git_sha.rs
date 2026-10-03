//! Build script shared by the daemons. Bakes the git short SHA into the binary as `PZ_GIT_SHA`
//! for `--version` and the startup log. A non-empty `PZ_GIT_SHA` in the environment wins (a
//! tarball build has no `.git`); otherwise a missing git or `.git` gives `unknown`.

use std::process::Command;

fn main() {
    let sha = std::env::var("PZ_GIT_SHA")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            Command::new("git")
                .args(["rev-parse", "--short=12", "HEAD"])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8(o.stdout).ok())
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "unknown".to_owned());

    println!("cargo:rustc-env=PZ_GIT_SHA={sha}");
    println!("cargo:rerun-if-changed=../.git/HEAD");
    println!("cargo:rerun-if-env-changed=PZ_GIT_SHA");
}
