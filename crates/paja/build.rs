//! Records the commit `paja` is built from, for `paja --version`: `PAJA_COMMIT` when the
//! release workflow sets it, else what git says about this checkout, else `unknown`.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=PAJA_COMMIT");
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    };
    // A new commit or checkout changes these, so the recorded commit follows it.
    for path in ["HEAD", "logs/HEAD"] {
        if let Some(file) = git(&["rev-parse", "--git-path", path]) {
            println!("cargo:rerun-if-changed={file}");
        }
    }
    let commit = std::env::var("PAJA_COMMIT")
        .ok()
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .or_else(|| git(&["rev-parse", "HEAD"]))
        .unwrap_or_else(|| "unknown".to_string());
    // A short hash reads like git's own; a full one still names the same commit.
    let short: String = commit.chars().take(12).collect();
    println!("cargo:rustc-env=PAJA_COMMIT={short}");
}
