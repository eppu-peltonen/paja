//! Paja Desk: a desktop window over Paja's core. It is a fourth door to the same core as the
//! CLI, the MCP server and the plugin: its commands call the library functions the CLI calls
//! and return the JSON `paja --json` prints. It keeps no state of its own; everything comes from
//! replaying `$PAJA_HOME`'s event log.

use std::path::{Path, PathBuf};
use std::time::Duration;

use paja::home::Home;
use paja::jono;
use serde_json::{json, Value};
use tauri::{Emitter, Manager};

pub mod runs;
use runs::Runner;

/// The event the window hears when it asked to close while Desk's runs are going, with their
/// repositories, so the person decides whether to stop them or leave them running.
pub const CLOSE_ASKED: &str = "paja://close-asked";

/// The event the window hears when Paja's event log changed, so it reads the queue again.
pub const CHANGED: &str = "paja://events-changed";

/// Every repository's work items, as `paja --json work list --all` prints them. An error is
/// `{"error": "..."}` with the text the CLI prints.
#[tauri::command]
async fn work_list() -> Value {
    off_main(|home| work_list_in(home, &checkout())).await
}

/// [`work_list`] for `home`. `checkout` is only asked for its repository when the log still
/// holds events from before items carried one.
pub fn work_list_in(home: &Home, checkout: &Path) -> Value {
    match jono::list_all_from(home, checkout).and_then(|items| Ok(serde_json::to_value(items)?)) {
        Ok(items) => items,
        Err(err) => error(err),
    }
}

/// Where an item's latest run is, for the run view: [`paja::mestari::live::live`], or null when
/// no run has started it.
#[tauri::command]
async fn work_run(repo: String, number: u64) -> Value {
    off_main(move |home| work_run_in(home, &repo, number)).await
}

pub fn work_run_in(home: &Home, repo: &str, number: u64) -> Value {
    match paja::mestari::live::live(home, repo, number)
        .and_then(|live| Ok(serde_json::to_value(live)?))
    {
        Ok(live) => live,
        Err(err) => error(err),
    }
}

/// Answer escalated item `number` of `repo`, as `paja --json work answer` does: it records the
/// answer, queues the item again and posts the answer on the issue. Returns the item's JSON.
#[tauri::command]
async fn work_answer(repo: String, number: u64, text: String) -> Value {
    off_main(move |home| work_answer_in(home, &repo, number, &text, &checkout())).await
}

/// [`work_answer`] for `home`.
pub fn work_answer_in(home: &Home, repo: &str, number: u64, text: &str, cwd: &Path) -> Value {
    let answered = checkout_of(home, repo, cwd)
        .and_then(|path| jono::answer(home, &path, number, text))
        .and_then(|item| Ok(serde_json::to_value(item)?));
    answered.unwrap_or_else(error)
}

/// File a new issue with `repo`'s work label, as `paja --json work file` does. Returns
/// `{"url": ...}`.
#[tauri::command]
async fn work_file(repo: String, title: String, body: String) -> Value {
    off_main(move |home| work_file_in(home, &repo, &title, &body, &checkout())).await
}

/// Run a core call on a blocking thread, so replaying a long log or waiting for `gh` never
/// freezes the window.
async fn off_main(call: impl FnOnce(&Home) -> Value + Send + 'static) -> Value {
    let ran = tauri::async_runtime::spawn_blocking(move || match Home::from_env() {
        Ok(home) => call(&home),
        Err(err) => error(err),
    })
    .await;
    ran.unwrap_or_else(|err| json!({ "error": format!("Desk's worker stopped: {err}") }))
}

/// [`work_file`] for `home`.
pub fn work_file_in(home: &Home, repo: &str, title: &str, body: &str, cwd: &Path) -> Value {
    let filed = checkout_of(home, repo, cwd)
        .and_then(|path| jono::file(home, &path, title, body))
        .and_then(|filed| Ok(serde_json::to_value(filed)?));
    filed.unwrap_or_else(error)
}

