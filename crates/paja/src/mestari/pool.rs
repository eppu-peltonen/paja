//! A pool of lanes: `paja run --every M --workers N`. One process keeps up to N lanes busy, each
//! a `paja run --item` of its own (today's build, check, review and land loop for one item, in
//! its own Ahjo environment), so the lanes share nothing but the home and its locks.
//!
//! The pool syncs Jono once for all lanes, at the start, after a lane finishes and every M
//! minutes while a lane is free, and each tick hands every free lane the next item a run could
//! take ([`jono::runnable`]). The number of lanes is the latest `pool.lanes` event, so
//! `paja pool lanes <N>` changes it while the pool runs. Every change of a lane is a `pool.lane`
//! event, and [`status`] rebuilds the pool from the log alone.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};

use super::{progress, EV_LANDED, EV_ROUND, EV_STEP};
use crate::events::Event;
use crate::home::Home;
use crate::jono::{self, Status, WorkItem};

/// A pool started: `{repo, lanes, pid}`.
pub const EV_POOL_STARTED: &str = "pool.started";
/// The number of lanes a repository's pool keeps busy: `{repo, lanes}`.
pub const EV_POOL_LANES: &str = "pool.lanes";
/// A lane's item or stage changed: `{repo, lane, number, stage, step}`, with `outcome` once
/// `stage` is `done`.
pub const EV_POOL_LANE: &str = "pool.lane";
/// A pool stopped: `{repo}`, with `error` when it stopped on one.
pub const EV_POOL_STOPPED: &str = "pool.stopped";

/// The most lanes one pool runs.
pub const MAX_LANES: u32 = 32;

/// How often the pool looks at its lanes and the lane count.
const TICK: Duration = Duration::from_millis(500);

pub struct PoolOptions<'a> {
    /// Any path inside the repository's main checkout.
    pub repo: &'a Path,
    /// The lanes to start with (`--workers`).
    pub lanes: u32,
    /// Sync again this many minutes after the last sync while a lane is free (`--every`).
    pub every: u64,
    /// Print each lane's report as JSON (`--json`) rather than text.
    pub json: bool,
}

/// One busy lane: the `paja run` working on its item.
struct Lane {
    number: u64,
    child: Child,
    stdin: Option<ChildStdin>,
    report: mpsc::Receiver<String>,
    /// The last event this lane's stage was read from: at first the `pool.lane` event that
    /// started it, as the item's steps after it are this lane's.
    seen_seq: u64,
    stage: String,
    step: Option<String>,
}

impl Lane {
    /// Pass a stop on to the lane's run, as Ctrl-C or a `stop` line did to the pool.
    fn tell(&mut self, line: &str) {
        if let Some(stdin) = self.stdin.as_mut() {
            let _ = writeln!(stdin, "{line}").and_then(|()| stdin.flush());
        }
    }
}

/// Run the pool until Ctrl-C (or a `stop` line) and its lanes' current steps are done, or a
/// repository's git configuration is tampered with. Prints one value per finished item on
/// stdout, as `paja run --every` does, with its lane.
pub fn run(home: &Home, opts: PoolOptions<'_>) -> Result<()> {
    if opts.lanes == 0 || opts.lanes > MAX_LANES {
        bail!("--workers takes 1 to {MAX_LANES} lanes");
    }
    let repo = std::path::absolute(opts.repo)?;
    let name = super::sync_queue(home, &repo)?;
    let Some(_pool) = home.try_pool_lock(&name)? else {
        bail!(
            "a pool is already running for {name} (see `paja pool status`); change its lanes \
             with `paja pool lanes <N>`, or stop it first"
        );
    };
    record(
        home,
        EV_POOL_LANES,
        json!({ "repo": name, "lanes": opts.lanes }),
    )?;
    record(
        home,
        EV_POOL_STARTED,
        json!({ "repo": name, "lanes": opts.lanes, "pid": std::process::id() }),
    )?;
    progress(&format!(
        "pool of {name}: up to {} lanes (`paja pool lanes <N>` changes it; Ctrl+C stops)",
        opts.lanes
    ));
    let outcome = Pool {
        home,
        repo: &repo,
        name: &name,
        opts: &opts,
        lanes: BTreeMap::new(),
        held_back: BTreeSet::new(),
    }
    .run();
    let stopped = match &outcome {
        Ok(()) => json!({ "repo": name }),
        Err(err) => json!({ "repo": name, "error": format!("{err:#}") }),
    };
    record(home, EV_POOL_STOPPED, stopped)?;
    progress(&format!("pool of {name} stopped"));
    outcome
}

