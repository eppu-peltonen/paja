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
    paja_with(home, cwd, args, &[])
}

/// `paja` with extra environment variables, such as `PAJA_GH` pointing at a fake `gh`.
fn paja_with(home: &Path, cwd: &Path, args: &[&str], vars: &[(&str, &Path)]) -> (bool, Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_paja"))
        .current_dir(cwd)
        .env("PAJA_HOME", home)
        .envs(vars.iter().copied())
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

fn git_out(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?} failed");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[test]
fn a_live_name_is_reported_as_taken_even_with_base() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, home) = (tmp.path().join("app"), tmp.path().join("home"));
    init_repo(&repo);
    let (ok, _) = paja(&home, &repo, &["env", "create", "busy"]);
    assert!(ok);
    let (ok, err) = paja(&home, &repo, &["env", "create", "busy", "--base", "main"]);
    assert!(!ok);
    let msg = err["error"].as_str().unwrap();
    assert!(
        msg.contains("already exists") && msg.contains("env remove busy"),
        "{err}"
    );
    assert!(!msg.contains("branch -D"), "{err}");
}

#[test]
fn head_means_the_callers_checkout_inside_an_env() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, home) = (tmp.path().join("app"), tmp.path().join("home"));
    init_repo(&repo);
    let (_, parent) = paja(&home, &repo, &["env", "create", "parent"]);
    let parent_path = Path::new(parent["path"].as_str().unwrap()).to_path_buf();
    std::fs::write(parent_path.join("more.txt"), "more\n").unwrap();
    git(&parent_path, &["add", "."]);
    git(&parent_path, &["commit", "-q", "-m", "more"]);
    let parent_head = git_out(&parent_path, &["rev-parse", "HEAD"]);
    assert_ne!(parent_head, git_out(&repo, &["rev-parse", "HEAD"]));

    let (ok, child) = paja(&home, &parent_path, &["env", "create", "child"]);
    assert!(ok, "{child}");
    assert_eq!(child["base_commit"].as_str().unwrap(), parent_head);
    let (ok, rel) = paja(
        &home,
        &parent_path,
        &["env", "create", "rel", "--base", "HEAD~1"],
    );
    assert!(ok, "{rel}");
    assert_eq!(
        rel["base_commit"].as_str().unwrap(),
        git_out(&parent_path, &["rev-parse", "HEAD~1"])
    );
}

#[test]
fn a_missing_subcommand_is_an_error_under_json() {
    let tmp = tempfile::tempdir().unwrap();
    let (ok, out) = paja(tmp.path(), tmp.path(), &[]);
    assert!(!ok);
    assert!(
        out["error"].as_str().unwrap().contains("subcommand"),
        "{out}"
    );
}

#[test]
fn a_bare_repository_gets_its_own_hint() {
    let tmp = tempfile::tempdir().unwrap();
    let (bare, home) = (tmp.path().join("bare.git"), tmp.path().join("home"));
    std::fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "-q", "--bare"]);
    let (ok, err) = paja(&home, &bare, &["env", "create", "x"]);
    assert!(!ok);
    assert!(
        err["error"].as_str().unwrap().contains("bare repository"),
        "{err}"
    );
}

#[test]
fn force_does_not_delete_a_worktree_git_still_guards() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, home) = (tmp.path().join("app"), tmp.path().join("home"));
    init_repo(&repo);
    let (_, env) = paja(&home, &repo, &["env", "create", "held"]);
    let path = Path::new(env["path"].as_str().unwrap()).to_path_buf();
    // A locked worktree refuses a single --force; paja must report that, not delete it.
    git(&repo, &["worktree", "lock", path.to_str().unwrap()]);

    let (ok, err) = paja(&home, &repo, &["env", "remove", "held", "--force"]);
    assert!(!ok);
    let msg = err["error"].as_str().unwrap();
    assert!(
        msg.contains("locked") && msg.contains("worktree unlock"),
        "{err}"
    );
    assert!(path.exists());
}

