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

use super::land::{EV_LANDING, EV_MERGING};
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
            self.reap_planners()?;
            let stopping = level > 0 || fatal.is_some();
            if stopping {
                // A plan is only worth having for a lane that may start.
                self.stop_planners();
            }
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
        let events = self.home.events().read_all()?;
        let mut plans = plans_since(&events, self.name, self.since);
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
        let planning: BTreeSet<u64> = self.planners.keys().copied().collect();
        // At most as many plans at once as there are free lanes to use them.
        let room = free.saturating_sub(self.planners.len());
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
    fn start_planner(&mut self, number: u64) -> Result<()> {
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
            Err(err) => self.plan_failed(number, &format!("{err:#}"))?,
        }
        Ok(())
    }

    /// Free the plans that ended; one that ended without recording a plan failed.
    fn reap_planners(&mut self) -> Result<()> {
        for (number, why) in reap(self.home, self.name, &mut self.planners, PLAN_LIMIT)? {
            self.plan_failed(number, &why)?;
        }
        Ok(())
    }

    /// Record a failed plan, which the pool admits only to an otherwise idle pool.
    fn plan_failed(&mut self, number: u64, error: &str) -> Result<()> {
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
        record(self.home, EV_POOL_PLANNED, serde_json::to_value(&failed)?).map(|_| ())
    }

    /// End the plans going: they are read-only, and an item left unplanned is planned again.
    fn stop_planners(&mut self) {
        for (child, _, _) in self.planners.values_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.planners.clear();
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
        self.stop_planners();
        for lane in self.lanes.values_mut() {
            lane.tell("stop now");
        }
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

/// Take out of `planners` the plans that ended, ending first any going longer than `limit`, and
/// return each that recorded no plan with why. A plan killed for its time may still have
/// recorded one in the instant before, which counts like any other.
fn reap(
    home: &Home,
    repo: &str,
    planners: &mut BTreeMap<u64, (Child, u64, Instant)>,
    limit: Duration,
) -> Result<Vec<(u64, String)>> {
    let overdue = end_overdue(planners, limit);
    let ended: Vec<(u64, u64)> = planners
        .iter_mut()
        .filter_map(|(&n, (child, at, _))| {
            (!matches!(child.try_wait(), Ok(None))).then_some((n, *at))
        })
        .collect();
    if ended.is_empty() {
        return Ok(Vec::new());
    }
    // The plan may have been recorded after this tick read the log.
    let events = home.events().read_all()?;
    let mut failed = Vec::new();
    for (number, at) in ended {
        planners.remove(&number);
        let planned = events.iter().any(|e| {
            e.seq > at
                && e.kind == EV_POOL_PLANNED
                && e.data["repo"] == repo
                && e.data["number"] == number
        });
        if planned {
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

/// End the planners in `planners` that have run longer than `limit`, and return their items.
/// They stay in the map, for [`reap`] to take out.
fn end_overdue(planners: &mut BTreeMap<u64, (Child, u64, Instant)>, limit: Duration) -> Vec<u64> {
    let mut overdue = Vec::new();
    for (&number, (child, _, started)) in planners.iter_mut() {
        if started.elapsed() > limit && matches!(child.try_wait(), Ok(None)) {
            let _ = child.kill();
            let _ = child.wait();
            overdue.push(number);
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

/// `paja pool plan <number>` as a child of the pool; its progress lines go to the pool's stderr.
fn spawn_planner(repo: &Path, number: u64) -> Result<Child> {
    let exe = std::env::current_exe().context("finding the paja program to plan with")?;
    let mut child = Command::new(exe)
        .args(["--json", "pool", "plan"])
        .arg(number.to_string())
        .arg("--repo")
        .arg(repo)
        .current_dir(repo)
        .stdin(Stdio::null())
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
    /// When the running pool started, in milliseconds since the Unix epoch; None while stopped.
    pub started_ms: Option<u64>,
    /// What the repository's finished rounds spent since the running pool started, in US
    /// dollars (Claude Code's estimate).
    pub spent_usd: f64,
    /// How many items landed since the running pool started.
    pub landed_since_start: usize,
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
    /// The files the lane was admitted for, from its `pool.admitted`.
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct WaitingItem {
    pub number: u64,
    pub title: String,
    pub why: String,
    /// The files its plan names, once it has one.
    pub paths: Vec<String>,
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
/// A lane's latest `pool.merging`, `pool.landing`, `run.round` or `run.landed` since its item
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
                    && [EV_MERGING, EV_LANDING, EV_ROUND, EV_LANDED].contains(&e.kind.as_str())
            });
        match latest {
            Some(e) if e.kind == EV_LANDING => landing = Some(lane.number),
            Some(e) if e.kind == EV_MERGING => merging.push((e.seq, lane.number)),
            _ => {}
        }
    }
    merging.sort();
    (landing, merging.into_iter().map(|(_, n)| n).collect())
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
            let plan = events
                .iter()
                .rev()
                .take_while(|e| e.seq >= begun)
                .find(|e| {
                    e.kind == EV_POOL_ADMITTED
                        && e.data["repo"] == repo
                        && e.data["number"] == number
                })
                .and_then(|e| serde_json::from_value::<Plan>(e.data.clone()).ok());
            let lane = LaneStatus {
                lane,
                number,
                title: title(number),
                stage: event.data["stage"].as_str().unwrap_or_default().to_string(),
                step: event.data["step"].as_str().map(str::to_string),
                since_ms: event.ts_ms,
                spent_usd,
                paths: plan.as_ref().map(|p| p.paths.clone()).unwrap_or_default(),
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
        .map(|(item, why)| WaitingItem {
            number: item.number,
            title: item.title.clone(),
            why: why.to_string(),
            paths: plans
                .get(&item.number)
                .map(|plan| plan.paths.clone())
                .unwrap_or_default(),
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
    let landed_since_start = since
        .unwrap_or_default()
        .iter()
        .filter(|e| e.kind == EV_LANDED && e.data["repo"] == repo)
        .count();
    let held = items
        .iter()
        .filter(|item| item.open && item.status == Status::Escalated)
        .count();
    PoolStatus {
        repo: repo.to_string(),
        state: if running { "running" } else { "stopped" }.to_string(),
        target: target_lanes(events, repo),
        busy: lanes.len(),
        admitted,
        lanes,
        waiting,
        landed,
        started_ms: since.map(|events| events[0].ts_ms),
        spent_usd,
        landed_since_start,
        held,
        ready,
        landing,
        merging,
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
        let failed = reap(&home, "acme/app", &mut planners, Duration::from_secs(60)).unwrap();
        let numbers: Vec<u64> = failed.iter().map(|(n, _)| *n).collect();
        assert_eq!(numbers, [1, 4], "{failed:?}");
        assert!(failed[0].1.contains("took longer than"), "{failed:?}");
        assert_eq!(failed[1].1, "the plan of #4 ended without one");
        assert_eq!(planners.keys().copied().collect::<Vec<_>>(), [5]);
        let (mut going, _, _) = planners.remove(&5).unwrap();
        let _ = going.kill();
        let _ = going.wait();
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
        assert_eq!(end_overdue(&mut planners, Duration::from_secs(1)), [3]);
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
        assert_eq!(status.started_ms, Some(2000));
        assert!((status.spent_usd - 0.75).abs() < 1e-9);
        assert_eq!(status.landed_since_start, 1);
        assert_eq!((status.held, status.ready), (0, 1));
        let text = describe(&status, 67_000);
        assert!(text.contains("running, 1 busy of 3 lanes"), "{text}");
        assert!(text.contains("1m00s"), "{text}");
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
