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

use super::land::{turn_kind, EV_LANDING, EV_MERGING};
use super::plan::{blocked_by, overlap, Plan, EV_POOL_PLANNED};
use super::EV_CHANGED;
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
/// The pool started planning an item: `{repo, number}`. Its plan follows as `pool.planned`.
pub const EV_POOL_PLANNING: &str = "pool.planning";
/// An item was admitted to a lane for the files its plan names:
/// `{repo, number, lane, paths, source, high_risk}`.
pub const EV_POOL_ADMITTED: &str = "pool.admitted";
/// A lane's change touched files outside the ones it was admitted for:
/// `{repo, number, lane, outside, admitted}`.
pub const EV_POOL_DRIFT: &str = "pool.drift";

/// The most lanes one pool runs.
pub const MAX_LANES: u32 = 32;

/// How often the pool looks at its lanes and the lane count.
const TICK: Duration = Duration::from_millis(500);

/// How long [`Pool::stop_all`] waits for its lanes after `stop now` before it ends the ones
/// still going: longer than a lane's own run takes to stop at once, so only a stuck one is
/// ended.
const STOP_GRACE: Duration = Duration::from_secs(30);

/// How long a planner told to stop now has to end its agent and exit before it is killed.
const PLANNER_GRACE: Duration = Duration::from_secs(5);

/// How long a `pool plan` may take before the pool ends it and counts the plan as failed, so a
/// hung planner can't hold up planning and admission.
const PLAN_LIMIT: Duration = Duration::from_secs(10 * 60);

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
    /// The plan the lane was admitted for.
    plan: Plan,
    /// The `pool.lane` event that started it.
    started_seq: u64,
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
    // The key the tamper lock uses for this checkout, read once: the pool looks for a tamper
    // stop on every tick without running git.
    let key = crate::ahjo::repo_key(&repo)?;
    // Under the home lock, as `paja pool status` probes it, so a probe never makes a starting
    // pool think another one runs.
    let pool_lock = {
        let _home = home.lock()?;
        home.try_pool_lock(&name)?
    };
    let Some(_pool) = pool_lock else {
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
    let since = {
        let lock = home.lock()?;
        let data = json!({ "repo": name, "lanes": opts.lanes, "pid": std::process::id() });
        home.events().append(&lock, EV_POOL_STARTED, data)?.seq
    };
    progress(&format!(
        "pool of {name}: up to {} lanes (`paja pool lanes <N>` changes it; Ctrl+C stops)",
        opts.lanes
    ));
    let mut pool = Pool {
        home,
        repo: &repo,
        name: &name,
        key: &key,
        opts: &opts,
        lanes: BTreeMap::new(),
        planners: BTreeMap::new(),
        elsewhere: BTreeMap::new(),
        stopped: Vec::new(),
        since,
        held_back: BTreeSet::new(),
        retry_failed_to: 0,
        retried: BTreeSet::new(),
    };
    let outcome = pool.run();
    // A pool that ends on an error leaves no lane running unsupervised.
    if outcome.is_err() {
        pool.stop_all();
    }
    pool.end_planners();
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
    /// The checkout's key in the tamper lock.
    key: &'a str,
    opts: &'a PoolOptions<'a>,
    lanes: BTreeMap<u32, Lane>,
    /// The `pool plan` runs going, by item, with the seq they started after and when.
    planners: BTreeMap<u64, (Child, u64, Instant)>,
    /// Items another `paja pool plan` (one started by hand) was planning at the last tick, with
    /// when the pool first saw it: the pool waits for that plan instead of starting one, counts
    /// the item as being planned, isn't idle meanwhile, and stops waiting after `PLAN_LIMIT`.
    elsewhere: BTreeMap<u64, Instant>,
    /// Planners told `stop now`, with when, until they end or are killed.
    stopped: Vec<(Child, Instant)>,
    /// The seq of this pool's `pool.started`: plans before it are an earlier pool's.
    since: u64,
    /// Items whose lane ended on an error: not handed out again until the next timed sync, as
    /// `paja run --every` waits after a failed pass.
    held_back: BTreeSet<u64>,
    /// Failed plans recorded at or before this seq are planned again: set at each timed sync,
    /// so a planner's passing error doesn't keep its item waiting for an idle pool.
    retry_failed_to: u64,
    /// The items planned again that way: once each per pool run, so a plan that keeps failing
    /// doesn't start a planner every sync.
    retried: BTreeSet<u64>,
}