/// Drive `paja mcp` over stdio: initialize, list tools, then create, list and remove an env.
#[test]
fn the_mcp_server_serves_the_same_core() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::Stdio;

    let tmp = tempfile::tempdir().unwrap();
    let (repo, home) = (tmp.path().join("app"), tmp.path().join("home"));
    init_repo(&repo);
    let mut child = Command::new(env!("CARGO_BIN_EXE_paja"))
        .arg("mcp")
        .current_dir(&repo)
        .env("PAJA_HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());
    let mut next_id = 0;
    let mut request = |method: &str, params: Value| -> Value {
        next_id += 1;
        let msg = serde_json::json!({"jsonrpc": "2.0", "id": next_id, "method": method, "params": params});
        writeln!(stdin, "{msg}").unwrap();
        if method == "initialize" {
            let note = serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
            // The client confirms only after the server's reply, as the protocol asks.
            let mut line = String::new();
            stdout.read_line(&mut line).unwrap();
            writeln!(stdin, "{note}").unwrap();
            return serde_json::from_str(&line).unwrap();
        }
        loop {
            let mut line = String::new();
            assert!(
                stdout.read_line(&mut line).unwrap() > 0,
                "server closed stdout"
            );
            let reply: Value = serde_json::from_str(&line).unwrap();
            if reply["id"] == next_id {
                return reply;
            }
        }
    };
    let call =
        |request: &mut dyn FnMut(&str, Value) -> Value, tool: &str, args: Value| -> (bool, Value) {
            let reply = request(
                "tools/call",
                serde_json::json!({"name": tool, "arguments": args}),
            );
            let result = &reply["result"];
            let text = result["content"][0]["text"]
                .as_str()
                .unwrap_or_else(|| panic!("{reply}"));
            (
                result["isError"] != true,
                serde_json::from_str(text).unwrap(),
            )
        };

    let init = request(
        "initialize",
        serde_json::json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": {"name": "smoke", "version": "0"}
        }),
    );
    assert_eq!(init["result"]["serverInfo"]["name"], "paja", "{init}");
    let tools = request("tools/list", serde_json::json!({}));
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    for want in [
        "env_create",
        "env_list",
        "env_show",
        "env_remove",
        "log",
        "work_sync",
        "work_list",
        "work_show",
        "work_next",
        "work_claim",
        "work_set",
    ] {
        assert!(names.contains(&want), "{want} missing from {names:?}");
    }

    let (ok, env) = call(
        &mut request,
        "env_create",
        serde_json::json!({"name": "via-mcp"}),
    );
    assert!(ok, "{env}");
    assert_eq!(env["branch"], "paja/via-mcp");
    // The CLI sees what the MCP server wrote: one event log, one truth.
    let (ok, listed) = paja(&home, &repo, &["env", "list"]);
    assert!(ok);
    assert_eq!(listed[0]["name"], "via-mcp");
    let (ok, mcp_list) = call(&mut request, "env_list", serde_json::json!({}));
    assert!(ok);
    assert_eq!(mcp_list, listed, "both doors give the same JSON");
    let (ok, shown) = call(
        &mut request,
        "env_show",
        serde_json::json!({"name": "via-mcp"}),
    );
    assert!(ok);
    assert_eq!(shown, listed[0]);
    let (ok, missing) = call(
        &mut request,
        "env_show",
        serde_json::json!({"name": "nope"}),
    );
    assert!(!ok);
    assert!(
        missing["error"]
            .as_str()
            .unwrap()
            .contains("no environment"),
        "{missing}"
    );
    let (ok, err) = call(
        &mut request,
        "env_create",
        serde_json::json!({"name": "via-mcp"}),
    );
    assert!(!ok);
    assert!(
        err["error"].as_str().unwrap().contains("already exists"),
        "{err}"
    );
    // Jono over MCP: nothing imported yet, so next is null and show says how to import.
    let (ok, next) = call(&mut request, "work_next", serde_json::json!({}));
    assert!(ok);
    assert!(next.is_null(), "{next}");
    let (ok, missing) = call(&mut request, "work_show", serde_json::json!({"number": 1}));
    assert!(!ok);
    assert!(
        missing["error"]
            .as_str()
            .unwrap()
            .contains("paja work sync"),
        "{missing}"
    );
    let (ok, removed) = call(
        &mut request,
        "env_remove",
        serde_json::json!({"name": "via-mcp"}),
    );
    assert!(ok, "{removed}");
    let (ok, log) = call(&mut request, "log", serde_json::json!({"limit": 1}));
    assert!(ok);
    assert_eq!(log[0]["kind"], "env.removed");

    drop(stdin);
    assert!(child.wait().unwrap().success());
}

