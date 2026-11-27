//! Runs Paja Desk starts: the installed `paja` binary as a child process (`paja run --once
//! --yes` or `paja run --every 5`) in the repository's checkout, so a run is exactly the CLI's,
//! with its own lock and hook. Desk stops one with `--stop-on-stdin`'s `stop` and `stop now`
//! lines, the same as the first and second Ctrl-C. The run's stdout and stderr go to a file in
//! the home (`desk/<owner>@<name>.log`), so a run that is left going when Desk closes has
//! somewhere to write.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use paja::home::Home;
use paja::jono::{self, Status};
use serde_json::{json, Value};

/// How often `paja run --every` checks for work when the queue is empty, in minutes.
pub const EVERY_MINUTES: u32 = 5;
/// How long "Stop now" waits for `paja` to end its agents before Desk ends `paja` itself.
pub const STOP_NOW_GRACE: Duration = Duration::from_secs(15);
/// How much of a run's output Desk shows.
const OUTPUT_LINES: usize = 200;

struct DeskRun {
    child: Child,
    stdin: Option<ChildStdin>,
    every: bool,
    workers: Option<u32>,
    started_ms: u64,
}

/// The runs this Desk started, by repository (`owner/name`). Clones share them.
#[derive(Clone)]
pub struct Runner {
    program: PathBuf,
    runs: Arc<Mutex<HashMap<String, DeskRun>>>,
}

