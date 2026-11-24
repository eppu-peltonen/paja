//! End-to-end: drive the real `paja` binary against a throwaway git repo.

use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=test", "-c", "user.email=test@example.com"])
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

/// Run `paja --json <args>` and parse its stdout. Returns (success, json).
fn paja(home: &Path, cwd: &Path, args: &[&str]) -> (bool, Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_paja"))
        .current_dir(cwd)
        .env("PAJA_HOME", home)
        .arg("--json")
        .args(args)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {stdout}"));
    (output.status.success(), value)
}

#[test]
fn env_lifecycle() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("app");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("hello.txt"), "hi\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);

    // Two environments get separate worktrees, branches and port blocks.
    let (ok, a) = paja(&home, &repo, &["env", "create", "alpha"]);
    assert!(ok, "{a}");
    let (ok, b) = paja(&home, &repo, &["env", "create", "beta"]);
    assert!(ok, "{b}");
    assert_eq!(a["branch"], "paja/alpha");
    assert_eq!(a["port_base"], 20_000);
    assert_eq!(b["port_base"], 20_100);
    let alpha_path = Path::new(a["path"].as_str().unwrap()).to_path_buf();
    assert!(alpha_path.join("hello.txt").exists());

    // Duplicate names are refused, and the error is JSON too.
    let (ok, dup) = paja(&home, &repo, &["env", "create", "alpha"]);
    assert!(!ok);
    assert!(dup["error"].as_str().unwrap().contains("already exists"));

    let (_, list) = paja(&home, &repo, &["env", "list"]);
    assert_eq!(list.as_array().unwrap().len(), 2);

    // A dirty worktree is not removed without --force.
    std::fs::write(alpha_path.join("hello.txt"), "changed\n").unwrap();
    let (ok, _) = paja(&home, &repo, &["env", "remove", "alpha"]);
    assert!(!ok);
    let (ok, removed) = paja(&home, &repo, &["env", "remove", "alpha", "--force"]);
    assert!(ok, "{removed}");
    assert!(!alpha_path.exists());

    // The freed port block is reused.
    let (ok, c) = paja(&home, &repo, &["env", "create", "gamma"]);
    assert!(ok, "{c}");
    assert_eq!(c["port_base"], 20_000);

    // Every change is in the log, in order.
    let (_, log) = paja(&home, &repo, &["log"]);
    let kinds: Vec<_> = log
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        kinds,
        ["env.created", "env.created", "env.removed", "env.created"]
    );
}