struct Pool<'a> {
    home: &'a Home,
    repo: &'a Path,
    name: &'a str,
    opts: &'a PoolOptions<'a>,
    lanes: BTreeMap<u32, Lane>,
    /// Items whose lane ended on an error: not handed out again until the next timed sync, as
    /// `paja run --every` waits after a failed pass.
    held_back: BTreeSet<u64>,
}

impl Pool<'_> {
    fn run(&mut self) -> Result<()> {
        let every = Duration::from_secs(self.opts.every.saturating_mul(60));
        // The first sync was the one that named the repository.
        let mut last_sync = Instant::now();
        let mut need_sync = false;
        let mut said_idle = false;
        let mut forwarded = 0;
        let mut fatal: Option<anyhow::Error> = None;
        loop {
            let level = crate::platform::interrupts();
            if level > forwarded {
                let line = if level >= 2 { "stop now" } else { "stop" };
                for lane in self.lanes.values_mut() {
                    lane.tell(line);
                }
                forwarded = level;
            }
            let done: Vec<u32> = self
                .lanes
                .iter_mut()
                .filter_map(|(&n, lane)| (!matches!(lane.child.try_wait(), Ok(None))).then_some(n))
                .collect();
            for n in done {
                if let Some(lane) = self.lanes.remove(&n) {
                    self.finish(n, lane)?;
                    need_sync = true;
                }
            }
            let stopping = level > 0 || fatal.is_some();
            if stopping && self.lanes.is_empty() {
                return fatal.map_or(Ok(()), Err);
            }
            let events = self.home.events().read_all()?;
            self.follow_stages(&events)?;
            if !stopping {
                let target = target_lanes(&events, self.name).unwrap_or(self.opts.lanes);
                let free = (target as usize).saturating_sub(self.lanes.len());
                if free > 0 {
                    let timed = last_sync.elapsed() >= every;
                    if need_sync || timed {
                        need_sync = false;
                        last_sync = Instant::now();
                        said_idle = false;
                        if timed {
                            self.held_back.clear();
                        }
                        match super::sync_queue(self.home, self.repo) {
                            Ok(_) => {}
                            Err(err) if super::is_fatal(&err) => {
                                fatal = Some(err);
                                continue;
                            }
                            Err(err) => self.say_error(&err),
                        }
                    }
                    let admitted = self.admit(free)?;
                    if admitted == 0 && self.lanes.is_empty() && !said_idle {
                        said_idle = true;
                        self.idle()?;
                    }
                }
            }
            std::thread::sleep(TICK);
        }
    }

    /// Hand up to `free` lanes the next items a run could take. Returns how many started.
    fn admit(&mut self, free: usize) -> Result<usize> {
        let busy: BTreeSet<u64> = self.lanes.values().map(|lane| lane.number).collect();
        let next: Vec<WorkItem> = match jono::runnable(self.home, self.name) {
            Ok(items) => items,
            Err(err) => {
                self.say_error(&err);
                return Ok(0);
            }
        };
        let next: Vec<WorkItem> = next
            .into_iter()
            .filter(|item| !busy.contains(&item.number) && !self.held_back.contains(&item.number))
            .take(free)
            .collect();
        let mut started = 0;
        for item in next {
            let lane = (1..).find(|n| !self.lanes.contains_key(n)).unwrap_or(1);
            let start = record_lane(self.home, self.name, lane, item.number, "starting", None)?;
            match spawn(self.repo, lane, item.number) {
                Ok((child, stdin, report)) => {
                    progress(&format!("lane {lane}: #{} {}", item.number, item.title));
                    self.lanes.insert(
                        lane,
                        Lane {
                            number: item.number,
                            child,
                            stdin,
                            report,
                            seen_seq: start.seq,
                            stage: "starting".to_string(),
                            step: None,
                        },
                    );
                    started += 1;
                }
                Err(err) => {
                    let mut done = record_lane_data(self.name, lane, item.number, "done", None);
                    done["outcome"] = json!("error");
                    record(self.home, EV_POOL_LANE, done)?;
                    self.held_back.insert(item.number);
                    self.say_error(&err);
                }
            }
        }
        Ok(started)
    }

    /// Record each step a busy lane's run started since the last tick.
    fn follow_stages(&mut self, events: &[Event]) -> Result<()> {
        for (&n, lane) in self.lanes.iter_mut() {
            follow_lane(self.home, self.name, n, lane, events)?;
        }
        Ok(())
    }

    /// A lane's run is over: print its report and free the lane.
    fn finish(&mut self, n: u32, mut lane: Lane) -> Result<()> {
        let status = lane.child.wait().ok();
        // The steps it started since the last tick, landing included, before it is done.
        let events = self.home.events().read_all()?;
        follow_lane(self.home, self.name, n, &mut lane, &events)?;
        let out = lane
            .report
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_default();
        let mut report: Value = serde_json::from_str(out.trim()).unwrap_or_else(|_| {
            json!({
                "error": format!(
                    "lane {n}'s run of #{} ended ({}) without a report",
                    lane.number,
                    status.map_or("unknown".to_string(), |s| s.to_string())
                )
            })
        });
        let outcome = if report.get("error").is_some() {
            "error".to_string()
        } else {
            report["outcome"].as_str().unwrap_or("unknown").to_string()
        };
        if outcome == "error" {
            self.held_back.insert(lane.number);
        }
        let mut done = record_lane_data(self.name, n, lane.number, "done", None);
        done["outcome"] = json!(outcome);
        record(self.home, EV_POOL_LANE, done)?;
        if let Some(fields) = report.as_object_mut() {
            fields.insert("lane".to_string(), json!(n));
            fields
                .entry("number".to_string())
                .or_insert(json!(lane.number));
        }
        if self.opts.json {
            println!("{report}");
        } else {
            println!("{}", describe_report(n, lane.number, &report));
        }
        Ok(())
    }

    /// Nothing to start and no lane busy: the time for the daily triage, as for `--every`.
    fn idle(&mut self) -> Result<()> {
        if let Some(waiting) = jono::waiting(self.home, self.name)? {
            progress(&format!("nothing is ready: {waiting}"));
        }
        match super::triage_if_due(self.home, self.repo) {
            Ok(Some(report)) => {
                if self.opts.json {
                    println!("{}", json!({ "outcome": "triage", "triage": report }));
                } else {
                    println!("triage: {}", serde_json::to_string(&report)?);
                }
            }
            Ok(None) => {}
            Err(err) if super::is_fatal(&err) => return Err(err),
            Err(err) => self.say_error(&err),
        }
        let minutes = self.opts.every;
        progress(&format!(
            "nothing to do right now; checking again in {minutes} minute{} (Ctrl+C stops)",
            if minutes == 1 { "" } else { "s" }
        ));
        Ok(())
    }

    fn say_error(&self, err: &anyhow::Error) {
        if self.opts.json {
            println!("{}", json!({ "error": format!("{err:#}") }));
        } else {
            println!("error: {err:#}");
        }
    }
}