/// Where to run the core's commands for `repo`: the checkout Paja last worked in for it, else
/// the directory Desk started in when that is a checkout of `repo`.
fn checkout_of(home: &Home, repo: &str, cwd: &Path) -> anyhow::Result<PathBuf> {
    // The core asks `gh` which repository a checkout is, so a checkout whose remote has since
    // changed would act on another repository's item: it must still be `repo`.
    if let Some(path) = jono::known_checkout(home, repo)? {
        // A failing `gh` is its own error, not a reason to look elsewhere.
        if jono::repo_name(home, &path)? == repo {
            return Ok(path);
        }
    }
    if jono::repo_name(home, cwd).is_ok_and(|name| name == repo) {
        return Ok(cwd.to_path_buf());
    }
    anyhow::bail!(
        "Paja Desk doesn't know where {repo} is checked out yet: add it with its checkout folder \
         under Repositories (or run `paja work sync` there), or start Desk from inside it"
    )
}

/// The repositories Paja knows: every one with a work item or a remembered checkout, with the
/// checkout Desk would use (null when it doesn't know one) and where its runs stand
/// ([`Runner::status`]).
#[tauri::command]
async fn repos(runner: tauri::State<'_, Runner>) -> Result<Value, String> {
    let runner = runner.inner().clone();
    Ok(off_main(move |home| repos_in(home, &runner, &checkout())).await)
}

/// [`repos`] for `home`.
pub fn repos_in(home: &Home, runner: &Runner, cwd: &Path) -> Value {
    let listed = (|| -> anyhow::Result<Value> {
        let mut names: Vec<String> = jono::list_all_from(home, cwd)?
            .into_iter()
            .map(|item| item.repo)
            .filter(|repo| !repo.is_empty())
            .collect();
        names.extend(
            home.events()
                .read_all()?
                .iter()
                .filter(|e| e.kind == jono::EV_CHECKOUT)
                .filter_map(|e| e.data["repo"].as_str().map(str::to_string)),
        );
        names.sort();
        names.dedup();
        let repos: Vec<Value> = names
            .iter()
            .map(|repo| {
                let checkout = jono::known_checkout(home, repo).ok().flatten();
                json!({ "repo": repo, "checkout": checkout, "runs": runner.status(home, repo) })
            })
            .collect();
        Ok(Value::Array(repos))
    })();
    listed.unwrap_or_else(error)
}

/// Add a repository by its checkout folder: sync its issues there, as `paja work sync` does,
/// which also remembers the checkout. Returns the sync's JSON.
#[tauri::command]
async fn repo_add(path: String) -> Value {
    off_main(move |home| {
        let path = PathBuf::from(path.trim());
        jono::sync(home, &path)
            .and_then(|report| Ok(serde_json::to_value(report)?))
            .unwrap_or_else(error)
    })
    .await
}

/// `paja doctor` for `repo`'s checkout: the checks a run needs, each with its fix.
#[tauri::command]
async fn repo_doctor(repo: String) -> Value {
    off_main(move |home| {
        checkout_of(home, &repo, &checkout())
            .and_then(|path| paja::doctor::run_checks(home, &path))
            .and_then(|report| Ok(serde_json::to_value(report)?))
            .unwrap_or_else(error)
    })
    .await
}

/// Start `paja run` for `repo`: one item, or `every` to keep going. Refused while a check of
/// `paja doctor` fails (the error lists the fixes) or a run of `repo` is going.
#[tauri::command]
async fn run_start(
    runner: tauri::State<'_, Runner>,
    repo: String,
    every: bool,
) -> Result<Value, String> {
    let runner = runner.inner().clone();
    Ok(off_main(move |home| run_start_in(home, &runner, &repo, every, &checkout())).await)
}

/// [`run_start`] for `home`.
pub fn run_start_in(home: &Home, runner: &Runner, repo: &str, every: bool, cwd: &Path) -> Value {
    let started = checkout_of(home, repo, cwd).and_then(|path| {
        let report = paja::doctor::run_checks(home, &path)?;
        if !report.ok {
            let failed: Vec<String> = report
                .checks
                .iter()
                .filter(|c| c.status == paja::doctor::Status::Failed)
                .map(|c| match &c.fix {
                    Some(fix) => format!("{}: {} ({fix})", c.name, c.detail),
                    None => format!("{}: {}", c.name, c.detail),
                })
                .collect();
            anyhow::bail!(
                "paja doctor found what would stop a run of {repo}; fix it first:\n{}",
                failed.join("\n")
            );
        }
        runner.start(home, repo, &path, every)
    });
    started.unwrap_or_else(error)
}

/// Ask Desk's run of `repo` to stop after the step in flight, or `now`.
#[tauri::command]
async fn run_stop(
    runner: tauri::State<'_, Runner>,
    repo: String,
    now: bool,
) -> Result<Value, String> {
    let runner = runner.inner().clone();
    Ok(off_main(move |home| {
        runner
            .stop(&repo, now)
            .map(|()| runner.status(home, &repo))
            .unwrap_or_else(error)
    })
    .await)
}