/// Install `body` as the program `name` in `dir`: a shell script, which on Windows the
/// `fake_tool` example runs (Windows can't run a script as a program).
fn program(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
    let script = dir.join(name);
    std::fs::write(&script, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }
    #[cfg(windows)]
    {
        let tool = Path::new(env!("CARGO_BIN_EXE_paja"))
            .parent()
            .unwrap()
            .join("examples")
            .join("fake_tool.exe");
        let exe = dir.join(format!("{name}.exe"));
        std::fs::copy(&tool, &exe).unwrap_or_else(|e| {
            panic!("copying {} (run all of `cargo test`): {e}", tool.display())
        });
        exe
    }
}

/// `path` as a shell script can name it: with `/`, which Git Bash on Windows reads too.
fn slash(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

/// A fake `gh` that prints the issues in `issues.json` next to it, and logs its arguments.
fn fake_gh(dir: &Path, issues: &str) -> std::path::PathBuf {
    std::fs::write(dir.join("issues.json"), issues).unwrap();
    program(
        dir,
        "gh",
        &format!(
            "#!/bin/sh\necho \"$@\" >> {log}\ncat {issues}\n",
            log = slash(&dir.join("gh.log")),
            issues = slash(&dir.join("issues.json"))
        ),
    )
}

#[test]
fn work_sync_imports_labelled_issues_and_next_picks_the_oldest() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, home) = (tmp.path().join("app"), tmp.path().join("home"));
    init_repo(&repo);
    let gh = fake_gh(
        tmp.path(),
        r#"[{"number": 7, "title": "Later", "body": "", "url": "https://example.com/7"},
            {"number": 3, "title": "Add a plugin", "body": "Ship it.", "url": "https://example.com/3"}]"#,
    );
    let with_gh = [("PAJA_GH", gh.as_path())];

    let (ok, report) = paja_with(&home, &repo, &["work", "sync"], &with_gh);
    assert!(ok, "{report}");
    assert_eq!(report["imported"], serde_json::json!([7, 3]));
    let log = std::fs::read_to_string(tmp.path().join("gh.log")).unwrap();
    assert!(
        log.contains("--label paja") && log.contains("--state open"),
        "{log}"
    );

    let (ok, next) = paja(&home, &repo, &["work", "next"]);
    assert!(ok);
    assert_eq!(next["number"], 3);
    assert_eq!(next["status"], "queued");
    let (ok, shown) = paja(&home, &repo, &["work", "show", "3"]);
    assert!(ok);
    assert_eq!(shown["body"], "Ship it.");
    let (ok, listed) = paja(&home, &repo, &["work", "list"]);
    assert!(ok);
    assert_eq!(listed.as_array().unwrap().len(), 2);

    // Claim hands out 3 and marks it active; an escalation keeps its question.
    let (ok, claimed) = paja(&home, &repo, &["work", "claim"]);
    assert!(ok);
    assert_eq!(claimed["number"], 3);
    assert_eq!(claimed["status"], "active");
    let (ok, esc) = paja(
        &home,
        &repo,
        &["work", "set", "3", "escalated", "--note", "Which API?"],
    );
    assert!(ok, "{esc}");
    assert_eq!(esc["note"], "Which API?");
    let (ok, bad) = paja(&home, &repo, &["work", "set", "3", "done"]);
    assert!(!ok);
    assert!(
        bad["error"].as_str().unwrap().contains("queued, active"),
        "{bad}"
    );
    paja(&home, &repo, &["work", "set", "3", "queued"]);

    // Issue 3 is closed upstream: the next sync marks it closed, and 7 is next.
    std::fs::write(
        tmp.path().join("issues.json"),
        r#"[{"number": 7, "title": "Later", "body": "", "url": "https://example.com/7"}]"#,
    )
    .unwrap();
    let (ok, report) = paja_with(&home, &repo, &["work", "sync"], &with_gh);
    assert!(ok);
    assert_eq!(report["closed"], serde_json::json!([3]));
    let (_, next) = paja(&home, &repo, &["work", "next"]);
    assert_eq!(next["number"], 7);

    let (ok, missing) = paja(&home, &repo, &["work", "show", "99"]);
    assert!(!ok);
    assert!(
        missing["error"]
            .as_str()
            .unwrap()
            .contains("paja work sync"),
        "{missing}"
    );
}