/// Record each step lane `n`'s run started after the last one read, in order, as the lane's
/// stage and step.
fn follow_lane(home: &Home, repo: &str, n: u32, lane: &mut Lane, events: &[Event]) -> Result<()> {
    let steps: Vec<&Event> = events
        .iter()
        .filter(|e| {
            e.seq > lane.seen_seq
                && e.kind == EV_STEP
                && e.data["repo"] == repo
                && e.data["number"] == lane.number
        })
        .collect();
    for step in steps {
        lane.seen_seq = step.seq;
        let role = step.data["role"].as_str().unwrap_or("unknown").to_string();
        let stage = stage_of(&role).to_string();
        if lane.stage != stage || lane.step.as_deref() != Some(role.as_str()) {
            record_lane(home, repo, n, lane.number, &stage, Some(&role))?;
            lane.stage = stage;
            lane.step = Some(role);
        }
    }
    Ok(())
}

/// Start lane `lane`'s run of item `number`: `paja --json run --item <number>` in the checkout,
/// which skips the sync the pool did and reads `stop` lines on stdin. Its progress lines are
/// passed on to stderr with the lane in front; its report comes back on the channel.
fn spawn(
    repo: &Path,
    lane: u32,
    number: u64,
) -> Result<(Child, Option<ChildStdin>, mpsc::Receiver<String>)> {
    let exe = std::env::current_exe().context("finding the paja program to start a lane with")?;
    let mut child = Command::new(exe)
        .args(["--json", "run", "--yes", "--stop-on-stdin", "--lane"])
        .arg("--item")
        .arg(number.to_string())
        .arg("--repo")
        .arg(repo)
        .current_dir(repo)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting lane {lane}'s `paja run --item {number}`"))?;
    let stdin = child.stdin.take();
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                eprintln!("lane {lane} #{number}: {line}");
            }
        });
    }
    let (tx, rx) = mpsc::channel();
    if let Some(mut stdout) = child.stdout.take() {
        std::thread::spawn(move || {
            let mut out = String::new();
            let _ = stdout.read_to_string(&mut out);
            let _ = tx.send(out);
        });
    }
    Ok((child, stdin, rx))
}

