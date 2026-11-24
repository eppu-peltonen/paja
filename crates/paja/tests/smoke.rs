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

fn init_repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("hello.txt"), "hi\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-q", "-m", "init"]);
}

#[test]
fn a_removed_name_can_be_reused_and_resumes_its_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, home) = (tmp.path().join("app"), tmp.path().join("home"));
    init_repo(&repo);

    let (ok, a) = paja(&home, &repo, &["env", "create", "alpha"]);
    assert!(ok, "{a}");
    let path = Path::new(a["path"].as_str().unwrap()).to_path_buf();
    std::fs::write(path.join("work.txt"), "unlanded\n").unwrap();
    git(&path, &["add", "."]);
    git(&path, &["commit", "-q", "-m", "work"]);
    let (ok, _) = paja(&home, &repo, &["env", "remove", "alpha"]);
    assert!(ok);

    let (ok, again) = paja(&home, &repo, &["env", "create", "alpha"]);
    assert!(ok, "{again}");
    assert!(
        path.join("work.txt").exists(),
        "the kept branch should be resumed"
    );

    // --base can't apply to a branch that already exists, and the error says what to do.
    paja(&home, &repo, &["env", "remove", "alpha"]);
    let (ok, err) = paja(&home, &repo, &["env", "create", "alpha", "--base", "main"]);
    assert!(!ok);
    assert!(
        err["error"].as_str().unwrap().contains("branch -D"),
        "{err}"
    );
}

#[test]
fn an_env_created_inside_an_env_belongs_to_the_main_repo() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, home) = (tmp.path().join("app"), tmp.path().join("home"));
    init_repo(&repo);

    let (_, parent) = paja(&home, &repo, &["env", "create", "parent"]);
    let parent_path = Path::new(parent["path"].as_str().unwrap()).to_path_buf();
    let (ok, child) = paja(&home, &parent_path, &["env", "create", "child"]);
    assert!(ok, "{child}");
    assert_eq!(
        Path::new(child["repo"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        repo.canonicalize().unwrap()
    );
    let (ok, _) = paja(&home, &repo, &["env", "remove", "parent"]);
    assert!(ok);
    let (ok, removed) = paja(&home, &repo, &["env", "remove", "child"]);
    assert!(ok, "{removed}");
}

#[test]
fn a_relative_paja_home_is_made_absolute() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("app");
    init_repo(&repo);
    let elsewhere = tmp.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();

    let (ok, env) = paja(
        Path::new("rel-home"),
        &elsewhere,
        &["env", "create", "gamma", "--repo", repo.to_str().unwrap()],
    );
    assert!(ok, "{env}");
    let path = Path::new(env["path"].as_str().unwrap());
    assert!(path.is_absolute());
    assert!(path.starts_with(elsewhere.join("rel-home")));
    let (ok, _) = paja(
        Path::new("rel-home"),
        &elsewhere,
        &["env", "remove", "gamma"],
    );
    assert!(ok);
    assert!(!path.exists());
}

#[test]
fn an_env_whose_repo_is_gone_can_still_be_removed_with_force() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, home) = (tmp.path().join("app"), tmp.path().join("home"));
    init_repo(&repo);
    let (_, env) = paja(&home, &repo, &["env", "create", "orphan"]);
    std::fs::remove_dir_all(&repo).unwrap();

    let (ok, err) = paja(&home, tmp.path(), &["env", "remove", "orphan"]);
    assert!(!ok);
    assert!(err["error"].as_str().unwrap().contains("--force"), "{err}");
    let (ok, removed) = paja(&home, tmp.path(), &["env", "remove", "orphan", "--force"]);
    assert!(ok, "{removed}");
    assert!(!Path::new(env["path"].as_str().unwrap()).exists());
    let (_, list) = paja(&home, tmp.path(), &["env", "list"]);
    assert!(list.as_array().unwrap().is_empty());
}

#[test]
fn usage_errors_are_json_too() {
    let tmp = tempfile::tempdir().unwrap();
    let (ok, err) = paja(tmp.path(), tmp.path(), &["env", "create"]);
    assert!(!ok);
    assert!(err["error"].as_str().unwrap().contains("NAME"), "{err}");
}

#[test]
fn show_and_log_limit_work() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, home) = (tmp.path().join("app"), tmp.path().join("home"));
    init_repo(&repo);
    paja(&home, &repo, &["env", "create", "one"]);
    paja(&home, &repo, &["env", "create", "two"]);

    let (ok, shown) = paja(&home, &repo, &["env", "show", "two"]);
    assert!(ok, "{shown}");
    assert_eq!(shown["name"], "two");
    let (ok, missing) = paja(&home, &repo, &["env", "show", "nope"]);
    assert!(!ok);
    assert!(missing["error"].as_str().unwrap().contains("paja env list"));

    let (_, log) = paja(&home, &repo, &["log", "--limit", "1"]);
    let log = log.as_array().unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0]["seq"], 2);
}

#[test]
fn help_and_version_are_json_under_json() {
    let tmp = tempfile::tempdir().unwrap();
    let (ok, version) = paja(tmp.path(), tmp.path(), &["--version"]);
    assert!(ok);
    assert!(version["version"].as_str().unwrap().contains("paja"));
    let (ok, help) = paja(tmp.path(), tmp.path(), &["--help"]);
    assert!(ok);
    assert!(help["help"].as_str().unwrap().contains("env"));
}

#[test]
fn works_inside_a_submodule() {
    let tmp = tempfile::tempdir().unwrap();
    let (lib, app, home) = (
        tmp.path().join("lib"),
        tmp.path().join("app"),
        tmp.path().join("home"),
    );
    init_repo(&lib);
    init_repo(&app);
    git(
        &app,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            lib.to_str().unwrap(),
            "lib",
        ],
    );
    let sub = app.join("lib");
    let (ok, env) = paja(&home, &sub, &["env", "create", "sub-env"]);
    assert!(ok, "{env}");
    assert_eq!(
        Path::new(env["repo"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        sub.canonicalize().unwrap()
    );
}

#[test]
fn force_removes_a_directory_git_no_longer_knows() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, home) = (tmp.path().join("app"), tmp.path().join("home"));
    init_repo(&repo);
    let (_, env) = paja(&home, &repo, &["env", "create", "lost"]);
    let path = Path::new(env["path"].as_str().unwrap()).to_path_buf();
    // Re-create the repo at the same path: git forgets the worktree, the directory stays.
    std::fs::remove_dir_all(&repo).unwrap();
    init_repo(&repo);

    let (ok, removed) = paja(&home, &repo, &["env", "remove", "lost", "--force"]);
    assert!(ok, "{removed}");
    assert!(!path.exists());
}