#[test]
fn work_sync_says_what_to_do_when_gh_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, home) = (tmp.path().join("app"), tmp.path().join("home"));
    init_repo(&repo);
    let missing = tmp.path().join("no-such-gh");
    let (ok, err) = paja_with(
        &home,
        &repo,
        &["work", "sync"],
        &[("PAJA_GH", missing.as_path())],
    );
    assert!(!ok);
    assert!(
        err["error"].as_str().unwrap().contains("gh auth login"),
        "{err}"
    );
}

/// A repo with a bare `origin`, `.paja/checks` on main, and fake `gh` and `claude` programs.
/// The fake reviewer's verdict is read from `verdict.txt`; the fake builder writes
/// `feature.txt` and commits it.
struct Factory {
    tmp: tempfile::TempDir,
    repo: std::path::PathBuf,
    home: std::path::PathBuf,
    gh: std::path::PathBuf,
    claude: std::path::PathBuf,
}

impl Factory {
    fn new(checks: &str, verdict: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, home) = (tmp.path().join("app"), tmp.path().join("home"));
        let origin = tmp.path().join("origin.git");
        std::fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "-q", "--bare", "-b", "main"]);
        init_repo(&repo);
        std::fs::create_dir_all(repo.join(".paja")).unwrap();
        std::fs::write(repo.join(".paja/checks"), checks).unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "checks"]);
        git(
            &repo,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        git(&repo, &["push", "-q", "origin", "main"]);

        let dir = tmp.path();
        std::fs::write(
            dir.join("issues.json"),
            r#"[{"number": 5, "title": "Add a feature file", "body": "Please.", "url": "https://example.com/5"}]"#,
        )
        .unwrap();
        std::fs::write(dir.join("verdict.txt"), verdict).unwrap();
        let script = |name: &str, body: String| program(dir, name, &body);
        let d = slash(dir);
        let gh = script(
            "gh",
            format!(
                "#!/bin/sh\necho \"$@\" >> {d}/gh.log\ncase \"$1 $2\" in\n\
                 \"issue list\") cat {d}/issues.json ;;\n\
                 \"pr view\") exit 1 ;;\n\
                 \"pr create\") if [ -f {d}/gh-down ]; then echo 'HTTP 502' >&2; exit 1; fi\n\
                 echo https://example.com/pull/1 ;;\n\
                 \"issue comment\") if [ -f {d}/gh-down ]; then exit 1; fi ;;\n\
                 *) ;;\nesac\n"
            ),
        );
        let claude = script(
            "claude",
            format!(
                "#!/bin/sh\necho \"$PAJA_HOME\" >> {d}/claude-homes.log\n\
                 echo \"$*\" >> {d}/claude-args.log\n\
                 case \"$*\" in\n\
                 *--json-schema*) v=$(cat {d}/verdict.txt)\n\
                 echo \"{{\\\"is_error\\\": false, \\\"structured_output\\\": {{\\\"verdict\\\": \\\"$v\\\", \\\"summary\\\": \\\"fake review\\\", \\\"comments\\\": \\\"add more\\\", \\\"question\\\": \\\"Which API?\\\", \\\"follow_ups\\\": [\\\"Tidy the helper\\\"]}}}}\" ;;\n\
                 *) echo built >> feature.txt\n\
                 case \"$*\" in *\"another round\"*) if [ -f {d}/then-land ]; then echo land > {d}/verdict.txt; fi ;; esac\n\
                 stamp=$(echo \"$*\" | sed -n \"s/.*--stamp [\\\\\\\"']*\\([^\\\\\\\"']*\\).*/\\1/p\")\n\
                 if [ ! -f {d}/unhooked ] && [ -n \"$stamp\" ]; then : > \"$stamp\"; fi\n\
                 case \"$*\" in *\"another round\"*) if [ -f {d}/lazy ]; then rm {d}/lazy; git checkout -q -- feature.txt; echo '{{\"is_error\": false, \"result\": \"done\"}}'; exit 0; fi ;; esac\n\
                 if [ -f {d}/cheat ]; then echo true > .paja/checks; git add .paja/checks; fi\n\
                 git add feature.txt\n\
                 git -c user.name=t -c user.email=t@example.com commit -qm built >/dev/null\n\
                 echo '{{\"is_error\": false, \"result\": \"done\"}}' ;;\nesac\n"
            ),
        );
        Factory {
            tmp,
            repo,
            home,
            gh,
            claude,
        }
    }

    fn run(&self) -> (bool, Value) {
        paja_with(
            &self.home,
            &self.repo,
            &["run"],
            &[
                ("PAJA_GH", self.gh.as_path()),
                ("PAJA_CLAUDE", self.claude.as_path()),
            ],
        )
    }

    fn gh_log(&self) -> String {
        std::fs::read_to_string(self.tmp.path().join("gh.log")).unwrap_or_default()
    }
}