impl Pool<'_> {
    fn run(&mut self) -> Result<()> {
        let every = Duration::from_secs(self.opts.every.saturating_mul(60));
        // The first sync was the one that named the repository.
        let mut last_sync = Instant::now();
        let mut need_sync = false;
        let mut said_idle = false;
        let mut said_paused: Option<u64> = None;
        let mut forwarded = 0;
        let mut fatal: Option<anyhow::Error> = None;
        let mut told_fatal = false;
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
            let events = self.home.events().read_all()?;
            // A lane's run that found the repository's git configuration changed recorded a
            // tamper stop: no lane may run git or gh there again, so all of them stop now.
            if fatal.is_none() && tampered(&events, self.key) {
                let err = crate::ahjo::ensure_untampered(self.home, self.repo)
                    .err()
                    .unwrap_or_else(|| anyhow::anyhow!("a lane's run stopped on a tamper"));
                fatal = Some(err);
            }
            // However the pool learnt of it (this check, or its own sync refusing), every lane
            // is told once.
            if fatal.is_some() && !told_fatal {
                for lane in self.lanes.values_mut() {
                    lane.tell("stop now");
                }
                told_fatal = true;
            }
            let stopping = level > 0 || fatal.is_some();
            if stopping {
                // A plan is only worth having for a lane that may start. Stopped before they
                // are reaped, so one that the same Ctrl-C ended isn't counted as failed.
                self.stop_planners();
            }
            self.reap_planners()?;
            reap_stopped(&mut self.stopped, PLANNER_GRACE);
            if stopping && self.lanes.is_empty() {
                return fatal.map_or(Ok(()), Err);
            }
            self.follow_stages(&events)?;
            if !stopping {
                let target = target_lanes(&events, self.name).unwrap_or(self.opts.lanes);
                let free = (target as usize).saturating_sub(self.lanes.len());
                // The usage window is the account's: until it resets, no lane or planner starts.
                let paused = super::usage_pause(&events, crate::events::now_ms());
                if let (Some(ms), true) = (paused, said_paused != paused) {
                    progress(&format!(
                        "Claude Code's usage limit hasn't reset; starting nothing until {}",
                        super::format_utc(ms)
                    ));
                }
                said_paused = paused;
                if free > 0 && paused.is_none() {
                    let timed = last_sync.elapsed() >= every;
                    if need_sync || timed {
                        need_sync = false;
                        last_sync = Instant::now();
                        said_idle = false;
                        if timed {
                            self.held_back.clear();
                            self.retry_failed_to = events.last().map_or(0, |e| e.seq);
                        }
                        match super::sync_queue(self.home, self.repo) {
                            Ok(_) => {}
                            // The next tick tells the lanes and stops, after the usual sleep.
                            Err(err) if super::is_fatal(&err) => fatal = Some(err),
                            Err(err) => self.say_error(&err),
                        }
                    }
                }
                if free > 0 && paused.is_none() && fatal.is_none() {
                    let admitted = self.admit(free)?;
                    if admitted == 0
                        && self.lanes.is_empty()
                        && self.planners.is_empty()
                        && self.elsewhere.is_empty()
                        && !said_idle
                    {
                        said_idle = true;
                        self.idle()?;
                    }
                }
            }
            std::thread::sleep(TICK);
        }
    }

    /// Hand up to `free` lanes the next items a run could take whose files don't overlap a
    /// running item's, planning each first. Returns how many started.
    fn admit(&mut self, free: usize) -> Result<usize> {
        let busy: BTreeSet<u64> = self.lanes.values().map(|lane| lane.number).collect();
        let next: Vec<WorkItem> = match jono::runnable(self.home, self.name) {
            Ok(items) => items,
            Err(err) => {
                self.say_error(&err);
                return Ok(0);
            }
        };
        let (home, name) = (self.home, self.name);
        // Probed before the log is read, as `reap` does. A probe that can't read the lock
        // counts as free: the pool's own planner then says what is wrong.
        let queued: BTreeSet<u64> = next.iter().map(|item| item.number).collect();
        keep_waiting(&mut self.elsewhere, &queued, |n| {
            home.plan_held(name, n).unwrap_or(false)
        });
        let events = self.home.events().read_all()?;
        let mut plans = plans_since(&events, self.name, self.since);
        for number in give_up(&mut self.elsewhere, &plans, PLAN_LIMIT) {
            let why = format!(
                "the other `paja pool plan {number}` took longer than {} minutes",
                PLAN_LIMIT.as_secs() / 60
            );
            // Seen by this tick's choice too, so the item isn't planned again at once.
            let failed = self.plan_failed(number, &why)?;
            plans.insert(number, failed);
        }
        let retry: BTreeSet<u64> = failed_by(&events, self.name, self.since, self.retry_failed_to)
            .into_iter()
            .filter(|n| !self.retried.contains(n))
            .collect();
        for number in &retry {
            plans.remove(number);
        }
        let queue: Vec<u64> = next
            .iter()
            .map(|item| item.number)
            .filter(|n| !busy.contains(n) && !self.held_back.contains(n))
            .collect();
        let running: Vec<&Plan> = self.lanes.values().map(|lane| &lane.plan).collect();
        let planning: BTreeSet<u64> = self
            .planners
            .keys()
            .chain(self.elsewhere.keys())
            .copied()
            .collect();
        // At most as many plans at once as there are free lanes to use them.
        let room = free.saturating_sub(self.planners.len() + self.elsewhere.len());
        let (chosen, to_plan) = choose(&queue, &plans, &running, &planning, free, room);
        let chosen: Vec<Plan> = chosen.into_iter().cloned().collect();
        for number in to_plan {
            if retry.contains(&number) {
                self.retried.insert(number);
            }
            self.start_planner(number)?;
        }
        let mut started = 0;
        for plan in chosen {
            let Some(item) = next.iter().find(|item| item.number == plan.number) else {
                continue;
            };
            let lane = (1..).find(|n| !self.lanes.contains_key(n)).unwrap_or(1);
            let start = record_lane(self.home, self.name, lane, item.number, "starting", None)?;
            record(
                self.home,
                EV_POOL_ADMITTED,
                json!({
                    "repo": self.name, "number": item.number, "lane": lane, "paths": plan.paths,
                    "source": plan.source, "high_risk": plan.high_risk,
                }),
            )?;
            match spawn(self.repo, lane, item.number) {
                Ok((child, stdin, report)) => {
                    progress(&format!("lane {lane}: #{} {}", item.number, item.title));
                    self.lanes.insert(
                        lane,
                        Lane {
                            number: item.number,
                            plan: plan.clone(),
                            started_seq: start.seq,
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

    /// Plan item `number` in a `paja pool plan` of its own, so the pool's ticks go on meanwhile.
    /// A plan of the item already running (one started by hand) is left to finish: the pool
    /// takes the plan it records, and starts none of its own meanwhile.
    fn start_planner(&mut self, number: u64) -> Result<()> {
        if self.home.plan_held(self.name, number).unwrap_or(false) {
            if let std::collections::btree_map::Entry::Vacant(seen) = self.elsewhere.entry(number) {
                seen.insert(Instant::now());
                // Recorded, so `paja pool status` says the item is being planned.
                record(
                    self.home,
                    EV_POOL_PLANNING,
                    json!({ "repo": self.name, "number": number }),
                )?;
                progress(&format!(
                    "#{number} is being planned by another `paja pool plan`; using its plan"
                ));
            }
            return Ok(());
        }
        let at = {
            let lock = self.home.lock()?;
            let data = json!({ "repo": self.name, "number": number });
            self.home
                .events()
                .append(&lock, EV_POOL_PLANNING, data)?
                .seq
        };
        progress(&format!("planning which files #{number} touches"));
        match spawn_planner(self.repo, number) {
            Ok(child) => {
                self.planners.insert(number, (child, at, Instant::now()));
            }
            Err(err) => {
                self.plan_failed(number, &format!("{err:#}"))?;
            }
        }
        Ok(())
    }

    /// Free the plans that ended; one that ended without recording a plan failed.
    fn reap_planners(&mut self) -> Result<()> {
        for (number, why) in reap(
            self.home,
            self.name,
            &mut self.planners,
            &mut self.elsewhere,
            PLAN_LIMIT,
            PLANNER_GRACE,
        )? {
            self.plan_failed(number, &why)?;
        }
        Ok(())
    }

    /// Record a failed plan, which the pool admits only to an otherwise idle pool.
    fn plan_failed(&mut self, number: u64, error: &str) -> Result<Plan> {
        progress(&format!(
            "could not plan #{number} ({error}); it runs only when no other lane is busy"
        ));
        let failed = Plan {
            repo: self.name.to_string(),
            number,
            paths: Vec::new(),
            source: "failed".to_string(),
            high_risk: false,
            error: Some(error.to_string()),
        };
        record(self.home, EV_POOL_PLANNED, serde_json::to_value(&failed)?)?;
        Ok(failed)
    }

    /// End the plans going: they are read-only, and an item left unplanned is planned again. Each
    /// is told `stop now`, which ends its agent and records no plan, and is killed if it still
    /// runs [`PLANNER_GRACE`] later: on a later tick ([`reap_stopped`]), or when the pool
    /// ends ([`Pool::end_planners`]), so the pool never waits on one while lanes need it.
    fn stop_planners(&mut self) {
        for (_, (mut child, _, _)) in std::mem::take(&mut self.planners) {
            tell_planner(&mut child);
            self.stopped.push((child, Instant::now()));
        }
    }

    /// Stop every planner and wait for each to end, up to [`PLANNER_GRACE`]: the pool is ending,
    /// and leaves none running unsupervised.
    fn end_planners(&mut self) {
        self.stop_planners();
        let deadline = Instant::now() + PLANNER_GRACE;
        while !self.stopped.is_empty() && Instant::now() < deadline {
            reap_stopped(&mut self.stopped, PLANNER_GRACE);
            std::thread::sleep(Duration::from_millis(100));
        }
        reap_stopped(&mut self.stopped, Duration::ZERO);
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
        if let Some(drift) = drift(&events, self.name, n, &lane) {
            record(self.home, EV_POOL_DRIFT, drift)?;
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
            crate::platform::print_line(format_args!("{report}"));
        } else {
            crate::platform::print_line(describe_report(n, lane.number, &report));
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
                    crate::platform::print_line(json!({ "outcome": "triage", "triage": report }));
                } else {
                    crate::platform::print_line(format_args!(
                        "triage: {}",
                        serde_json::to_string(&report)?
                    ));
                }
            }
            Ok(None) => {}
            Err(err) if super::is_fatal(&err) => return Err(err),
            // Tagged, so a reader of the pool's lines can tell it from a lane's error.
            Err(err) if self.opts.json => {
                crate::platform::print_line(
                    json!({ "outcome": "triage", "error": format!("{err:#}") }),
                );
            }
            Err(err) => self.say_error(&err),
        }
        let minutes = self.opts.every;
        progress(&format!(
            "nothing to do right now; checking again in {minutes} minute{} (Ctrl+C stops)",
            if minutes == 1 { "" } else { "s" }
        ));
        Ok(())
    }

    /// Stop every lane's run at once and wait for it to end.
    fn stop_all(&mut self) {
        for lane in self.lanes.values_mut() {
            lane.tell("stop now");
        }
        self.stop_planners();
        stop_children(
            self.lanes.values_mut().map(|lane| &mut lane.child),
            STOP_GRACE,
        );
        self.lanes.clear();
    }

    fn say_error(&self, err: &anyhow::Error) {
        if self.opts.json {
            crate::platform::print_line(json!({ "error": format!("{err:#}") }));
        } else {
            crate::platform::print_line(format_args!("error: {err:#}"));
        }
    }
}

/// Wait for `children`, told to stop, to end, up to `grace` in all, then end the ones still
/// going, so a run that never ends can't hang the pool.
fn stop_children<'c>(children: impl Iterator<Item = &'c mut Child>, grace: Duration) {
    let mut children: Vec<&mut Child> = children.collect();
    let deadline = Instant::now() + grace;
    loop {
        children.retain_mut(|child| matches!(child.try_wait(), Ok(None)));
        if children.is_empty() || Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    for child in children {
        let _ = child.kill();
        let _ = child.wait();
    }
}

/// Forget the stopped planners in `stopped` that ended, and kill ([`crate::platform::kill_tree`])
/// the ones still going `grace` after they were told to stop. A planner ends its own agent on
/// `stop now`; this is for one wedged before it could.
fn reap_stopped(stopped: &mut Vec<(Child, Instant)>, grace: Duration) {
    stopped.retain_mut(|(child, told)| {
        if !matches!(child.try_wait(), Ok(None)) {
            return false;
        }
        if told.elapsed() < grace {
            return true;
        }
        crate::platform::kill_tree(child);
        let _ = child.wait();
        false
    });
}

/// Keep waiting in `elsewhere` only on items still queued whose other plan still holds its lock.
fn keep_waiting(
    elsewhere: &mut BTreeMap<u64, Instant>,
    queued: &BTreeSet<u64>,
    held: impl Fn(u64) -> bool,
) {
    elsewhere.retain(|&n, _| queued.contains(&n) && held(n));
}

/// Take out of `elsewhere` the items waited on for longer than `limit`, and return the ones
/// whose plan failed for it: not one the other plan recorded just now, before letting go.
fn give_up(
    elsewhere: &mut BTreeMap<u64, Instant>,
    plans: &BTreeMap<u64, Plan>,
    limit: Duration,
) -> Vec<u64> {
    let gone: Vec<u64> = elsewhere
        .iter()
        .filter(|(_, seen)| seen.elapsed() > limit)
        .map(|(&n, _)| n)
        .collect();
    for number in &gone {
        elsewhere.remove(number);
    }
    gone.into_iter()
        .filter(|n| !plans.get(n).is_some_and(|plan| plan.source != "failed"))
        .collect()
}

/// Take out of `planners` the plans that ended, ending first any going longer than `limit`, and
/// return each that recorded no plan with why. A plan killed for its time may still have
/// recorded one in the instant before, which counts like any other.
fn reap(
    home: &Home,
    repo: &str,
    planners: &mut BTreeMap<u64, (Child, u64, Instant)>,
    elsewhere: &mut BTreeMap<u64, Instant>,
    limit: Duration,
    grace: Duration,
) -> Result<Vec<(u64, String)>> {
    let overdue = end_overdue(planners, limit, grace);
    let ended: Vec<(u64, u64)> = planners
        .iter_mut()
        .filter_map(|(&n, (child, at, _))| {
            (!matches!(child.try_wait(), Ok(None))).then_some((n, *at))
        })
        .collect();
    if ended.is_empty() {
        return Ok(Vec::new());
    }
    // Probed before the log is read: a plan by another `paja pool plan` is recorded before
    // its lock is let go, so one that let go meanwhile is in the log read after.
    let held: BTreeSet<u64> = ended
        .iter()
        .filter(|(n, _)| home.plan_held(repo, *n).unwrap_or(false))
        .map(|(n, _)| *n)
        .collect();
    // The plan may have been recorded after this tick read the log.
    let events = home.events().read_all()?;
    let mut failed = Vec::new();
    for (number, at) in ended {
        let kept = planners.remove(&number);
        let planned = events.iter().any(|e| {
            e.seq > at
                && e.kind == EV_POOL_PLANNED
                && e.data["repo"] == repo
                && e.data["number"] == number
        });
        if planned {
            continue;
        }
        drop(kept);
        // Refused because another plan of the item is running (one started by hand): that one
        // records the plan, so the pool waits for it as it does when it finds the lock held
        // before starting a planner, under the same rules and time limit.
        if !overdue.contains(&number) && held.contains(&number) {
            elsewhere.entry(number).or_insert_with(Instant::now);
            continue;
        }
        let why = if overdue.contains(&number) {
            format!(
                "the plan of #{number} took longer than {} minutes",
                limit.as_secs() / 60
            )
        } else {
            format!("the plan of #{number} ended without one")
        };
        failed.push((number, why));
    }
    Ok(failed)
}

/// End the planners in `planners` that have run longer than `limit`, and return their items,
/// ended or not. One still going is told `stop now` on each tick, and killed once it has run
/// `grace` past the limit, so the pool never waits on it. They stay in the map, for [`reap`] to
/// take out once they have ended.
fn end_overdue(
    planners: &mut BTreeMap<u64, (Child, u64, Instant)>,
    limit: Duration,
    grace: Duration,
) -> Vec<u64> {
    let mut overdue = Vec::new();
    for (&number, (child, _, started)) in planners.iter_mut() {
        let age = started.elapsed();
        if age <= limit {
            continue;
        }
        overdue.push(number);
        if matches!(child.try_wait(), Ok(None)) {
            if age > limit + grace {
                crate::platform::kill_tree(child);
                let _ = child.wait();
            } else {
                tell_planner(child);
            }
        }
    }
    overdue
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

/// The items to admit this tick and the ones to start planning, from the runnable `queue` in
/// order. An item is admitted only once every item before it has a plan, so a plan that
/// finishes first never puts a later item ahead of an earlier one; an item whose plan overlaps
/// a `running` plan, or one admitted before it in the same tick, is skipped and the next that
/// doesn't overlap is admitted instead. At most `free` are admitted and `room` plans started.
fn choose<'p>(
    queue: &[u64],
    plans: &'p BTreeMap<u64, Plan>,
    running: &[&'p Plan],
    planning: &BTreeSet<u64>,
    free: usize,
    room: usize,
) -> (Vec<&'p Plan>, Vec<u64>) {
    let mut chosen: Vec<&Plan> = Vec::new();
    let mut to_plan = Vec::new();
    let mut unplanned_before = false;
    for number in queue {
        // An item being planned again has only its old, failed plan: it waits for the new one.
        let plan = plans.get(number).filter(|_| !planning.contains(number));
        match plan {
            None => {
                unplanned_before = true;
                if !planning.contains(number) && to_plan.len() < room {
                    to_plan.push(*number);
                }
            }
            Some(_) if unplanned_before || chosen.len() >= free => {}
            Some(plan) => {
                let mut busy: Vec<&Plan> = running.to_vec();
                busy.extend(chosen.iter().copied());
                if blocked_by(plan, &busy).is_none() {
                    chosen.push(plan);
                }
            }
        }
    }
    (chosen, to_plan)
}

/// The latest plan of each item of `repo` recorded after `since`.
fn plans_since(events: &[Event], repo: &str, since: u64) -> BTreeMap<u64, Plan> {
    events
        .iter()
        .filter(|e| e.seq > since && e.kind == EV_POOL_PLANNED && e.data["repo"] == repo)
        .filter_map(|e| serde_json::from_value::<Plan>(e.data.clone()).ok())
        .map(|plan| (plan.number, plan))
        .collect()
}

/// The items of `repo` whose latest plan after `since` failed and was recorded at or before
/// `to`: the ones to plan again.
fn failed_by(events: &[Event], repo: &str, since: u64, to: u64) -> BTreeSet<u64> {
    let mut latest: BTreeMap<u64, (u64, bool)> = BTreeMap::new();
    for e in events {
        if e.seq > since && e.kind == EV_POOL_PLANNED && e.data["repo"] == repo {
            if let Some(number) = e.data["number"].as_u64() {
                latest.insert(number, (e.seq, e.data["source"] == "failed"));
            }
        }
    }
    latest
        .into_iter()
        .filter(|(_, (seq, failed))| *failed && *seq <= to)
        .map(|(number, _)| number)
        .collect()
}

/// The files lane `n`'s change touched, by its latest `run.changed`, that its plan didn't name:
/// the `pool.drift` to record, if any. A failed plan named nothing to drift from.
fn drift(events: &[Event], repo: &str, n: u32, lane: &Lane) -> Option<Value> {
    if lane.plan.source == "failed" {
        return None;
    }
    let changed = events.iter().rev().find(|e| {
        e.seq > lane.started_seq
            && e.kind == EV_CHANGED
            && e.data["repo"] == repo
            && e.data["number"] == lane.number
    })?;
    let outside: Vec<&str> = changed.data["paths"]
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .filter(|path| !lane.plan.paths.iter().any(|p| overlap(p, path)))
        .collect();
    (!outside.is_empty()).then(|| {
        json!({
            "repo": repo, "number": lane.number, "lane": n, "outside": outside,
            "admitted": lane.plan.paths,
        })
    })
}

/// Tell a planner to stop now: it ends its planning agent, with every process the agent
/// started, and exits.
fn tell_planner(child: &mut Child) {
    if let Some(stdin) = child.stdin.as_mut() {
        let _ = writeln!(stdin, "stop now").and_then(|()| stdin.flush());
    }
}

/// `paja pool plan <number>` as a child of the pool, reading `stop now` on stdin; its progress
/// lines go to the pool's stderr.
fn spawn_planner(repo: &Path, number: u64) -> Result<Child> {
    let exe = std::env::current_exe().context("finding the paja program to plan with")?;
    let mut cmd = Command::new(exe);
    // Only the pool stops a planner: a terminal's Ctrl-C would end it at once and leave its
    // agent, which runs in a process group of its own, going. A planner whose pool is gone
    // stops anyway, when its stdin closes.
    crate::platform::own_process_group(&mut cmd);
    let mut child = cmd
        .args(["--json", "pool", "plan", "--stop-on-stdin"])
        .arg(number.to_string())
        .arg("--repo")
        .arg(repo)
        .current_dir(repo)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting `paja pool plan {number}`"))?;
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                // Not `eprintln!`: on a closed stderr it panics, this thread stops reading,
                // and the child blocks once its stderr pipe is full.
                let _ = writeln!(std::io::stderr(), "plan #{number}: {line}");
            }
        });
    }
    Ok(child)
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
                // Not `eprintln!`: on a closed stderr it panics, this thread stops reading,
                // and the child blocks once its stderr pipe is full.
                let _ = writeln!(std::io::stderr(), "lane {lane} #{number}: {line}");
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

/// Whether the latest tamper event for the checkout `key` is a stop, not a person's check.
fn tampered(events: &[Event], key: &str) -> bool {
    events
        .iter()
        .rev()
        .find(|e| {
            (e.kind == crate::ahjo::EV_TAMPERED || e.kind == crate::ahjo::EV_TAMPER_CHECKED)
                && e.data["repo"] == key
        })
        .is_some_and(|e| e.kind == crate::ahjo::EV_TAMPERED)
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
/// lock, under which a pool takes it too.
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
    /// The lane count the pool keeps: the latest set with `paja pool lanes`, or 1 for a
    /// repository that never recorded one.
    pub target: u32,
    /// How many lanes are working on an item.
    pub busy: usize,
    pub lanes: Vec<LaneStatus>,
    /// The items on a lane.
    pub admitted: Vec<u64>,
    /// The queued items not on a lane, with why.
    pub waiting: Vec<WaitingItem>,
    /// The latest landings of the repository, newest first.
    pub landed: Vec<Landing>,
    /// When the running pool started, in milliseconds since the Unix epoch; None while stopped.
    pub started_ms: Option<u64>,
    /// What the repository's finished rounds spent since the running pool started, in US
    /// dollars (Claude Code's estimate).
    pub spent_usd: f64,
    /// What those rounds used in tokens: input, output and cache, as `paja usage` counts them.
    pub tokens: u64,
    /// The latest items that left a lane, newest first.
    pub out: Vec<LaneExit>,
    /// How many items landed since the running pool started.
    pub landed_since_start: usize,
    /// How many items left a lane since the running pool started without landing and without
    /// being held: failed, or closed.
    pub retired_since_start: usize,
    /// How many open items are held, escalated and waiting on the person's answer.
    pub held: usize,
    /// How many waiting items are ready and wait only for a free lane.
    pub ready: usize,
    /// The item whose lane holds the repository's turn to land, if one does.
    pub landing: Option<u64>,
    /// The merging queue: items whose lanes wait for that turn, longest waiting first.
    pub merging: Vec<u64>,
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
    /// What those rounds used in tokens (input, output and cache).
    pub tokens: u64,
    /// The files the lane was admitted for, from its `pool.admitted`.
    pub paths: Vec<String>,
    /// The item's main folder: the longest directory its paths share, else the first path's top
    /// folder; empty without paths.
    pub area: String,
    /// The review round the item is in, 1 up to the most rounds a change gets.
    pub round: u32,
    /// When the lane took the item, in milliseconds since the Unix epoch.
    pub admitted_ms: u64,
    /// For an item in `review`: one entry per reviewer of the round, `pending`, `pass` or
    /// `block`.
    pub reviews: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct WaitingItem {
    pub number: u64,
    pub title: String,
    pub why: String,
    /// The files its plan names, once it has one.
    pub paths: Vec<String>,
    /// The item's main folder, as [`LaneStatus::area`].
    pub area: String,
}

/// An item that left a lane.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LaneExit {
    pub number: u64,
    pub title: String,
    /// `landed`, `held` (escalated), `failed`, or `retired` (closed without landing).
    pub outcome: String,
    pub ts_ms: u64,
}

/// The longest directory all of `paths` share; when they share none, the first path's top
/// folder; empty when there are no paths or the first is a file at the root.
fn area(paths: &[String]) -> String {
    let dirs: Vec<Vec<String>> = paths
        .iter()
        .map(|p| {
            let p = p.replace('\\', "/");
            let mut parts: Vec<String> = p
                .split('/')
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect();
            parts.pop();
            parts
        })
        .collect();
    let Some(first) = dirs.first() else {
        return String::new();
    };
    let common = first
        .iter()
        .enumerate()
        .take_while(|(i, part)| dirs.iter().all(|d| d.get(*i) == Some(*part)))
        .count();
    if common > 0 {
        first[..common].join("/")
    } else {
        first.first().cloned().unwrap_or_default()
    }
}

/// The tokens a round event's steps used; a step whose figures are partly missing adds nothing,
/// as in `paja usage`.
fn round_tokens(event: &Event) -> u64 {
    event.data["steps"]
        .as_array()
        .map(|steps| {
            steps
                .iter()
                .filter_map(|step| {
                    [
                        "input_tokens",
                        "output_tokens",
                        "cache_read_input_tokens",
                        "cache_creation_input_tokens",
                    ]
                    .iter()
                    .map(|key| step[key].as_u64())
                    .sum::<Option<u64>>()
                })
                .sum()
        })
        .unwrap_or(0)
}

/// The review round an item's events say it is in, capped at the rounds a change gets.
fn lane_round(mine: &[&Event]) -> u32 {
    let mut round = 1;
    for e in mine {
        let n = e.data["round"].as_u64().unwrap_or(0) as u32;
        match e.kind.as_str() {
            EV_STEP => round = round.max(n),
            // A round that goes on leaves the next one to start.
            EV_ROUND if e.data.get("next").is_some_and(|next| !next.is_null()) => {
                round = round.max(n + 1)
            }
            _ => {}
        }
    }
    round.min(super::MAX_ROUNDS)
}

/// Where each reviewer of the round in progress stands, from the item's events in order.
fn lane_reviews(mine: &[&Event], high_risk: bool) -> Vec<String> {
    let mut reviews: Vec<String> = Vec::new();
    for e in mine {
        match e.kind.as_str() {
            EV_STEP => match e.data["reviewer"].as_u64() {
                Some(reviewer) => {
                    let count = e.data["reviewers"]
                        .as_u64()
                        .unwrap_or(if high_risk { 2 } else { 1 })
                        .max(reviewer) as usize;
                    if reviews.len() < count {
                        reviews.resize(count, "pending".to_string());
                    }
                    reviews[reviewer as usize - 1] = "pending".to_string();
                }
                None => reviews.clear(),
            },
            super::EV_REVIEWED => {
                let reviewer = e.data["reviewer"].as_u64().unwrap_or(0) as usize;
                if (1..=reviews.len()).contains(&reviewer) {
                    reviews[reviewer - 1] = match e.data["verdict"].as_str() {
                        Some("land") => "pass",
                        _ => "block",
                    }
                    .to_string();
                }
            }
            EV_ROUND => reviews.clear(),
            _ => {}
        }
    }
    if reviews.is_empty() {
        reviews = vec!["pending".to_string(); if high_risk { 2 } else { 1 }];
    }
    reviews
}

/// Why a queued item isn't in a lane, which [`WaitingItem::why`] says in words.
enum Why {
    /// It waits on these other items.
    WaitsOn(Vec<u64>),
    NoPool,
    /// Another runner holds it on GitHub, under this runner id.
    HeldBy(String),
    /// Its files overlap this lane's item's, for this reason.
    Overlaps(u64, String),
    Planning,
    /// Its last lane ended on an error; it waits for the next timed sync.
    LaneErrored,
    /// Nothing but a busy pool: it is ready.
    NoFreeLane,
}

impl std::fmt::Display for Why {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Why::WaitsOn(on) => {
                let on: Vec<String> = on.iter().map(|n| format!("#{n}")).collect();
                write!(f, "waits on {}", on.join(", "))
            }
            Why::HeldBy(runner) => write!(f, "claimed by runner {runner} on GitHub"),
            Why::NoPool => f.write_str("no pool is running"),
            Why::Overlaps(other, why) => write!(f, "overlaps #{other}: {why}"),
            Why::Planning => f.write_str("planning which files it touches"),
            Why::LaneErrored => f.write_str(
                "its last run ended on an error; it is held back until a timed sync (see --every), \
                 and may have waited for a free lane since",
            ),
            Why::NoFreeLane => f.write_str("no free lane"),
        }
    }
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