impl Runner {
    /// A runner that starts `program` (the installed `paja`).
    pub fn new(program: PathBuf) -> Self {
        Self {
            program,
            runs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Start `paja run` for `repo` in `checkout`: one item, or `every` to keep going, with
    /// `workers` as a pool of that many lanes. Refused while a run or pool of `repo` is going,
    /// from Desk or a terminal.
    pub fn start(
        &self,
        home: &Home,
        repo: &str,
        checkout: &Path,
        every: bool,
        workers: Option<u32>,
    ) -> Result<Value> {
        if workers.is_some() && !every {
            bail!("a pool keeps running: start it with every");
        }
        let mut runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        if runs.get_mut(repo).is_some_and(|run| alive(&mut run.child)) {
            bail!("Desk is already running {repo}; stop that run first");
        }
        if paja::mestari::pool::status(home, repo)?.state == "running" {
            bail!(
                "a pool is already running for {repo} (started from a terminal or another \
                 Desk); change its lanes with the lane count, or stop it there first"
            );
        }
        if let Some(number) = running_item(home, repo)? {
            bail!(
                "a paja run is already working on {repo}#{number} (started from a terminal or \
                 another Desk); stop it there first"
            );
        }
        let log = log_path(home, repo);
        if let Some(dir) = log.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let out =
            std::fs::File::create(&log).with_context(|| format!("creating {}", log.display()))?;
        let mut cmd = Command::new(&self.program);
        paja::platform::no_console(&mut cmd)
            .current_dir(checkout)
            .env("PAJA_HOME", home.root())
            .args(["--json", "run", "--stop-on-stdin"])
            .stdin(Stdio::piped())
            .stdout(out.try_clone()?)
            .stderr(out);
        if every {
            cmd.args(["--every", &EVERY_MINUTES.to_string()]);
            if let Some(n) = workers {
                cmd.args(["--workers", &n.to_string()]);
            }
        } else {
            cmd.args(["--once", "--yes"]);
        }
        let mut child = cmd.spawn().with_context(|| {
            format!(
                "starting {} (install Paja, or Desk's installer, which includes it)",
                self.program.display()
            )
        })?;
        let stdin = child.stdin.take();
        runs.insert(
            repo.to_string(),
            DeskRun {
                child,
                stdin,
                every,
                workers,
                started_ms: now_ms(),
            },
        );
        drop(runs);
        Ok(self.status(home, repo))
    }

    /// Ask Desk's run of `repo` to stop: after the step in flight, or `now` (it ends its agents
    /// and checks; if it is still going after a grace period, Desk ends it).
    pub fn stop(&self, repo: &str, now: bool) -> Result<()> {
        let mut runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        let Some(run) = runs.get_mut(repo) else {
            bail!("Desk isn't running {repo}; a run started in a terminal stops with Ctrl-C there");
        };
        if !alive(&mut run.child) {
            bail!("Desk's run of {repo} has already ended");
        }
        let line = if now { "stop now\n" } else { "stop\n" };
        let sent = run
            .stdin
            .as_mut()
            .context("the run's stdin is closed")
            .and_then(|stdin| {
                stdin.write_all(line.as_bytes())?;
                stdin.flush()?;
                Ok(())
            });
        if now {
            // `paja` ends its children itself; this only ends a `paja` that is stuck.
            let pid = run.child.id();
            drop(runs);
            self.end_later(repo.to_string(), pid);
        }
        sent.with_context(|| format!("telling the run of {repo} to stop"))
    }

    /// Tell every run Desk started to stop now, and wait for them to end, up to `grace`; a
    /// `paja` still going then is ended. For closing Desk, which takes [`Self::end_later`]'s
    /// timers with it.
    pub fn stop_all_now(&self, grace: Duration) {
        for repo in self.running() {
            let mut runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(stdin) = runs.get_mut(&repo).and_then(|run| run.stdin.as_mut()) {
                let _ = stdin.write_all(b"stop now\n").and_then(|()| stdin.flush());
            }
        }
        let deadline = std::time::Instant::now() + grace;
        while !self.running().is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        let mut runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        for run in runs.values_mut() {
            if alive(&mut run.child) {
                let _ = run.child.kill();
                let _ = run.child.wait();
            }
        }
    }

    /// End `paja` (process `pid`) for `repo` if it is still going after the grace period.
    fn end_later(&self, repo: String, pid: u32) {
        let runs = Arc::clone(&self.runs);
        std::thread::spawn(move || {
            std::thread::sleep(STOP_NOW_GRACE);
            let mut runs = runs.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(run) = runs.get_mut(&repo) {
                if run.child.id() == pid && alive(&mut run.child) {
                    let _ = run.child.kill();
                    let _ = run.child.wait();
                }
            }
        });
    }

    /// Whether Desk's run of `repo` is still going.
    pub fn is_running(&self, repo: &str) -> bool {
        let mut runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        runs.get_mut(repo).is_some_and(|run| alive(&mut run.child))
    }

    /// The repositories Desk is running.
    pub fn running(&self) -> Vec<String> {
        let mut runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        let mut names: Vec<String> = runs
            .iter_mut()
            .filter_map(|(repo, run)| alive(&mut run.child).then(|| repo.clone()))
            .collect();
        names.sort();
        names
    }

    /// Where runs of `repo` stand, for the window: `{"repo", "desk", "every", "workers",
    /// "started_ms", "terminal", "output"}`. `desk` says Desk's own run is going; `terminal` names an item a
    /// run Desk didn't start is working on; `output` is the end of Desk's last run's output.
    pub fn status(&self, home: &Home, repo: &str) -> Value {
        let mut runs = self.runs.lock().unwrap_or_else(|e| e.into_inner());
        let mine = runs
            .get_mut(repo)
            .map(|run| (alive(&mut run.child), run.every, run.started_ms));
        let workers = runs.get(repo).and_then(|run| run.workers);
        drop(runs);
        let desk = mine.is_some_and(|(alive, _, _)| alive);
        let terminal = if desk {
            None
        } else {
            running_item(home, repo).ok().flatten()
        };
        json!({
            "repo": repo,
            "desk": desk,
            "every": mine.map(|(_, every, _)| every),
            "workers": workers,
            "started_ms": mine.map(|(_, _, started)| started),
            "terminal": terminal,
            "output": tail(&log_path(home, repo), OUTPUT_LINES),
        })
    }
}

/// Whether `child` is still running; a child that can't be asked counts as ended.
fn alive(child: &mut Child) -> bool {
    matches!(child.try_wait(), Ok(None))
}

/// An active item of `repo` that a live `paja run` holds, if any. Like the run view, it probes
/// each item's run lock under the home lock, so a run claiming an item never meets the probe.
pub fn running_item(home: &Home, repo: &str) -> Result<Option<u64>> {
    let events = home.events().read_all()?;
    let items = jono::replay_repo(&events, repo)?;
    for item in items.values().filter(|i| i.status == Status::Active) {
        let _home = home.lock()?;
        if home.try_run_lock(repo, item.number)?.is_none() {
            return Ok(Some(item.number));
        }
    }
    Ok(None)
}

/// The file Desk's runs of `repo` write their output to.
pub fn log_path(home: &Home, repo: &str) -> PathBuf {
    home.root()
        .join("desk")
        .join(format!("{}.log", repo.replace('/', "@")))
}

/// The last `lines` lines of `path`, or "" when it can't be read.
fn tail(path: &Path, lines: usize) -> String {
    let text = std::fs::read(path)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A fake `paja`: logs its arguments and working directory, then waits for a `stop` line on
    /// stdin and exits, as `paja run --stop-on-stdin` stops after its step.
    fn fake_paja(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("paja");
        std::fs::write(
            &path,
            "#!/bin/sh\necho \"args: $*\"\necho \"in: $(pwd)\"\n\
             while read -r line; do\n  echo \"got: $line\"\n  \
             case \"$line\" in stop*) exit 0 ;; esac\ndone\n",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let started = std::time::Instant::now();
        while !done() {
            assert!(started.elapsed().as_secs() < 10, "{what} never happened");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    #[test]
    fn a_run_starts_refuses_a_second_and_stops_after_its_step() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home")).unwrap();
        let checkout = dir.path().join("app");
        std::fs::create_dir_all(&checkout).unwrap();
        let runner = Runner::new(fake_paja(dir.path()));

        let started = runner
            .start(&home, "acme/app", &checkout, false, None)
            .unwrap();
        assert_eq!(started["desk"], true, "{started}");
        assert_eq!(started["every"], false, "{started}");
        assert_eq!(runner.running(), ["acme/app"]);
        // A second run of the same repository is refused while the first is going.
        let again = runner
            .start(&home, "acme/app", &checkout, true, None)
            .unwrap_err();
        assert!(
            format!("{again:#}").contains("already running acme/app"),
            "{again:#}"
        );

        runner.stop("acme/app", false).unwrap();
        wait_until("the run ending", || !runner.is_running("acme/app"));
        let status = runner.status(&home, "acme/app");
        assert_eq!(status["desk"], false, "{status}");
        let output = status["output"].as_str().unwrap();
        assert!(
            output.contains("args: --json run --stop-on-stdin --once --yes"),
            "{output}"
        );
        assert!(
            output.contains(&format!("in: {}", checkout.display())),
            "{output}"
        );
        assert!(output.contains("got: stop"), "{output}");
        // Once it ended, Desk can start the repository again, this time to keep running.
        let looping = runner
            .start(&home, "acme/app", &checkout, true, None)
            .unwrap();
        assert_eq!(looping["every"], true, "{looping}");
        runner.stop("acme/app", true).unwrap();
        wait_until("the loop ending", || !runner.is_running("acme/app"));
        let output = runner.status(&home, "acme/app")["output"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(output.contains("--every 5"), "{output}");
        assert!(output.contains("got: stop now"), "{output}");
        assert!(runner.stop("acme/app", false).is_err());
    }

    #[test]
    fn closing_stops_every_run_and_ends_one_that_is_stuck() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home")).unwrap();
        let checkout = dir.path().join("app");
        std::fs::create_dir_all(&checkout).unwrap();
        // A `paja` that reads stop lines and never ends.
        let stuck = dir.path().join("stuck");
        std::fs::write(
            &stuck,
            "#!/bin/sh\nwhile read -r line; do echo \"got: $line\"; done\nsleep 60\n",
        )
        .unwrap();
        std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o755)).unwrap();
        let fine = Runner::new(fake_paja(dir.path()));
        fine.start(&home, "acme/app", &checkout, false, None)
            .unwrap();
        let started = std::time::Instant::now();
        fine.stop_all_now(Duration::from_secs(10));
        // The run that stops on `stop now` isn't waited on for the whole grace period.
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(fine.running().is_empty());

        let stuck = Runner::new(stuck);
        stuck
            .start(&home, "acme/lib", &checkout, false, None)
            .unwrap();
        stuck.stop_all_now(Duration::from_millis(500));
        assert!(stuck.running().is_empty());
        let output = stuck.status(&home, "acme/lib")["output"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(output.contains("got: stop now"), "{output}");
    }

    #[test]
    fn a_pool_starts_with_its_lanes_and_one_elsewhere_blocks_it() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home")).unwrap();
        let checkout = dir.path().join("app");
        std::fs::create_dir_all(&checkout).unwrap();
        let runner = Runner::new(fake_paja(dir.path()));
        assert!(runner
            .start(&home, "acme/app", &checkout, false, Some(2))
            .is_err());
        let started = runner
            .start(&home, "acme/app", &checkout, true, Some(3))
            .unwrap();
        assert_eq!(started["workers"], 3, "{started}");
        runner.stop("acme/app", false).unwrap();
        wait_until("the pool ending", || !runner.is_running("acme/app"));
        let output = runner.status(&home, "acme/app")["output"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(output.contains("--every 5 --workers 3"), "{output}");
        // A pool a terminal started holds the pool lock and recorded its start.
        {
            let lock = home.lock().unwrap();
            home.events()
                .append(
                    &lock,
                    paja::mestari::pool::EV_POOL_STARTED,
                    json!({"repo": "acme/app", "lanes": 2}),
                )
                .unwrap();
        }
        let held = home.try_pool_lock("acme/app").unwrap().unwrap();
        let refused = runner
            .start(&home, "acme/app", &checkout, true, Some(2))
            .unwrap_err();
        assert!(
            format!("{refused:#}").contains("pool is already running"),
            "{refused:#}"
        );
        drop(held);
    }

    #[test]
    fn a_run_started_elsewhere_blocks_a_desk_run() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home")).unwrap();
        let checkout = dir.path().join("app");
        std::fs::create_dir_all(&checkout).unwrap();
        {
            let lock = home.lock().unwrap();
            let log = home.events();
            log.append(
                &lock,
                jono::EV_IMPORTED,
                json!({"repo": "acme/app", "number": 4, "title": "t", "body": "", "url": "u"}),
            )
            .unwrap();
            log.append(
                &lock,
                jono::EV_STATUS,
                json!({"repo": "acme/app", "number": 4, "status": "active", "note": null}),
            )
            .unwrap();
        }
        // A terminal's `paja run` holds the item's run lock.
        let held = home.try_run_lock("acme/app", 4).unwrap().unwrap();
        let runner = Runner::new(fake_paja(dir.path()));
        assert_eq!(runner.status(&home, "acme/app")["terminal"], 4);
        let refused = runner
            .start(&home, "acme/app", &checkout, false, None)
            .unwrap_err();
        assert!(format!("{refused:#}").contains("acme/app#4"), "{refused:#}");
        drop(held);
        assert_eq!(runner.status(&home, "acme/app")["terminal"], Value::Null);
        runner
            .start(&home, "acme/app", &checkout, false, None)
            .unwrap();
        runner.stop("acme/app", false).unwrap();
        wait_until("the run ending", || !runner.is_running("acme/app"));
    }
}
