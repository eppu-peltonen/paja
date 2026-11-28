//! Records the commit `paja` is built from, for `paja --version`: `PAJA_COMMIT` when the
//! release workflow sets it, else what git says about this checkout, else `unknown`.

use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=PAJA_COMMIT");
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
    };
    // Only Paja's own repository names its commit: a crate unpacked or vendored under some
    // other repository (a `cargo install` under a home directory that is one) is not its
    // checkout. Paja's repository tracks this script at this path.
    let ours = git(&["ls-files", "--full-name", "--", "build.rs"]).as_deref()
        == Some("crates/paja/build.rs");
    if ours {
        // A new commit or checkout changes these, so the recorded commit follows it: HEAD for a
        // checkout, the branch's ref for a commit, the reflog for both. A file that doesn't
        // exist (no reflog, a packed ref) is left out, since cargo counts a missing file as
        // changed and would rebuild every time.
        let mut paths = vec![
            "HEAD".to_string(),
            "logs/HEAD".to_string(),
            // A repository on the reftable backend keeps every ref and its log in here.
            "reftable".to_string(),
        ];
        if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"]) {
            paths.push(branch);
        }
        for path in &paths {
            if let Some(file) = git(&["rev-parse", "--git-path", path]) {
                let file = Path::new(&dir).join(file);
                if file.exists() {
                    // Cargo scans a directory for any newer file in it.
                    println!("cargo:rerun-if-changed={}", file.display());
                } else if path.starts_with("refs/") {
                    // A packed branch: a commit writes it loose, under the nearest directory
                    // that is still there (packing removes a `feat/` left empty).
                    let mut up = file.parent();
                    for _ in path.matches('/').skip(1) {
                        match up {
                            Some(d) if !d.is_dir() => up = d.parent(),
                            _ => break,
                        }
                    }
                    if let Some(d) = up.filter(|d| d.is_dir()) {
                        println!("cargo:rerun-if-changed={}", d.display());
                    }
                }
            }
        }
    }
    let commit = std::env::var("PAJA_COMMIT")
        .ok()
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .or_else(|| ours.then(|| git(&["rev-parse", "HEAD"])).flatten())
        .unwrap_or_else(|| "unknown".to_string());
    // A short hash reads like git's own; a full one still names the same commit.
    let short: String = commit.chars().take(12).collect();
    println!("cargo:rustc-env=PAJA_COMMIT={short}");
}