#[test]
fn run_builds_reviews_and_lands_an_issue() {
    // Vasara's checks run the branch's code, so they get no GitHub access either: Mestari's
    // `PAJA_GH` is gone, and so is its PAJA_HOME.
    let f = Factory::new(
        "test -f feature.txt\ntest -z \"$PAJA_GH\"\ntest ! -e \"$PAJA_HOME/events.jsonl\"\n",
        "land",
    );
    let (ok, report) = f.run();
    assert!(ok, "{report}");
    assert_eq!(report["outcome"], "landed", "{report}");
    assert_eq!(report["number"], 5);
    assert_eq!(report["pr"], "https://example.com/pull/1");
    let log = f.gh_log();
    assert!(
        log.contains("pr create --base main --head paja/work-5"),
        "{log}"
    );
    assert!(
        log.contains("pr checks https://example.com/pull/1 --watch"),
        "{log}"
    );
    assert!(
        log.contains("pr merge https://example.com/pull/1 --squash"),
        "{log}"
    );
    // The branch reached origin, the item is landed, and the environment is gone.
    git(&f.repo, &["fetch", "-q", "origin", "paja/work-5"]);
    let (_, item) = paja(&f.home, &f.repo, &["work", "show", "5"]);
    assert_eq!(item["status"], "landed");
    let (_, envs) = paja(&f.home, &f.repo, &["env", "list"]);
    assert_eq!(envs, serde_json::json!([]));
    // A finding that didn't block landing becomes an issue for a person to queue, not queued
    // work.
    assert!(
        log.contains("issue create --label paja-follow-up --title Tidy the helper"),
        "{log}"
    );
    // Every agent runs under the deny-list hook, bounded to its environment, and with no
    // settings files that could switch the hook off.
    let args = std::fs::read_to_string(f.tmp.path().join("claude-args.log")).unwrap();
    assert!(args.contains("--setting-sources  --settings"), "{args}");
    // CLAUDE.md doesn't load without setting sources, so main's copy comes with the prompt.
    assert!(args.contains("--append-system-prompt"), "{args}");
    // Reviewers run read-only under the hook; builders don't.
    let reviewer = args.lines().find(|l| l.contains("--json-schema")).unwrap();
    let builder = args.lines().find(|l| !l.contains("--json-schema")).unwrap();
    assert!(reviewer.contains("--read-only"), "{reviewer}");
    assert!(!builder.contains("--read-only"), "{builder}");
    assert!(
        args.contains("PreToolUse") && args.contains("hook --root") && args.contains("work-5"),
        "{args}"
    );
    // Agents never see the factory's own PAJA_HOME.
    let homes = std::fs::read_to_string(f.tmp.path().join("claude-homes.log")).unwrap();
    assert!(!homes.contains(f.home.to_str().unwrap()), "{homes}");

    // Nothing else is queued.
    let (ok, idle) = f.run();
    assert!(ok);
    assert_eq!(idle["outcome"], "idle");
}