/// Close Desk after the person chose what happens to its runs: `stop` sends each one `stop
/// now`; otherwise they keep going on their own.
#[tauri::command]
fn close_desk(app: tauri::AppHandle, runner: tauri::State<'_, Runner>, stop: bool) {
    if stop {
        for repo in runner.running() {
            let _ = runner.stop(&repo, true);
        }
    }
    app.exit(0);
}

/// Desk's version and the `paja` it found: the one installed next to Desk (an installer puts
/// it there), else the one on `PATH`. `{"desk", "paja", "paja_path", "matches"}`; `paja` is
/// null when none answered `paja --json --version`.
#[tauri::command]
async fn about() -> Value {
    tauri::async_runtime::spawn_blocking(|| about_with(&paja_candidates()))
        .await
        .unwrap_or_else(|err| json!({ "error": format!("Desk's worker stopped: {err}") }))
}

/// The `paja` Desk starts runs with: the first of [`paja_candidates`] that exists, else the
/// name alone, found on `PATH`.
fn paja_program() -> PathBuf {
    let candidates = paja_candidates();
    candidates
        .iter()
        .find(|p| p.is_file())
        .cloned()
        .unwrap_or_else(|| candidates.last().cloned().unwrap_or_else(|| "paja".into()))
}

/// The `paja` programs Desk tries, in order.
fn paja_candidates() -> Vec<PathBuf> {
    let name = if cfg!(windows) { "paja.exe" } else { "paja" };
    let mut found = Vec::new();
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf))
    {
        found.push(dir.join(name));
    }
    found.push(PathBuf::from(name));
    found
}

/// [`about`] with the `paja` programs to try.
pub fn about_with(candidates: &[PathBuf]) -> Value {
    let desk = env!("CARGO_PKG_VERSION");
    for program in candidates {
        let mut cmd = std::process::Command::new(program);
        let Ok(out) = paja::platform::no_console(&mut cmd)
            .args(["--json", "--version"])
            .stdin(std::process::Stdio::null())
            .output()
        else {
            continue;
        };
        let Ok(reply) = serde_json::from_slice::<Value>(&out.stdout) else {
            continue;
        };
        if let Some(version) = reply["version"].as_str() {
            return json!({
                "desk": desk,
                "paja": version,
                "paja_commit": reply["commit"],
                "paja_path": program,
                "matches": version == desk,
            });
        }
    }
    json!({ "desk": desk, "paja": null, "paja_path": null, "matches": false })
}

/// The directory Desk was started in: the checkout, when started from one.
fn checkout() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn error(err: anyhow::Error) -> Value {
    json!({ "error": format!("{err:#}") })
}

/// A fingerprint of the event log that changes whenever an event is appended: its length and
/// modification time. None while the file doesn't exist.
fn fingerprint(path: &Path) -> Option<(u64, std::time::SystemTime)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.len(), meta.modified().ok()?))
}

/// Tell the window each time the event log changes. The log is append-only, so a poll of its
/// size and time is enough, and works the same on every platform.
fn watch(app: tauri::AppHandle) {
    let Ok(home) = Home::from_env() else { return };
    let path = home.root().join("events.jsonl");
    std::thread::spawn(move || {
        let mut seen = fingerprint(&path);
        loop {
            std::thread::sleep(Duration::from_millis(750));
            let now = fingerprint(&path);
            if now != seen {
                seen = now;
                let _ = app.emit(CHANGED, ());
            }
        }
    });
}