/// Which of `lanes` holds the repository's turn to land, and which wait for it, longest first.
/// A lane's latest `run.merging`, `run.landing`, `run.round` or `run.landed` since its item
/// started says: a round or a landing after the turn ends it.
fn land_turns(
    events: &[Event],
    repo: &str,
    lanes: &[LaneStatus],
    begun: &[u64],
) -> (Option<u64>, Vec<u64>) {
    let mut landing = None;
    let mut merging: Vec<(u64, u64)> = Vec::new();
    for (lane, begun) in lanes.iter().zip(begun) {
        let latest = events
            .iter()
            .rev()
            .take_while(|e| e.seq > *begun)
            .find(|e| {
                e.data["repo"] == repo
                    && e.data["number"] == lane.number
                    && [EV_MERGING, EV_LANDING, EV_ROUND, EV_LANDED].contains(&turn_kind(e))
            });
        match latest {
            Some(e) if turn_kind(e) == EV_LANDING => landing = Some(lane.number),
            Some(e) if turn_kind(e) == EV_MERGING => merging.push((e.seq, lane.number)),
            _ => {}
        }
    }
    merging.sort();
    (landing, merging.into_iter().map(|(_, n)| n).collect())
}

/// The items that left a lane of `repo`, newest first, each with the seq of the event that
/// freed the lane. A lane's `done` says how its run ended: `landed`, `escalated` (held), or
/// `error` (failed); any other end of a run that left the item closed is `retired`. An item
/// that went back to the queue (stopped, usage limit) did not leave.
fn lane_exits(events: &[Event], repo: &str, title: &dyn Fn(u64) -> String) -> Vec<(u64, LaneExit)> {
    let mut closed: BTreeSet<u64> = BTreeSet::new();
    let mut exits = Vec::new();
    for e in events.iter().filter(|e| e.data["repo"] == repo) {
        let Some(number) = e.data["number"].as_u64() else {
            continue;
        };
        match e.kind.as_str() {
            jono::EV_CLOSED => {
                closed.insert(number);
            }
            jono::EV_REOPENED => {
                closed.remove(&number);
            }
            EV_POOL_LANE if e.data["stage"] == "done" => {
                let outcome = match e.data["outcome"].as_str() {
                    Some("landed") => "landed",
                    Some("escalated") => "held",
                    Some("error") => "failed",
                    _ if closed.contains(&number) => "retired",
                    _ => continue,
                };
                exits.push((
                    e.seq,
                    LaneExit {
                        number,
                        title: title(number),
                        outcome: outcome.to_string(),
                        ts_ms: e.ts_ms,
                    },
                ));
            }
            _ => {}
        }
    }
    exits.reverse();
    exits
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
    let (lanes, begun): (Vec<(LaneStatus, Option<Plan>)>, Vec<u64>) = lanes
        .into_iter()
        .filter(|(_, (event, _))| event.data["stage"] != "done")
        .filter_map(|(lane, (event, begun))| {
            let number = event.data["number"].as_u64()?;
            let rounds = || {
                events.iter().filter(|e| {
                    e.seq > begun
                        && e.kind == EV_ROUND
                        && e.data["repo"] == repo
                        && e.data["number"] == number
                })
            };
            let tokens = rounds().map(round_tokens).sum::<u64>();
            let spent_usd = rounds()
                .flat_map(|e| e.data["steps"].as_array().cloned().unwrap_or_default())
                .filter_map(|step| step["cost_usd"].as_f64())
                .sum::<f64>()
                // An empty sum is -0.0, which JSON shows as such.
                + 0.0;
            let admitted = events
                .iter()
                .rev()
                .take_while(|e| e.seq >= begun)
                .find(|e| {
                    e.kind == EV_POOL_ADMITTED
                        && e.data["repo"] == repo
                        && e.data["number"] == number
                });
            let plan = admitted.and_then(|e| serde_json::from_value::<Plan>(e.data.clone()).ok());
            let admitted_ms = admitted
                .or_else(|| events.iter().find(|e| e.seq == begun))
                .map_or(event.ts_ms, |e| e.ts_ms);
            let mine: Vec<&Event> = events
                .iter()
                .filter(|e| e.seq > begun && e.data["repo"] == repo && e.data["number"] == number)
                .collect();
            let stage = event.data["stage"].as_str().unwrap_or_default().to_string();
            let high_risk = plan.as_ref().is_some_and(|p| p.high_risk);
            let paths = plan.as_ref().map(|p| p.paths.clone()).unwrap_or_default();
            let lane = LaneStatus {
                tokens,
                area: area(&paths),
                round: lane_round(&mine),
                admitted_ms,
                reviews: if stage == "review" {
                    lane_reviews(&mine, high_risk)
                } else {
                    Vec::new()
                },
                lane,
                number,
                title: title(number),
                stage,
                step: event.data["step"].as_str().map(str::to_string),
                since_ms: event.ts_ms,
                spent_usd,
                paths,
            };
            Some(((lane, plan), begun))
        })
        .unzip();
    let (lanes, lane_plans): (Vec<LaneStatus>, Vec<Option<Plan>>) = lanes.into_iter().unzip();
    let lane_plans: Vec<&Plan> = lane_plans.iter().flatten().collect();
    let since = started.map_or(u64::MAX, |start| events[start].seq);
    let plans = plans_since(events, repo, since);
    let planning = |number: u64| {
        events
            .iter()
            .rev()
            .take_while(|e| e.seq > since)
            .find(|e| {
                matches!(e.kind.as_str(), EV_POOL_PLANNING | EV_POOL_PLANNED)
                    && e.data["repo"] == repo
                    && e.data["number"] == number
            })
            .is_some_and(|e| e.kind == EV_POOL_PLANNING)
    };
    // The item's latest lane since the pool started ended on an error: it waits for the next
    // timed sync, as `--every` waits after a failed pass.
    let errored = |number: u64| {
        events
            .iter()
            .rev()
            .take_while(|e| e.seq > since)
            .find(|e| {
                e.kind == EV_POOL_LANE && e.data["repo"] == repo && e.data["number"] == number
            })
            .is_some_and(|e| e.data["stage"] == "done" && e.data["outcome"] == "error")
    };
    let admitted: Vec<u64> = lanes.iter().map(|lane| lane.number).collect();
    let (landing, merging) = land_turns(events, repo, &lanes, &begun);
    let why_waiting = |item: &WorkItem| {
        if !item.waiting_on.is_empty() {
            Why::WaitsOn(item.waiting_on.clone())
        } else if let Some(runner) = jono::held_by_other(events, item, jono::now_secs()) {
            Why::HeldBy(runner.to_string())
        } else if !running {
            Why::NoPool
        } else if let Some((other, why)) = plans
            .get(&item.number)
            .and_then(|plan| blocked_by(plan, &lane_plans))
        {
            Why::Overlaps(other, why)
        } else if planning(item.number) {
            Why::Planning
        } else if errored(item.number) {
            Why::LaneErrored
        } else {
            Why::NoFreeLane
        }
    };
    let queued: Vec<(&WorkItem, Why)> = items
        .iter()
        .filter(|item| item.open && item.status == Status::Queued)
        .filter(|item| !admitted.contains(&item.number))
        .map(|item| (item, why_waiting(item)))
        .collect();
    // Ready: nothing holds it back but a busy pool.
    let ready = queued
        .iter()
        .filter(|(_, why)| matches!(why, Why::NoFreeLane))
        .count();
    let waiting: Vec<WaitingItem> = queued
        .into_iter()
        .map(|(item, why)| {
            let paths = plans
                .get(&item.number)
                .map(|plan| plan.paths.clone())
                .unwrap_or_default();
            WaitingItem {
                number: item.number,
                title: item.title.clone(),
                why: why.to_string(),
                area: area(&paths),
                paths,
            }
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
    let since = started.filter(|_| running).map(|start| &events[start..]);
    let spent_usd = since
        .unwrap_or_default()
        .iter()
        .filter(|e| e.kind == EV_ROUND && e.data["repo"] == repo)
        .flat_map(|e| e.data["steps"].as_array().cloned().unwrap_or_default())
        .filter_map(|step| step["cost_usd"].as_f64())
        .sum::<f64>()
        + 0.0;
    let tokens = since
        .unwrap_or_default()
        .iter()
        .filter(|e| e.kind == EV_ROUND && e.data["repo"] == repo)
        .map(round_tokens)
        .sum();
    let landed_since_start = since
        .unwrap_or_default()
        .iter()
        .filter(|e| e.kind == EV_LANDED && e.data["repo"] == repo)
        .count();
    let exits = lane_exits(events, repo, &title);
    let retired_since_start = since.map_or(0, |since| {
        exits
            .iter()
            .filter(|(seq, exit)| {
                *seq > since[0].seq && matches!(exit.outcome.as_str(), "retired" | "failed")
            })
            .count()
    });
    let out = exits
        .into_iter()
        .map(|(_, exit)| exit)
        .take(LANDINGS)
        .collect();
    let held = items
        .iter()
        .filter(|item| item.open && item.status == Status::Escalated)
        .count();
    PoolStatus {
        repo: repo.to_string(),
        state: if running { "running" } else { "stopped" }.to_string(),
        target: target_lanes(events, repo).unwrap_or(1),
        busy: lanes.len(),
        admitted,
        lanes,
        waiting,
        landed,
        started_ms: since.map(|events| events[0].ts_ms),
        spent_usd,
        tokens,
        out,
        landed_since_start,
        retired_since_start,
        held,
        ready,
        landing,
        merging,
    }
}

/// `paja pool status` as text: a line for the pool, a table of its lanes, then what waits and
/// what landed. `now_ms` is the clock the times in step are read against.
pub fn describe(status: &PoolStatus, now_ms: u64) -> String {
    let target = format!("{} lanes", status.target);
    let mut out = vec![format!(
        "pool of {}: {}, {} busy of {target}",
        status.repo, status.state, status.busy
    )];
    if let Some(started) = status.started_ms {
        let minutes = now_ms.saturating_sub(started) / 60_000;
        out.push(format!(
            "running {}h{:02}m: spent ${:.2}, landed {}, {} ready, {} held for an answer",
            minutes / 60,
            minutes % 60,
            status.spent_usd,
            status.landed_since_start,
            status.ready,
            status.held
        ));
    }
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
    for lane in status.lanes.iter().filter(|lane| !lane.paths.is_empty()) {
        out.push(format!(
            "lane {} touches: {}",
            lane.lane,
            lane.paths.join(", ")
        ));
    }
    if let Some(number) = status.landing {
        out.push(format!("landing: #{number}"));
    }
    if !status.merging.is_empty() {
        let queue: Vec<String> = status.merging.iter().map(|n| format!("#{n}")).collect();
        out.push(format!("waiting to land: {}", queue.join(", ")));
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

    #[test]
    fn an_item_being_planned_again_waits_for_its_new_plan() {
        let failed = Plan {
            repo: "acme/app".into(),
            number: 5,
            paths: Vec::new(),
            source: "failed".into(),
            high_risk: false,
            error: Some("network".into()),
        };
        let plans = BTreeMap::from([(5, failed)]);
        let planning = BTreeSet::from([5]);
        let (chosen, to_plan) = choose(&[5], &plans, &[], &planning, 1, 1);
        assert!(chosen.is_empty() && to_plan.is_empty());
        // Once its planner is done, the plan it recorded decides.
        let (chosen, _) = choose(&[5], &plans, &[], &BTreeSet::new(), 1, 1);
        assert_eq!(chosen.len(), 1);
    }

    #[test]
    fn a_failed_plan_from_before_the_last_timed_sync_is_planned_again() {
        let planned = |seq: u64, number: u64, source: &str| Event {
            v: 1,
            seq,
            ts_ms: seq,
            kind: EV_POOL_PLANNED.to_string(),
            data: json!({"repo": "acme/app", "number": number, "paths": [], "source": source,
                         "high_risk": false}),
        };
        let events = vec![
            planned(2, 3, "failed"),
            planned(3, 4, "failed"),
            planned(4, 4, "spec"),
            planned(6, 5, "failed"),
            planned(7, 6, "failed"),
        ];
        // #3 failed before the cutoff, #4 has a newer plan, and #5 and #6 failed after it.
        let again: Vec<u64> = failed_by(&events, "acme/app", 1, 5).into_iter().collect();
        assert_eq!(again, [3]);
        assert!(failed_by(&events, "acme/app", 1, 0).is_empty());
        assert_eq!(
            failed_by(&events, "acme/app", 2, 7)
                .into_iter()
                .collect::<Vec<_>>(),
            [5, 6]
        );
    }

    #[cfg(unix)]
    #[test]
    fn stopping_ends_a_lane_that_never_stops() {
        let mut quick = Command::new("true").spawn().unwrap();
        let mut stuck = Command::new("sleep").arg("60").spawn().unwrap();
        let started = Instant::now();
        stop_children(
            [&mut quick, &mut stuck].into_iter(),
            Duration::from_millis(300),
        );
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(stuck.try_wait().unwrap().is_some());
        assert!(quick.try_wait().unwrap().is_some());
    }

    #[cfg(unix)]
    #[test]
    fn reaping_planners_fails_only_the_plans_that_recorded_none() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home")).unwrap();
        let at = record(&home, "test.start", json!({})).unwrap().seq;
        let planned = |number: u64| {
            let plan = json!({"repo": "acme/app", "number": number, "paths": ["src/"],
                "source": "plan"});
            record(&home, EV_POOL_PLANNED, plan).unwrap();
        };
        let ended = || {
            let mut child = Command::new("true").spawn().unwrap();
            child.wait().unwrap();
            child
        };
        let long_ago = Instant::now() - Duration::from_secs(120);
        let sleep = || Command::new("sleep").arg("60").spawn().unwrap();
        let mut planners = BTreeMap::new();
        // Hung past the limit, with and without a plan recorded just before it was ended.
        planners.insert(1, (sleep(), at, long_ago));
        planners.insert(2, (sleep(), at, long_ago));
        // Ended by itself, with and without a plan.
        planners.insert(3, (ended(), at, Instant::now()));
        planners.insert(4, (ended(), at, Instant::now()));
        // Still going, within the limit.
        planners.insert(5, (sleep(), at, Instant::now()));
        planned(2);
        planned(3);
        let quick = Duration::from_millis(100);
        let failed = reap(
            &home,
            "acme/app",
            &mut planners,
            &mut BTreeMap::new(),
            Duration::from_secs(60),
            quick,
        )
        .unwrap();
        let numbers: Vec<u64> = failed.iter().map(|(n, _)| *n).collect();
        assert_eq!(numbers, [1, 4], "{failed:?}");
        assert!(failed[0].1.contains("took longer than"), "{failed:?}");
        assert_eq!(failed[1].1, "the plan of #4 ended without one");
        assert_eq!(planners.keys().copied().collect::<Vec<_>>(), [5]);
        let (mut going, _, _) = planners.remove(&5).unwrap();
        let _ = going.kill();
        let _ = going.wait();
    }

    /// A planner refused because another `paja pool plan` of the item holds its lock waits for
    /// that plan: no failure while the lock is held, the other plan's record counts, and with
    /// the lock let go and nothing recorded it failed.
    #[cfg(unix)]
    #[test]
    fn a_planner_refused_by_another_plan_waits_for_that_plan() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home")).unwrap();
        let at = record(&home, "test.start", json!({})).unwrap().seq;
        let ended = || {
            let mut child = Command::new("true").spawn().unwrap();
            child.wait().unwrap();
            child
        };
        let long_ago = Instant::now() - Duration::from_secs(50);
        let (limit, grace) = (Duration::from_secs(60), Duration::from_millis(100));
        let mut planners = BTreeMap::new();
        planners.insert(5, (ended(), at, long_ago));
        planners.insert(6, (ended(), at, long_ago));
        let other5 = home.try_plan_lock("acme/app", 5).unwrap().unwrap();
        let other6 = home.try_plan_lock("acme/app", 6).unwrap().unwrap();
        let mut elsewhere = BTreeMap::new();
        let failed = reap(
            &home,
            "acme/app",
            &mut planners,
            &mut elsewhere,
            limit,
            grace,
        )
        .unwrap();
        assert!(failed.is_empty(), "{failed:?}");
        // Both now wait on the other plan, as an item the pool found held before starting one
        // does, with the wait counted from the refusal.
        assert!(planners.is_empty());
        assert_eq!(elsewhere.keys().copied().collect::<Vec<_>>(), [5, 6]);
        assert!(elsewhere[&5].elapsed() < Duration::from_secs(5));
        // Once the other plans let go, or an item leaves the queue, the wait on it ends.
        drop(other5);
        // Another test's child forked at that moment holds the lock until it execs.
        let deadline = Instant::now() + Duration::from_secs(5);
        while home.plan_held("acme/app", 5).unwrap() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        keep_waiting(&mut elsewhere, &BTreeSet::from([5, 6, 7]), |n| {
            home.plan_held("acme/app", n).unwrap_or(false)
        });
        assert_eq!(elsewhere.keys().copied().collect::<Vec<_>>(), [6]);
        keep_waiting(&mut elsewhere, &BTreeSet::from([5]), |_| true);
        assert!(elsewhere.is_empty());
        drop(other6);
        // Past the limit, the wait ends and the plan fails, unless a plan was recorded just
        // before; one not yet past it stays.
        let old = Instant::now() - Duration::from_secs(120);
        elsewhere.extend([(5, old), (6, old), (7, old), (8, Instant::now())]);
        let plan = |number: u64, source: &str| Plan {
            repo: "acme/app".into(),
            number,
            paths: vec!["src/".into()],
            source: source.into(),
            high_risk: false,
            error: None,
        };
        let plans = BTreeMap::from([(5, plan(5, "plan")), (6, plan(6, "failed"))]);
        assert_eq!(give_up(&mut elsewhere, &plans, limit), [6, 7]);
        assert_eq!(elsewhere.keys().copied().collect::<Vec<_>>(), [8]);
    }

    #[cfg(unix)]
    #[test]
    fn a_planner_past_its_limit_is_ended() {
        let mut planners = BTreeMap::new();
        let long_ago = Instant::now() - Duration::from_secs(5);
        let hung = Command::new("sleep").arg("60").spawn().unwrap();
        let fresh = Command::new("sleep").arg("60").spawn().unwrap();
        planners.insert(3, (hung, 0, long_ago));
        planners.insert(4, (fresh, 0, Instant::now()));
        let quick = Duration::from_millis(100);
        assert_eq!(
            end_overdue(&mut planners, Duration::from_secs(1), quick),
            [3]
        );
        assert!(planners
            .get_mut(&3)
            .unwrap()
            .0
            .try_wait()
            .unwrap()
            .is_some());
        let young = &mut planners.get_mut(&4).unwrap().0;
        assert!(young.try_wait().unwrap().is_none());
        let _ = young.kill();
        let _ = young.wait();
    }

    /// A planner just past its limit is told `stop now` and left to end by itself: the pool
    /// doesn't wait for it, and once it has ended its plan failed for its time.
    #[cfg(unix)]
    #[test]
    fn an_overdue_planner_is_told_to_stop_without_the_pool_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home")).unwrap();
        let at = record(&home, "test.start", json!({})).unwrap().seq;
        let told = dir.path().join("told");
        let planner = Command::new("sh")
            .arg("-c")
            .arg(format!("read line; echo \"$line\" > '{}'", told.display()))
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let mut planners = BTreeMap::new();
        planners.insert(3, (planner, at, Instant::now() - Duration::from_secs(2)));
        let started = Instant::now();
        let limit = Duration::from_secs(1);
        let grace = Duration::from_secs(60);
        assert!(reap(
            &home,
            "acme/app",
            &mut planners,
            &mut BTreeMap::new(),
            limit,
            grace
        )
        .unwrap()
        .is_empty());
        assert!(started.elapsed() < Duration::from_secs(5));
        let deadline = Instant::now() + Duration::from_secs(10);
        let failed = loop {
            let failed = reap(
                &home,
                "acme/app",
                &mut planners,
                &mut BTreeMap::new(),
                limit,
                grace,
            )
            .unwrap();
            if !failed.is_empty() || Instant::now() > deadline {
                break failed;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(std::fs::read_to_string(&told).unwrap().trim(), "stop now");
        assert_eq!(failed.len(), 1, "{failed:?}");
        assert!(failed[0].1.contains("took longer than"), "{failed:?}");
        assert!(planners.is_empty());
    }

    /// A stopped planner is left alone through its grace, then killed.
    #[cfg(unix)]
    #[test]
    fn a_stopped_planner_still_going_past_its_grace_is_killed() {
        let spawn = || {
            let mut cmd = Command::new("sleep");
            crate::platform::own_process_group(&mut cmd);
            cmd.arg("60").stdin(Stdio::piped()).spawn().unwrap()
        };
        let mut stopped = vec![
            (spawn(), Instant::now() - Duration::from_secs(10)),
            (spawn(), Instant::now()),
        ];
        let old = stopped[0].0.id().to_string();
        reap_stopped(&mut stopped, Duration::from_secs(5));
        assert_eq!(stopped.len(), 1);
        let alive = Command::new("kill").args(["-0", &old]).status().unwrap();
        assert!(
            !alive.success(),
            "the planner past its grace is still running"
        );
        let (mut young, _) = stopped.pop().unwrap();
        assert!(young.try_wait().unwrap().is_none());
        let _ = young.kill();
        let _ = young.wait();
    }

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
        assert_eq!(status.target, 3);
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
        assert_eq!(status.started_ms, Some(2000));
        assert!((status.spent_usd - 0.75).abs() < 1e-9);
        assert_eq!(status.landed_since_start, 1);
        assert_eq!((status.held, status.ready), (0, 1));
        let text = describe(&status, 67_000);
        assert!(text.contains("running, 1 busy of 3 lanes"), "{text}");
        assert!(text.contains("1m00s"), "{text}");
    }

    #[test]
    fn the_area_is_the_longest_shared_directory_or_the_first_top_folder() {
        let paths = |p: &[&str]| p.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(area(&[]), "");
        assert_eq!(area(&paths(&["crates/paja/src/a.rs"])), "crates/paja/src");
        assert_eq!(
            area(&paths(&["crates/paja/src/a.rs", "crates/paja/tests/b.rs"])),
            "crates/paja"
        );
        assert_eq!(area(&paths(&["docs/a.md", "crates/b.rs"])), "docs");
        assert_eq!(area(&paths(&["Cargo.toml"])), "");
    }

    #[test]
    fn lanes_show_their_round_reviews_time_and_tokens_and_items_out_show_how_they_left() {
        let r = "acme/app";
        let admitted = |seq, lane, number, high_risk, paths: &[&str]| {
            event(
                seq,
                EV_POOL_ADMITTED,
                json!({"repo": r, "number": number, "lane": lane, "paths": paths,
                       "source": "spec", "high_risk": high_risk}),
            )
        };
        let tokens = |n: u64| {
            json!({"input_tokens": n, "output_tokens": n,
            "cache_read_input_tokens": n, "cache_creation_input_tokens": n})
        };
        let round = |seq, number, n, next: bool| {
            let mut data = json!({"repo": r, "number": number, "round": n,
                "steps": [tokens(10), {"input_tokens": 1}]});
            if next {
                data["next"] = json!({"feedback": "fix"});
            }
            event(seq, EV_ROUND, data)
        };
        let step = |seq, number, n, reviewer: Option<u32>, reviewers: Option<u32>| {
            event(
                seq,
                EV_STEP,
                json!({"repo": r, "number": number, "round": n,
                       "role": if reviewer.is_some() { "review" } else { "build" },
                       "reviewer": reviewer, "reviewers": reviewers}),
            )
        };
        let reviewed = |seq, number, n, reviewer, verdict| {
            event(
                seq,
                super::super::EV_REVIEWED,
                json!({"repo": r, "number": number, "round": n, "reviewer": reviewer,
                       "verdict": verdict}),
            )
        };
        let closed =
            |seq, number| event(seq, jono::EV_CLOSED, json!({"repo": r, "number": number}));
        let done = |seq, lane, number, outcome| {
            let mut data = record_lane_data(r, lane, number, "done", None);
            data["outcome"] = json!(outcome);
            event(seq, EV_POOL_LANE, data)
        };
        let events = vec![
            event(1, EV_POOL_STARTED, json!({"repo": r, "lanes": 3})),
            // Four items leave first: landed, held, failed and retired.
            lane(2, 1, 11, "starting", None),
            done(3, 1, 11, "landed"),
            lane(4, 1, 12, "starting", None),
            done(5, 1, 12, "escalated"),
            lane(6, 1, 13, "starting", None),
            done(7, 1, 13, "error"),
            lane(8, 1, 14, "starting", None),
            closed(9, 14),
            done(10, 1, 14, "idle"),
            // A stopped item went back to the queue: it did not leave.
            lane(11, 1, 15, "starting", None),
            done(12, 1, 15, "stopped"),
            // Lane 1: #5, first round, building.
            lane(20, 1, 5, "starting", None),
            admitted(21, 1, 5, false, &["docs/a.md", "docs/b.md"]),
            step(22, 5, 1, None, None),
            lane(23, 1, 5, "build", Some("build")),
            // Lane 2: #6, in its second round (rework 1), one reviewer still out.
            lane(30, 2, 6, "starting", None),
            admitted(31, 2, 6, false, &["crates/x/src/a.rs"]),
            round(32, 6, 1, true),
            step(33, 6, 2, None, None),
            step(34, 6, 2, Some(1), Some(1)),
            lane(35, 2, 6, "review", Some("review")),
            // Lane 3: #7, high risk, third round, one of two reviewers passed.
            lane(40, 3, 7, "starting", None),
            admitted(41, 3, 7, true, &["a.rs"]),
            round(42, 7, 1, true),
            round(43, 7, 2, true),
            step(44, 7, 3, Some(1), Some(2)),
            reviewed(45, 7, 3, 1, "land"),
            step(46, 7, 3, Some(2), Some(2)),
            lane(47, 3, 7, "review", Some("review")),
        ];
        let items = [item(11, &[]), item(14, &[])];
        let status = from_events(&events, r, true, &items);
        let by = |n: u64| status.lanes.iter().find(|l| l.number == n).unwrap();
        assert_eq!((by(5).round, by(6).round, by(7).round), (1, 2, 3));
        assert_eq!(by(5).admitted_ms, 21_000);
        assert_eq!(by(5).area, "docs");
        assert_eq!(by(5).tokens, 0);
        // A finished round's complete steps count; the step with figures missing adds nothing.
        assert_eq!(by(6).tokens, 40);
        assert_eq!(by(7).tokens, 80);
        assert!(by(5).reviews.is_empty());
        assert_eq!(by(6).reviews, ["pending"]);
        assert_eq!(by(7).reviews, ["pass", "pending"]);
        assert_eq!(status.tokens, 120);
        // No lane count was ever recorded: a new pool starts with one lane.
        assert_eq!(status.target, 1);
        let out: Vec<(u64, &str)> = status
            .out
            .iter()
            .map(|o| (o.number, o.outcome.as_str()))
            .collect();
        assert_eq!(
            out,
            [
                (14, "retired"),
                (13, "failed"),
                (12, "held"),
                (11, "landed")
            ]
        );
        assert_eq!(status.out[0].ts_ms, 10_000);
        assert_eq!(status.out[3].title, "item 11");
        // Failed (#13) and retired (#14) both left without landing or being held.
        assert_eq!(status.retired_since_start, 2);
        // A reviewer's block shows, and a stopped pool has no lanes and none since its start.
        let mut more = events.clone();
        more.push(reviewed(48, 7, 3, 2, "changes"));
        let status = from_events(&more, r, true, &items);
        assert_eq!(
            status.lanes.iter().find(|l| l.number == 7).unwrap().reviews,
            ["pass", "block"]
        );
        let stopped = from_events(&events, r, false, &items);
        assert_eq!(stopped.retired_since_start, 0);
        assert_eq!(stopped.tokens, 0);
        // Only the latest five are out.
        let mut many = events.clone();
        many.extend([
            lane(50, 1, 16, "starting", None),
            done(51, 1, 16, "landed"),
            lane(52, 1, 17, "starting", None),
            done(53, 1, 17, "landed"),
        ]);
        let status = from_events(&many, r, true, &items);
        assert_eq!(status.out.len(), 5);
        assert_eq!(status.out[0].number, 17);
    }

    #[test]
    fn an_item_whose_lane_errored_says_it_waits_for_the_next_timed_sync() {
        let mut done = lane(4, 1, 5, "done", None);
        done.data["outcome"] = json!("error");
        let events = vec![
            event(1, EV_POOL_STARTED, json!({"repo": "acme/app", "lanes": 1})),
            lane(2, 1, 5, "starting", None),
            lane(3, 1, 5, "build", Some("build")),
            done,
        ];
        let status = from_events(&events, "acme/app", true, &[item(5, &[]), item(6, &[])]);
        assert!(
            status.waiting[0].why.contains("timed sync"),
            "{:?}",
            status.waiting
        );
        assert_eq!(status.waiting[1].why, "no free lane");
    }

    #[test]
    fn the_land_turn_and_the_merging_queue_come_from_the_lanes_latest_turn_events() {
        let turn = |seq, kind, number| {
            event(
                seq,
                kind,
                json!({"repo": "acme/app", "number": number, "round": 1}),
            )
        };
        let round = |seq, number| {
            event(
                seq,
                EV_ROUND,
                json!({"repo": "acme/app", "number": number, "round": 1, "steps": []}),
            )
        };
        let mut events = vec![
            event(1, EV_POOL_STARTED, json!({"repo": "acme/app", "lanes": 3})),
            lane(2, 1, 5, "starting", None),
            lane(3, 2, 6, "starting", None),
            lane(4, 3, 7, "starting", None),
            round(5, 5),
            round(6, 6),
            round(7, 7),
            turn(8, EV_LANDING, 5),
            turn(9, EV_MERGING, 7),
            turn(10, EV_MERGING, 6),
        ];
        let status = from_events(&events, "acme/app", true, &[]);
        assert_eq!(status.landing, Some(5));
        assert_eq!(status.merging, vec![7, 6]);
        let text = describe(&status, 10_000);
        assert!(
            text.contains("landing: #5\nwaiting to land: #7, #6"),
            "{text}"
        );
        // #5 went back for a round; #7 took the turn.
        events.extend([round(11, 5), turn(12, EV_LANDING, 7)]);
        let status = from_events(&events, "acme/app", true, &[]);
        assert_eq!((status.landing, status.merging), (Some(7), vec![6]));
        // A log written before the rename names them `pool.landing` and `pool.merging`.
        use super::super::land::{OLD_LANDING, OLD_MERGING};
        let mut old = events.clone();
        for e in &mut old {
            e.kind = match e.kind.as_str() {
                EV_LANDING => OLD_LANDING.to_string(),
                EV_MERGING => OLD_MERGING.to_string(),
                _ => e.kind.clone(),
            };
        }
        let status = from_events(&old, "acme/app", true, &[]);
        assert_eq!((status.landing, status.merging), (Some(7), vec![6]));
        // Upgraded midway: #6 waited under the old name and takes the turn under the new one.
        events.truncate(10);
        events[9].kind = OLD_MERGING.to_string();
        events.extend([round(11, 5), turn(12, EV_LANDING, 6)]);
        let status = from_events(&events, "acme/app", true, &[]);
        assert_eq!((status.landing, status.merging), (Some(6), vec![7]));
    }

    #[test]
    fn items_are_admitted_in_queue_order_and_skipped_only_for_an_overlap() {
        let plan = |number, path: &str| Plan {
            repo: "acme/app".into(),
            number,
            paths: vec![path.to_string()],
            source: "spec".into(),
            high_risk: false,
            error: None,
        };
        let numbers = |plans: Vec<&Plan>| plans.iter().map(|p| p.number).collect::<Vec<_>>();
        let none = BTreeSet::new();
        // #5 is still being planned when #6's plan is in: #6 waits for it.
        let plans: BTreeMap<u64, Plan> = [(6, plan(6, "b.rs")), (7, plan(7, "c.rs"))].into();
        let (chosen, to_plan) = choose(&[5, 6, 7], &plans, &[], &BTreeSet::from([5]), 2, 1);
        assert!(chosen.is_empty());
        assert!(to_plan.is_empty(), "#5 is already being planned");
        let (_, to_plan) = choose(&[5, 6, 8], &plans, &[], &none, 2, 2);
        assert_eq!(to_plan, [5, 8]);
        // Once planned, the first two go, in order.
        let plans: BTreeMap<u64, Plan> = [
            (5, plan(5, "a.rs")),
            (6, plan(6, "b.rs")),
            (7, plan(7, "c.rs")),
        ]
        .into();
        let (chosen, _) = choose(&[5, 6, 7], &plans, &[], &none, 2, 2);
        assert_eq!(numbers(chosen), [5, 6]);
        // One that overlaps a running plan, or one chosen before it, is skipped for the next.
        let running = plan(9, "b.rs");
        let (chosen, _) = choose(&[5, 6, 7], &plans, &[&running], &none, 2, 2);
        assert_eq!(numbers(chosen), [5, 7]);
        let plans: BTreeMap<u64, Plan> = [
            (5, plan(5, "src/")),
            (6, plan(6, "src/b.rs")),
            (7, plan(7, "c.rs")),
        ]
        .into();
        let (chosen, _) = choose(&[5, 6, 7], &plans, &[], &none, 2, 2);
        assert_eq!(numbers(chosen), [5, 7]);
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
    fn a_tamper_stop_counts_until_a_person_checked() {
        let stop = |seq| event(seq, crate::ahjo::EV_TAMPERED, json!({"repo": "/r/.git"}));
        let checked = |seq| {
            event(
                seq,
                crate::ahjo::EV_TAMPER_CHECKED,
                json!({"repo": "/r/.git"}),
            )
        };
        assert!(!tampered(&[], "/r/.git"));
        assert!(tampered(&[stop(1)], "/r/.git"));
        assert!(!tampered(&[stop(1)], "/other/.git"));
        assert!(!tampered(&[stop(1), checked(2)], "/r/.git"));
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