#[test]
fn run_escalates_when_the_reviewer_asks_a_question() {
    let f = Factory::new("true\n", "escalate");
    let (ok, report) = f.run();
    assert!(ok, "{report}");
    assert_eq!(report["outcome"], "escalated");
    assert_eq!(report["question"], "Which API?");
    assert!(
        f.gh_log().contains("issue comment 5 --body"),
        "{}",
        f.gh_log()
    );
    assert!(!f.gh_log().contains("pr merge"));
    let (_, item) = paja(&f.home, &f.repo, &["work", "show", "5"]);
    assert_eq!(item["status"], "escalated");
    assert_eq!(item["note"], "Which API?");
}

#[test]
fn run_gives_up_after_three_failed_rounds_and_asks() {
    let f = Factory::new("test -f never.txt\n", "land");
    let (ok, report) = f.run();
    assert!(ok, "{report}");
    assert_eq!(report["outcome"], "escalated");
    assert_eq!(report["rounds"], 3);
    assert!(
        report["question"].as_str().unwrap().contains("never.txt"),
        "{report}"
    );
    let (_, log) = paja(&f.home, &f.repo, &["log"]);
    let rounds = log
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "run.round")
        .count();
    assert_eq!(rounds, 3);
}

#[test]
fn run_checks_come_from_main_not_the_branch() {
    // The builder can't make its change pass by editing .paja/checks on its branch.
    let f = Factory::new("test -f never.txt\n", "land");
    std::fs::write(f.tmp.path().join("cheat"), "").unwrap();
    let (_, report) = f.run();
    assert_eq!(report["outcome"], "escalated", "{report}");
    assert!(
        report["question"].as_str().unwrap().contains("never.txt"),
        "{report}"
    );
    assert!(!f.gh_log().contains("pr merge"));
}

#[test]
fn a_run_that_errors_parks_the_item_even_if_github_is_down() {
    let f = Factory::new("true\n", "land");
    std::fs::write(f.tmp.path().join("gh-down"), "").unwrap();
    let (ok, report) = f.run();
    assert!(ok, "{report}");
    assert_eq!(report["outcome"], "escalated", "{report}");
    assert_eq!(report["rounds"], 1);
    assert!(
        report["question"].as_str().unwrap().contains("HTTP 502"),
        "{report}"
    );
    let (_, item) = paja(&f.home, &f.repo, &["work", "show", "5"]);
    assert_eq!(item["status"], "escalated", "never left active");
    let (_, envs) = paja(&f.home, &f.repo, &["env", "list"]);
    assert_eq!(envs, serde_json::json!([]), "the worktree is freed");
    // The reviewer's follow-up isn't filed, but it isn't lost either.
    let log = std::fs::read_to_string(f.home.join("events.jsonl")).unwrap();
    assert!(
        log.contains("run.follow_up") && log.contains("not filed: the change did not land"),
        "{log}"
    );
}