/// Start Desk's window.
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(Runner::new(paja_program()))
        .on_window_event(|window, event| {
            // Closing while Desk's runs are going asks the person first.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                let running = window.state::<Runner>().running();
                if !running.is_empty() {
                    api.prevent_close();
                    let _ = window.emit(CLOSE_ASKED, running);
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            work_list,
            work_run,
            work_answer,
            work_file,
            about,
            repos,
            repo_add,
            repo_doctor,
            run_start,
            run_stop,
            close_desk
        ])
        .setup(|app| {
            watch(app.handle().clone());
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("Paja Desk could not start its window");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn import(home: &Home, repo: &str, number: u64, title: &str) {
        let lock = home.lock().unwrap();
        home.events()
            .append(
                &lock,
                jono::EV_IMPORTED,
                json!({
                    "repo": repo,
                    "number": number,
                    "title": title,
                    "body": "",
                    "url": format!("https://github.com/{repo}/issues/{number}"),
                }),
            )
            .unwrap();
    }

    #[test]
    fn a_recorded_run_becomes_the_run_view() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home")).unwrap();
        import(&home, "acme/app", 3, "Add a login page");
        assert_eq!(work_run_in(&home, "acme/app", 3), Value::Null);
        let at = |extra: Value| {
            let mut data = json!({ "repo": "acme/app", "number": 3 });
            data.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            data
        };
        let recorded = [
            (
                jono::EV_STATUS,
                at(json!({ "status": "active", "note": null })),
            ),
            ("run.started", at(json!({ "env": "app-3", "base": "abc" }))),
            (
                "run.step",
                at(json!({ "round": 1, "role": "build", "model": "opus" })),
            ),
            ("run.step", at(json!({ "round": 1, "role": "checks" }))),
            (
                "run.round",
                at(
                    json!({ "round": 1, "verdict": "checks_failed", "detail": "a test failed",
                           "steps": [{ "step": "build", "input_tokens": 1200, "output_tokens": 300,
                                       "cache_read_input_tokens": null,
                                       "cache_creation_input_tokens": null, "cost_usd": 0.25,
                                       "num_turns": 9, "duration_ms": 90000,
                                       "session_id": "s1" }] }),
                ),
            ),
            (
                "run.step",
                at(json!({ "round": 2, "role": "fix", "model": "opus" })),
            ),
        ];
        {
            let lock = home.lock().unwrap();
            for (kind, data) in recorded {
                home.events().append(&lock, kind, data).unwrap();
            }
        }
        let view = work_run_in(&home, "acme/app", 3);
        // No `paja run` holds the item in this test, so the run shows as interrupted.
        assert_eq!(view["state"], "interrupted", "{view}");
        assert_eq!(
            (&view["round"], &view["max_rounds"]),
            (&json!(2), &json!(3))
        );
        assert_eq!(view["step"]["role"], "fix", "{view}");
        assert!(view["step"]["started_ms"].as_u64().unwrap() > 0, "{view}");
        assert_eq!(view["steps"].as_array().unwrap().len(), 3, "{view}");
        let round = &view["rounds"][0];
        assert_eq!(round["verdict"], "checks_failed", "{view}");
        assert_eq!(round["steps"][0]["input_tokens"], 1200, "{view}");
        assert_eq!(round["steps"][0]["cost_usd"], 0.25, "{view}");
        assert_eq!(view["env"], Value::Null, "{view}");
        assert_eq!(view["pr"], Value::Null, "{view}");
    }

    #[test]
    fn the_list_command_returns_what_the_cli_prints() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home")).unwrap();
        import(&home, "acme/app", 3, "Add a login page");
        import(&home, "acme/lib", 1, "Fix the parser");
        {
            let lock = home.lock().unwrap();
            home.events()
                .append(
                    &lock,
                    jono::EV_STATUS,
                    json!({"repo": "acme/app", "number": 3, "status": "escalated", "note": "A or B?"}),
                )
                .unwrap();
        }
        let listed = work_list_in(&home, dir.path());
        // The CLI's `--json work list --all` prints exactly this value.
        let cli = serde_json::to_value(jono::list_all_from(&home, dir.path()).unwrap()).unwrap();
        assert_eq!(listed, cli);
        let items = listed.as_array().unwrap();
        assert_eq!(items.len(), 2, "{listed}");
        let app = items.iter().find(|i| i["repo"] == "acme/app").unwrap();
        assert_eq!(app["status"], "escalated");
        assert_eq!(app["note"], "A or B?");
    }

    #[test]
    fn an_error_is_the_text_the_cli_prints() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home")).unwrap();
        std::fs::write(home.root().join("events.jsonl"), "not json\n").unwrap();
        let listed = work_list_in(&home, dir.path());
        let cli = jono::list_all_from(&home, dir.path()).unwrap_err();
        assert_eq!(listed, json!({ "error": format!("{cli:#}") }));
    }

    /// A fake `gh` for every test (the variable is process-wide): it logs its arguments to
    /// `gh.log` in the checkout it runs in, fails when that checkout has a `gh-down` file, names
    /// the repository `acme/app` and prints a URL for a new issue. A shell script, so Unix only.
    #[cfg(unix)]
    fn fake_gh() {
        use std::os::unix::fs::PermissionsExt;
        static GH: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
        GH.get_or_init(|| {
            let dir = tempfile::tempdir().unwrap();
            let gh = dir.path().join("gh");
            std::fs::write(
                &gh,
                "#!/bin/sh\ncase \"$1\" in --version|auth) echo 'gh version 2'; exit 0 ;; esac\n\
                 [ -f gh-down ] && { echo 'HTTP 502: Bad Gateway' >&2; exit 1; }\n\
                 echo \"$@\" >> gh.log\n\
                 case \"$1 $2\" in\n\
                 \"repo view\") echo '{\"nameWithOwner\": \"acme/app\"}' ;;\n\
                 \"issue create\") echo https://github.com/acme/app/issues/9 ;;\n\
                 esac\n",
            )
            .unwrap();
            std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::env::set_var("PAJA_GH", &gh);
            dir
        });
    }

    /// A home with item 3 of acme/app escalated, and an environment Paja made for it from the
    /// checkout it returns.
    #[cfg(unix)]
    fn escalated_with_checkout(dir: &Path) -> (Home, PathBuf) {
        fake_gh();
        let home = Home::at(dir.join("home")).unwrap();
        let checkout = dir.join("app");
        std::fs::create_dir_all(&checkout).unwrap();
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&checkout)
            .status()
            .unwrap();
        assert!(init.success());
        import(&home, "acme/app", 3, "Add a login page");
        let lock = home.lock().unwrap();
        let log = home.events();
        log.append(
            &lock,
            jono::EV_STATUS,
            json!({"repo": "acme/app", "number": 3, "status": "escalated", "note": "A or B?"}),
        )
        .unwrap();
        log.append(
            &lock,
            paja::ahjo::EV_CREATED,
            json!({
                "name": jono::env_name("acme/app", 3),
                "repo": checkout,
                "path": dir.join("env"),
                "branch": "paja/x",
                "base_commit": "0",
                "port_base": 41000,
                "port_count": 10,
                "created_ms": 0,
            }),
        )
        .unwrap();
        (home, checkout)
    }

    #[cfg(unix)]
    #[test]
    fn answering_records_the_answer_and_queues_the_item() {
        let dir = tempfile::tempdir().unwrap();
        let (home, checkout) = escalated_with_checkout(dir.path());
        let item = work_answer_in(&home, "acme/app", 3, "Use A", dir.path());
        assert_eq!(item["status"], "queued", "{item}");
        assert_eq!(item["answers"][0]["answer"], "Use A", "{item}");
        let answered = home
            .events()
            .read_all()
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == jono::EV_ANSWERED)
            .count();
        assert_eq!(answered, 1);
        let log = std::fs::read_to_string(checkout.join("gh.log")).unwrap();
        assert!(log.contains("issue comment 3"), "{log}");
    }

    #[cfg(unix)]
    #[test]
    fn filing_opens_an_issue_with_the_work_label() {
        let dir = tempfile::tempdir().unwrap();
        let (home, checkout) = escalated_with_checkout(dir.path());
        let filed = work_file_in(&home, "acme/app", "Add search", "Goal: ...", dir.path());
        assert_eq!(
            filed["url"], "https://github.com/acme/app/issues/9",
            "{filed}"
        );
        let log = std::fs::read_to_string(checkout.join("gh.log")).unwrap();
        assert!(
            log.contains("issue create --label factory --title Add search"),
            "{log}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_gh_is_an_error_and_records_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (home, checkout) = escalated_with_checkout(dir.path());
        std::fs::write(checkout.join("gh-down"), "").unwrap();
        let before = home.events().read_all().unwrap().len();
        let answered = work_answer_in(&home, "acme/app", 3, "Use A", dir.path());
        let error = answered["error"].as_str().unwrap_or_default();
        assert!(error.contains("HTTP 502"), "{answered}");
        let filed = work_file_in(&home, "acme/app", "Add search", "Goal", dir.path());
        assert!(
            filed["error"]
                .as_str()
                .unwrap_or_default()
                .contains("HTTP 502"),
            "{filed}"
        );
        assert_eq!(home.events().read_all().unwrap().len(), before);
    }

    #[cfg(unix)]
    #[test]
    fn a_remembered_checkout_of_another_repository_is_not_used() {
        let dir = tempfile::tempdir().unwrap();
        // The fake gh names every checkout acme/app, so one remembered for acme/other is stale.
        let (home, checkout) = escalated_with_checkout(dir.path());
        import(&home, "acme/other", 3, "Same number elsewhere");
        let lock = home.lock().unwrap();
        home.events()
            .append(
                &lock,
                paja::ahjo::EV_CREATED,
                json!({
                    "name": jono::env_name("acme/other", 3),
                    "repo": checkout,
                    "path": dir.path().join("env2"),
                    "branch": "paja/y",
                    "base_commit": "0",
                    "port_base": 41010,
                    "port_count": 10,
                    "created_ms": 0,
                }),
            )
            .unwrap();
        drop(lock);
        let answered = work_answer_in(&home, "acme/other", 3, "Use A", dir.path());
        let error = answered["error"].as_str().unwrap_or_default();
        assert!(
            error.contains("doesn't know where acme/other is checked out"),
            "{answered}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_failing_doctor_check_blocks_starting_a_run() {
        let dir = tempfile::tempdir().unwrap();
        // The checkout has no `.paja/checks` on its default branch, which `paja doctor` fails.
        let (home, _) = escalated_with_checkout(dir.path());
        let runner = Runner::new(dir.path().join("no-paja"));
        let started = run_start_in(&home, &runner, "acme/app", false, dir.path());
        let error = started["error"].as_str().unwrap_or_default();
        assert!(error.contains("paja doctor found"), "{started}");
        assert!(runner.running().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn the_repository_list_has_each_repository_with_its_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let (home, checkout) = escalated_with_checkout(dir.path());
        import(&home, "acme/lib", 1, "Fix the parser");
        let runner = Runner::new(dir.path().join("no-paja"));
        let listed = repos_in(&home, &runner, dir.path());
        assert_eq!(listed[0]["repo"], "acme/app", "{listed}");
        assert_eq!(listed[0]["checkout"], json!(checkout), "{listed}");
        assert_eq!(listed[0]["runs"]["desk"], false, "{listed}");
        assert_eq!(listed[1]["repo"], "acme/lib", "{listed}");
        assert_eq!(listed[1]["checkout"], Value::Null, "{listed}");
    }

    #[test]
    fn a_repository_desk_has_no_checkout_for_says_where_to_start() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home")).unwrap();
        import(&home, "acme/app", 3, "Add a login page");
        let answered = work_answer_in(&home, "acme/app", 3, "Use A", dir.path());
        let error = answered["error"].as_str().unwrap_or_default();
        assert!(
            error.contains("doesn't know where acme/app is checked out"),
            "{answered}"
        );
    }

    #[test]
    fn desk_and_paja_share_one_version() {
        // The installer carries both, and Desk warns when they differ.
        let conf: Value = serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        assert_eq!(
            conf["version"],
            env!("CARGO_PKG_VERSION"),
            "desk/tauri.conf.json"
        );
        let core = include_str!("../../crates/paja/Cargo.toml");
        let line = core.lines().find(|l| l.starts_with("version = ")).unwrap();
        assert_eq!(
            line,
            format!("version = \"{}\"", env!("CARGO_PKG_VERSION")),
            "crates/paja/Cargo.toml"
        );
    }

    #[cfg(unix)]
    #[test]
    fn about_names_the_paja_it_found_and_whether_it_matches() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let fake = |name: &str, version: &str| {
            let path = dir.path().join(name);
            std::fs::write(
                &path,
                format!("#!/bin/sh\necho '{{\"version\": \"{version}\", \"commit\": \"abc\"}}'\n"),
            )
            .unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path
        };
        let same = fake("same", env!("CARGO_PKG_VERSION"));
        let old = fake("old", "0.0.1");
        let missing = dir.path().join("missing");
        let found = about_with(&[missing.clone(), same.clone()]);
        assert_eq!(found["paja"], env!("CARGO_PKG_VERSION"), "{found}");
        assert_eq!(found["matches"], true, "{found}");
        assert_eq!(found["paja_path"], json!(same));
        let differs = about_with(&[old]);
        assert_eq!(differs["matches"], false, "{differs}");
        let none = about_with(&[missing]);
        assert_eq!(none["paja"], Value::Null, "{none}");
    }

    #[test]
    fn the_fingerprint_changes_when_an_event_is_appended() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home")).unwrap();
        let path = home.root().join("events.jsonl");
        assert_eq!(fingerprint(&path), None);
        import(&home, "acme/app", 1, "One");
        let first = fingerprint(&path);
        assert!(first.is_some());
        import(&home, "acme/app", 2, "Two");
        assert_ne!(fingerprint(&path), first);
    }
}