/// The stage a run's step belongs to: `build` (building or fixing), `checks`, `review` or
/// `land` (waiting for CI and merging).
fn stage_of(role: &str) -> &'static str {
    match role {
        "checks" => "checks",
        "ci" | "land" => "land",
        role if role.starts_with("review") => "review",
        _ => "build",
    }
}

fn describe_report(lane: u32, number: u64, report: &Value) -> String {
    if let Some(error) = report["error"].as_str() {
        return format!("lane {lane}: #{number} error: {error}");
    }
    let outcome = report["outcome"].as_str().unwrap_or("finished");
    match report["pr"].as_str() {
        Some(pr) => format!("lane {lane}: #{number} {outcome} ({pr})"),
        None => format!("lane {lane}: #{number} {outcome}"),
    }
}

fn record(home: &Home, kind: &str, data: Value) -> Result<Event> {
    let lock = home.lock()?;
    home.events().append(&lock, kind, data)
}

fn record_lane_data(repo: &str, lane: u32, number: u64, stage: &str, step: Option<&str>) -> Value {
    json!({ "repo": repo, "lane": lane, "number": number, "stage": stage, "step": step })
}

fn record_lane(
    home: &Home,
    repo: &str,
    lane: u32,
    number: u64,
    stage: &str,
    step: Option<&str>,
) -> Result<Event> {
    record(
        home,
        EV_POOL_LANE,
        record_lane_data(repo, lane, number, stage, step),
    )
}

/// The lane count `repo`'s pool keeps: its latest `pool.lanes`, or None when none was set.
fn target_lanes(events: &[Event], repo: &str) -> Option<u32> {
    events
        .iter()
        .rev()
        .find(|e| e.kind == EV_POOL_LANES && e.data["repo"] == repo)
        .and_then(|e| e.data["lanes"].as_u64())
        .map(|n| n as u32)
}

/// What `paja pool lanes` reports.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Lanes {
    pub repo: String,
    /// The lane count the pool keeps; None when none was ever set.
    pub lanes: Option<u32>,
    /// Whether a pool of `repo` is running now.
    pub running: bool,
}