#[test]
fn a_second_review_judges_only_the_fix() {
    // Round one asks for changes; round two's reviewer sees the earlier blockers and the fix's
    // own diff, and lands it.
    let f = Factory::new("true\n", "changes");
    std::fs::write(f.tmp.path().join("then-land"), "").unwrap();
    let (ok, report) = f.run();
    assert!(ok, "{report}");
    assert_eq!(report["outcome"], "landed", "{report}");
    assert_eq!(report["rounds"], 2, "{report}");
    let args = std::fs::read_to_string(f.tmp.path().join("claude-args.log")).unwrap();
    // Each logged call starts with `-p`; the reviewer's carries `--json-schema`.
    let rereview = args
        .split("\n-p ")
        .filter(|call| call.contains("--json-schema"))
        .nth(1)
        .unwrap_or_default();
    assert!(rereview.contains("found these blockers"), "{args}");
    assert!(rereview.contains("add more"), "{args}");
    assert!(
        rereview.contains("Judge only the fix: `git diff "),
        "{args}"
    );
}

#[test]
fn a_fix_round_without_commits_is_not_reviewed() {
    // After a review asks for changes, a round that commits nothing goes back to the builder
    // instead of letting the reviewer land the unchanged branch.
    let f = Factory::new("true\n", "changes");
    std::fs::write(f.tmp.path().join("then-land"), "").unwrap();
    std::fs::write(f.tmp.path().join("lazy"), "").unwrap();
    let (ok, report) = f.run();
    assert!(ok, "{report}");
    assert_eq!(report["outcome"], "landed", "{report}");
    assert_eq!(report["rounds"], 3, "{report}");
    let args = std::fs::read_to_string(f.tmp.path().join("claude-args.log")).unwrap();
    assert_eq!(args.matches("--json-schema").count(), 2, "{args}");
    assert!(args.contains("made no new commits"), "{args}");
    // The earlier round's follow-ups reach the re-reviewer.
    let rereview = args
        .split("\n-p ")
        .filter(|call| call.contains("--json-schema"))
        .nth(1)
        .unwrap_or_default();
    assert!(rereview.contains("Tidy the helper"), "{args}");
}

#[test]
fn checks_that_change_the_worktree_fail_and_are_put_back() {
    // Branch code run by the checks must not leave files or commits for the reviewer to miss.
    let f = Factory::new("touch stray.txt\n", "land");
    let (_, report) = f.run();
    assert_eq!(report["outcome"], "escalated", "{report}");
    assert!(!f.gh_log().contains("pr create"));
    let log = std::fs::read_to_string(f.home.join("events.jsonl")).unwrap();
    assert!(log.contains("changed the worktree or its commits"), "{log}");
}

#[test]
fn checks_that_change_git_configuration_stop_the_run() {
    // Branch code run by the checks could plant a credential helper for Mestari's own git.
    let f = Factory::new("git config paja.planted yes\n", "land");
    let (ok, report) = f.run();
    assert!(!ok, "{report}");
    assert!(
        report["error"]
            .as_str()
            .unwrap()
            .contains("git's configuration, hooks or attributes"),
        "{report}"
    );
    assert!(!f.gh_log().contains("pr create"));
    // The item is parked, and the error tells the person; Paja runs no more git there, not
    // even to remove the environment.
    let (_, item) = paja(&f.home, &f.repo, &["work", "show", "5"]);
    assert_eq!(item["status"], "escalated");
    // Not even gh, which runs git to find the repository.
    assert!(!f.gh_log().contains("issue comment"), "{}", f.gh_log());
    let (_, envs) = paja(&f.home, &f.repo, &["env", "list"]);
    assert_eq!(envs.as_array().map(Vec::len), Some(1), "{envs}");
    // Later runs run no git there until a person says the repository was checked.
    let (ok, again) = f.run();
    assert!(!ok, "{again}");
    assert!(
        again["error"]
            .as_str()
            .unwrap()
            .contains("--tamper-checked"),
        "{again}"
    );
    // A person's check opens it again: the run fetches and syncs as usual (the item itself
    // stays escalated, so the queue is empty).
    let (ok, checked) = paja_with(
        &f.home,
        &f.repo,
        &["run", "--tamper-checked"],
        &[
            ("PAJA_GH", f.gh.as_path()),
            ("PAJA_CLAUDE", f.claude.as_path()),
        ],
    );
    assert!(ok, "{checked}");
    assert_eq!(checked["outcome"], "idle", "{checked}");
}

#[test]
fn work_from_an_agent_the_hook_never_saw_does_not_land() {
    // A run with tool turns but no hook call means something switched the hook off.
    let f = Factory::new("true\n", "land");
    std::fs::write(f.tmp.path().join("unhooked"), "").unwrap();
    let (_, report) = f.run();
    assert_eq!(report["outcome"], "escalated", "{report}");
    assert!(
        report["question"]
            .as_str()
            .unwrap()
            .contains("never called Paja's deny-list hook"),
        "{report}"
    );
    assert!(!f.gh_log().contains("pr create"));
    // Its commits are gone from the branch, so a later run can't resume them.
    let log = std::process::Command::new("git")
        .arg("-C")
        .arg(&f.repo)
        .args(["log", "--oneline", "paja/work-5"])
        .output()
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&log.stdout).contains("built"),
        "{}",
        String::from_utf8_lossy(&log.stdout)
    );
}

#[test]
fn an_empty_checks_file_is_an_error_not_a_pass() {
    let f = Factory::new("# nothing yet\n", "land");
    let (ok, report) = f.run();
    assert!(ok, "{report}");
    assert_eq!(report["outcome"], "escalated", "{report}");
    assert!(
        report["question"].as_str().unwrap().contains("no commands"),
        "{report}"
    );
    assert!(!f.gh_log().contains("pr merge"));
}

/// `path` (`/w/...`) as an absolute path on this platform: on a drive on Windows.
fn w(path: &str) -> String {
    if cfg!(windows) {
        format!("C:{}", path.replace('/', "\\"))
    } else {
        path.to_string()
    }
}

/// Run `paja hook` with a tool call on stdin; returns its exit code and stderr.
fn hook(input: &str) -> (Option<i32>, String) {
    hook_with(&["--root", &w("/w/env"), "--home", &w("/w/home")], input)
}

fn hook_with(args: &[&str], input: &str) -> (Option<i32>, String) {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = Command::new(env!("CARGO_BIN_EXE_paja"))
        .arg("hook")
        .args(args)
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn the_deny_list_hook_blocks_and_fails_closed() {
    let call = |cmd: &str| {
        serde_json::json!({"tool_name": "Bash", "tool_input": {"command": cmd}, "cwd": w("/w/env")})
            .to_string()
    };
    let (code, why) = hook(&call("cargo test && git push origin main"));
    assert_eq!(code, Some(2), "{why}");
    assert!(why.contains("git push"), "{why}");
    assert_eq!(hook(&call("cargo test")).0, Some(0));
    let write = serde_json::json!({"tool_name": "Write", "tool_input": {"file_path": w("/etc/x")}, "cwd": w("/w/env")});
    assert_eq!(hook(&write.to_string()).0, Some(2));
    // Anything it can't read blocks too.
    let (code, why) = hook("not json");
    assert_eq!(code, Some(2));
    assert!(why.contains("fails closed"), "{why}");
    // So does a hook started without its bounds.
    let (code, why) = hook_with(&[], &call("cargo test"));
    assert_eq!(code, Some(2));
    assert!(why.contains("--root"), "{why}");
    // The factory's own state is off limits, the environment inside it is not.
    let read = |path: &str| {
        serde_json::json!({"tool_name": "Read", "tool_input": {"file_path": path}, "cwd": w("/w/home/envs/a")})
            .to_string()
    };
    let (a, h) = (w("/w/home/envs/a"), w("/w/home"));
    let inside = ["--root", a.as_str(), "--home", h.as_str()];
    assert_eq!(
        hook_with(&inside, &read(&w("/w/home/events.jsonl"))).0,
        Some(2)
    );
    assert_eq!(
        hook_with(&inside, &read(&w("/w/home/envs/a/src/lib.rs"))).0,
        Some(0)
    );
}