/// Set the lane count of `repo`'s pool to `lanes`, or with None show it. A running pool reads
/// it on its next tick: more lanes start then; fewer let the running items finish and start no
/// new ones until the busy lanes are under the count. Without a running pool, it is the count
/// the next pool's `--workers` replaces.
pub fn lanes(home: &Home, repo: &str, lanes: Option<u32>) -> Result<Lanes> {
    if let Some(n) = lanes {
        if n > MAX_LANES {
            bail!(
                "a pool runs 0 to {MAX_LANES} lanes; 0 lets the busy lanes finish and starts none"
            );
        }
        record(home, EV_POOL_LANES, json!({ "repo": repo, "lanes": n }))?;
    }
    let events = home.events().read_all()?;
    Ok(Lanes {
        repo: repo.to_string(),
        lanes: target_lanes(&events, repo),
        running: pool_running(home, repo)?,
    })
}

/// Whether a live pool holds `repo`'s pool lock: taken and let go of at once, under the home
/// lock as pools take it... a probe only.
fn pool_running(home: &Home, repo: &str) -> Result<bool> {
    let _home = home.lock()?;
    Ok(home.try_pool_lock(repo)?.is_none())
}

/// `paja pool status`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PoolStatus {
    pub repo: String,
    /// `running` or `stopped`.
    pub state: String,
    /// The lane count the pool keeps.
    pub target: Option<u32>,
    /// How many lanes are working on an item.
    pub busy: usize,
    pub lanes: Vec<LaneStatus>,
    /// The items on a lane.
    pub admitted: Vec<u64>,
    /// The queued items not on a lane, with why.
    pub waiting: Vec<WaitingItem>,
    /// The latest landings of the repository, newest first.
    pub landed: Vec<Landing>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LaneStatus {
    pub lane: u32,
    pub number: u64,
    pub title: String,
    /// `starting`, `build`, `checks`, `review` or `land`.
    pub stage: String,
    /// The run's step (`build`, `fix`, `checks`, `review`, `ci`, `land`, ...), once one started.
    pub step: Option<String>,
    /// When the lane entered this stage and step, in milliseconds since the Unix epoch.
    pub since_ms: u64,
    /// What the item's finished rounds on this lane spent, in US dollars (Claude Code's
    /// estimate).
    pub spent_usd: f64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct WaitingItem {
    pub number: u64,
    pub title: String,
    pub why: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Landing {
    pub number: u64,
    pub pr: Option<String>,
    pub ts_ms: u64,
}

/// The pool of `repo`, rebuilt from the log.
pub fn status(home: &Home, repo: &str) -> Result<PoolStatus> {
    let events = home.events().read_all()?;
    let items = jono::list(home, repo)?;
    let running = pool_running(home, repo)?;
    Ok(from_events(&events, repo, running, &items))
}

/// How many landings [`status`] shows.
const LANDINGS: usize = 5;

/// [`status`] over events already read; `running` says whether a live pool holds the lock, and
/// `items` are the repository's work items in queue order.
pub fn from_events(events: &[Event], repo: &str, running: bool, items: &[WorkItem]) -> PoolStatus {
    let mine = |e: &&Event| e.data["repo"] == repo;
    let started = events
        .iter()
        .rposition(|e| e.kind == EV_POOL_STARTED && e.data["repo"] == repo);
    let stopped_after = started.is_some_and(|start| {
        events[start..]
            .iter()
            .filter(mine)
            .any(|e| e.kind == EV_POOL_STOPPED)
    });
    let running = running && started.is_some() && !stopped_after;
    // Each lane's latest change, and the seq that started its item, since the pool started.
    let mut lanes: BTreeMap<u32, (&Event, u64)> = BTreeMap::new();
    if let (true, Some(start)) = (running, started) {
        for event in events[start..]
            .iter()
            .filter(mine)
            .filter(|e| e.kind == EV_POOL_LANE)
        {
            let Some(lane) = event.data["lane"].as_u64() else {
                continue;
            };
            let begun = if event.data["stage"] == "starting" {
                event.seq
            } else {
                lanes.get(&(lane as u32)).map_or(event.seq, |(_, s)| *s)
            };
            lanes.insert(lane as u32, (event, begun));
        }
    }
    let title = |number: u64| {
        items
            .iter()
            .find(|item| item.number == number)
            .map(|item| item.title.clone())
            .unwrap_or_default()
    };
    let lanes: Vec<LaneStatus> = lanes
        .into_iter()
        .filter(|(_, (event, _))| event.data["stage"] != "done")
        .filter_map(|(lane, (event, begun))| {
            let number = event.data["number"].as_u64()?;
            let spent_usd = events
                .iter()
                .filter(|e| {
                    e.seq > begun
                        && e.kind == EV_ROUND
                        && e.data["repo"] == repo
                        && e.data["number"] == number
                })
                .flat_map(|e| e.data["steps"].as_array().cloned().unwrap_or_default())
                .filter_map(|step| step["cost_usd"].as_f64())
                .sum::<f64>()
                // An empty sum is -0.0, which JSON shows as such.
                + 0.0;
            Some(LaneStatus {
                lane,
                number,
                title: title(number),
                stage: event.data["stage"].as_str().unwrap_or_default().to_string(),
                step: event.data["step"].as_str().map(str::to_string),
                since_ms: event.ts_ms,
                spent_usd,
            })
        })
        .collect();
    let admitted: Vec<u64> = lanes.iter().map(|lane| lane.number).collect();
    let waiting = items
        .iter()
        .filter(|item| item.open && item.status == Status::Queued)
        .filter(|item| !admitted.contains(&item.number))
        .map(|item| WaitingItem {
            number: item.number,
            title: item.title.clone(),
            why: if !item.waiting_on.is_empty() {
                let on: Vec<String> = item.waiting_on.iter().map(|n| format!("#{n}")).collect();
                format!("waits on {}", on.join(", "))
            } else if running {
                "no free lane".to_string()
            } else {
                "no pool is running".to_string()
            },
        })
        .collect();
    let landed = events
        .iter()
        .rev()
        .filter(|e| e.kind == EV_LANDED && e.data["repo"] == repo)
        .filter_map(|e| {
            Some(Landing {
                number: e.data["number"].as_u64()?,
                pr: e.data["pr"].as_str().map(str::to_string),
                ts_ms: e.ts_ms,
            })
        })
        .take(LANDINGS)
        .collect();
    PoolStatus {
        repo: repo.to_string(),
        state: if running { "running" } else { "stopped" }.to_string(),
        target: target_lanes(events, repo),
        busy: lanes.len(),
        admitted,
        lanes,
        waiting,
        landed,
    }
}

/// `paja pool status` as text: a line for the pool, a table of its lanes, then what waits and
/// what landed. `now_ms` is the clock the times in step are read against.
pub fn describe(status: &PoolStatus, now_ms: u64) -> String {
    let target = status
        .target
        .map_or("no lane count set".to_string(), |n| format!("{n} lanes"));
    let mut out = vec![format!(
        "pool of {}: {}, {} busy of {target}",
        status.repo, status.state, status.busy
    )];
    if !status.lanes.is_empty() {
        out.push("lane  item    stage     step      in step  spent".to_string());
        for lane in &status.lanes {
            let secs = now_ms.saturating_sub(lane.since_ms) / 1000;
            out.push(format!(
                "{:<5} {:<7} {:<9} {:<9} {:>7}  ${:.2}",
                lane.lane,
                format!("#{}", lane.number),
                lane.stage,
                lane.step.as_deref().unwrap_or("-"),
                format!("{}m{:02}s", secs / 60, secs % 60),
                lane.spent_usd
            ));
        }
    }
    for item in &status.waiting {
        out.push(format!(
            "waiting: #{} {} ({})",
            item.number, item.title, item.why
        ));
    }
    for landing in &status.landed {
        out.push(format!(
            "landed: #{}{}",
            landing.number,
            landing
                .pr
                .as_deref()
                .map(|pr| format!(" ({pr})"))
                .unwrap_or_default()
        ));
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(seq: u64, kind: &str, data: Value) -> Event {
        Event {
            v: 1,
            seq,
            ts_ms: seq * 1000,
            kind: kind.to_string(),
            data,
        }
    }

    fn item(number: u64, waiting_on: &[u64]) -> WorkItem {
        serde_json::from_value(json!({
            "repo": "acme/app", "number": number, "title": format!("item {number}"), "body": "",
            "url": "u", "open": true, "status": "queued", "waiting_on": waiting_on,
        }))
        .unwrap()
    }

    fn lane(seq: u64, lane: u32, number: u64, stage: &str, step: Option<&str>) -> Event {
        event(
            seq,
            EV_POOL_LANE,
            record_lane_data("acme/app", lane, number, stage, step),
        )
    }

    #[test]
    fn the_status_is_the_replayed_lanes() {
        let events = vec![
            event(1, EV_POOL_LANES, json!({"repo": "acme/app", "lanes": 2})),
            event(2, EV_POOL_STARTED, json!({"repo": "acme/app", "lanes": 2})),
            lane(3, 1, 5, "starting", None),
            lane(4, 2, 6, "starting", None),
            lane(5, 1, 5, "build", Some("build")),
            event(
                6,
                EV_ROUND,
                json!({"repo": "acme/app", "number": 5, "round": 1,
                       "steps": [{"step": "build", "cost_usd": 0.25}, {"step": "review", "cost_usd": 0.5}]}),
            ),
            lane(7, 1, 5, "review", Some("review")),
            lane(8, 2, 6, "done", None),
            event(
                9,
                EV_LANDED,
                json!({"repo": "acme/app", "number": 6, "pr": "p/6"}),
            ),
            event(10, EV_POOL_LANES, json!({"repo": "acme/app", "lanes": 3})),
            // Another repository's pool is not this one's.
            event(11, EV_POOL_LANES, json!({"repo": "acme/other", "lanes": 9})),
        ];
        let items = [item(7, &[]), item(8, &[5])];
        let status = from_events(&events, "acme/app", true, &items);
        assert_eq!(status.state, "running");
        assert_eq!(status.target, Some(3));
        assert_eq!(status.busy, 1);
        assert_eq!(status.admitted, vec![5]);
        let one = &status.lanes[0];
        assert_eq!((one.lane, one.number), (1, 5));
        assert_eq!(
            (one.stage.as_str(), one.step.as_deref()),
            ("review", Some("review"))
        );
        assert_eq!(one.since_ms, 7000);
        assert!((one.spent_usd - 0.75).abs() < 1e-9);
        assert_eq!(status.waiting[0].why, "no free lane");
        assert_eq!(status.waiting[1].why, "waits on #5");
        assert_eq!(status.landed[0].number, 6);
        let text = describe(&status, 67_000);
        assert!(text.contains("running, 1 busy of 3 lanes"), "{text}");
        assert!(text.contains("1m00s"), "{text}");
    }

    #[test]
    fn a_pool_that_stopped_or_died_has_no_busy_lanes() {
        let events = vec![
            event(1, EV_POOL_STARTED, json!({"repo": "acme/app", "lanes": 2})),
            lane(2, 1, 5, "build", Some("build")),
            event(3, EV_POOL_STOPPED, json!({"repo": "acme/app"})),
        ];
        let stopped = from_events(&events, "acme/app", true, &[]);
        assert_eq!((stopped.state.as_str(), stopped.busy), ("stopped", 0));
        // No `pool.stopped`, but no live pool holds the lock: it was killed.
        let died = from_events(&events[..2], "acme/app", false, &[item(9, &[])]);
        assert_eq!((died.state.as_str(), died.busy), ("stopped", 0));
        assert_eq!(died.waiting[0].why, "no pool is running");
    }

    #[test]
    fn each_step_belongs_to_a_stage() {
        for (role, stage) in [
            ("build", "build"),
            ("fix", "build"),
            ("checks", "checks"),
            ("review", "review"),
            ("review-high-risk", "review"),
            ("ci", "land"),
            ("land", "land"),
        ] {
            assert_eq!(stage_of(role), stage, "{role}");
        }
    }
}
