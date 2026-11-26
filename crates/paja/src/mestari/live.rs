//! Following a run from outside its terminal: where an item's latest run is (its round of
//! [`MAX_ROUNDS`], the step it is in and since when, each finished round's steps and spend, the
//! pull request once open), rebuilt from the event log alone. `paja work show`, its MCP tool
//! and Paja Desk's run view all read it from here.

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;

use super::{StepUsage, EV_ROUND, EV_STARTED, EV_STEP, MAX_ROUNDS};
use crate::events::Event;
use crate::home::Home;
use crate::jono::{self, Status, WorkItem};

/// An item's latest run, as far as the log tells it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Live {
    /// `running` while a `paja run` holds the item's run lock, `interrupted` when the item is
    /// still active but no run holds it (the run stopped or was killed), else the item's
    /// status (`queued`, `landed`, `escalated`).
    pub state: String,
    /// The run's Ahjo environment, while it exists.
    pub env: Option<LiveEnv>,
    /// The round the run is in, or the last one it reached; 0 before the first starts.
    pub round: u32,
    pub max_rounds: u32,
    /// The step that started last and hasn't finished with its round, if any.
    pub step: Option<LiveStep>,
    /// Every step of the run so far, oldest first.
    pub steps: Vec<LiveStep>,
    /// The run's finished rounds, oldest first, with what each step spent.
    pub rounds: Vec<LiveRound>,
    /// The pull request, once the run opened one.
    pub pr: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LiveEnv {
    pub name: String,
    pub path: std::path::PathBuf,
    pub branch: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LiveStep {
    pub round: u32,
    /// A `.paja/models` role (`build`, `fix`, `review`, ...), `checks`, `ci` or `land`.
    pub role: String,
    pub model: Option<String>,
    pub reviewer: Option<u32>,
    /// When it started, in milliseconds since the Unix epoch.
    pub started_ms: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LiveRound {
    pub round: u32,
    /// How the round ended, as `run.round` records it (`approved`, `changes`, ...).
    pub verdict: String,
    pub detail: String,
    /// What each agent step spent; a step whose record can't be read is left out.
    pub steps: Vec<StepUsage>,
}

/// The latest run of item `number` of `repo`, or None when no run has started it. Whether the
/// run is live comes from its run lock, which this takes and lets go of at once. It does so
/// holding the home lock, under which `paja run` claims items, so a claim never meets the
/// probe's hold and skips the item.
pub fn live(home: &Home, repo: &str, number: u64) -> Result<Option<Live>> {
    let events = home.events().read_all()?;
    let item = jono::replay_repo(&events, repo)?
        .remove(&number)
        .with_context(|| {
            format!("no work item #{number} in {repo}; run `paja work sync` in its checkout")
        })?;
    let held = item.status == Status::Active && {
        let _home = home.lock()?;
        home.try_run_lock(repo, number)?.is_none()
    };
    let envs = crate::ahjo::replay(&events)?;
    Ok(from_events(&events, &item, held, |name| {
        envs.get(name).map(|env| LiveEnv {
            name: env.name.clone(),
            path: env.path.clone(),
            branch: env.branch.clone(),
        })
    }))
}

/// [`live`] over events already read; `held` says whether a live run holds the item, and
/// `env` looks an environment up by name.
pub fn from_events(
    events: &[Event],
    item: &WorkItem,
    held: bool,
    env: impl Fn(&str) -> Option<LiveEnv>,
) -> Option<Live> {
    let mine: Vec<&Event> = jono::item_events(events, &item.repo, item.number).collect();
    let start = mine.iter().rposition(|e| e.kind == EV_STARTED)?;
    let run = &mine[start..];
    let state = match item.status {
        Status::Active if held => "running".to_string(),
        Status::Active => "interrupted".to_string(),
        other => other.as_str().to_string(),
    };
    let mut live = Live {
        state,
        env: run[0].data["env"].as_str().and_then(&env),
        round: 0,
        max_rounds: MAX_ROUNDS,
        step: None,
        steps: Vec::new(),
        rounds: Vec::new(),
        pr: None,
    };
    let number = |v: &Value| v.as_u64().map(|n| n as u32);
    for event in run {
        let data = &event.data;
        match event.kind.as_str() {
            EV_STEP => {
                let step = LiveStep {
                    round: number(&data["round"]).unwrap_or(0),
                    role: data["role"].as_str().unwrap_or("unknown").to_string(),
                    model: data["model"].as_str().map(str::to_string),
                    reviewer: number(&data["reviewer"]),
                    started_ms: event.ts_ms,
                };
                if let Some(pr) = data["pr"].as_str() {
                    live.pr = Some(pr.to_string());
                }
                live.round = live.round.max(step.round);
                live.step = Some(step.clone());
                live.steps.push(step);
            }
            EV_ROUND => {
                let round = number(&data["round"]).unwrap_or(0);
                let steps = match &data["steps"] {
                    Value::Array(steps) => steps
                        .iter()
                        .filter_map(|s| serde_json::from_value(s.clone()).ok())
                        .collect(),
                    _ => Vec::new(),
                };
                live.rounds.push(LiveRound {
                    round,
                    verdict: data["verdict"].as_str().unwrap_or_default().to_string(),
                    detail: data["detail"].as_str().unwrap_or_default().to_string(),
                    steps,
                });
                live.round = live.round.max(round);
                // The round is over: its steps are finished.
                live.step = None;
            }
            super::EV_LANDED => {
                if let Some(pr) = data["pr"].as_str() {
                    live.pr = Some(pr.to_string());
                }
                live.step = None;
            }
            super::EV_ESCALATED => live.step = None,
            _ => {}
        }
    }
    // A run that is over has no current step.
    if live.state != "running" && live.state != "interrupted" {
        live.step = None;
    }
    Some(live)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(seq: u64, kind: &str, data: Value) -> Event {
        Event {
            v: 1,
            seq,
            ts_ms: seq * 1000,
            kind: kind.to_string(),
            data,
        }
    }

    fn item(status: Status) -> WorkItem {
        serde_json::from_value(json!({
            "repo": "acme/app", "number": 7, "title": "t", "body": "", "url": "u",
            "open": true, "status": status,
        }))
        .unwrap()
    }

    fn run() -> Vec<Event> {
        let at = |n: u64| json!({"repo": "acme/app", "number": 7, "round": n});
        let step = |n: u64, role: &str| {
            let mut d = at(n);
            d["role"] = json!(role);
            d
        };
        vec![
            event(
                1,
                EV_STARTED,
                json!({"repo": "acme/app", "number": 7, "env": "app-1"}),
            ),
            event(2, EV_STEP, step(1, "build")),
            event(3, EV_STEP, step(1, "checks")),
            event(4, EV_STEP, {
                let mut d = step(1, "review");
                d["reviewer"] = json!(1);
                d["model"] = json!("opus");
                d
            }),
            event(5, EV_ROUND, {
                let mut d = at(1);
                d["verdict"] = json!("changes");
                d["detail"] = json!("fix the test");
                d["steps"] = json!([{"step": "build", "input_tokens": 10, "output_tokens": 2,
                    "cache_read_input_tokens": null, "cache_creation_input_tokens": null,
                    "cost_usd": 0.5, "num_turns": 3, "duration_ms": 60000, "session_id": "s"}]);
                d
            }),
            event(6, EV_STEP, step(2, "fix")),
        ]
    }

    #[test]
    fn a_running_item_shows_its_round_step_and_finished_rounds() {
        let env = |name: &str| {
            Some(LiveEnv {
                name: name.to_string(),
                path: "/w/app-1".into(),
                branch: "paja/app-1".to_string(),
            })
        };
        let live = from_events(&run(), &item(Status::Active), true, env).unwrap();
        assert_eq!(live.state, "running");
        assert_eq!(live.round, 2);
        assert_eq!(live.env.unwrap().branch, "paja/app-1");
        let step = live.step.unwrap();
        assert_eq!((step.role.as_str(), step.started_ms), ("fix", 6000));
        let roles: Vec<&str> = live.steps.iter().map(|s| s.role.as_str()).collect();
        assert_eq!(roles, ["build", "checks", "review", "fix"]);
        assert_eq!(live.steps[2].model.as_deref(), Some("opus"));
        assert_eq!(live.steps[2].reviewer, Some(1));
        assert_eq!(live.rounds.len(), 1);
        assert_eq!(live.rounds[0].verdict, "changes");
        assert_eq!(live.rounds[0].steps[0].input_tokens, Some(10));
    }

    #[test]
    fn an_active_item_no_run_holds_is_interrupted() {
        let live = from_events(&run(), &item(Status::Active), false, |_| None).unwrap();
        assert_eq!(live.state, "interrupted");
        assert!(live.env.is_none());
        assert_eq!(live.step.unwrap().role, "fix");
    }

    #[test]
    fn a_landed_run_has_its_pull_request_and_no_current_step() {
        let mut events = run();
        let pr = "https://github.com/acme/app/pull/9";
        events.push(event(
            7,
            EV_STEP,
            json!({"repo": "acme/app", "number": 7, "round": 2, "role": "ci", "pr": pr}),
        ));
        events.push(event(
            8,
            super::super::EV_LANDED,
            json!({"repo": "acme/app", "number": 7, "pr": pr, "rounds": 2}),
        ));
        let live = from_events(&events, &item(Status::Landed), false, |_| None).unwrap();
        assert_eq!(live.state, "landed");
        assert_eq!(live.pr.as_deref(), Some(pr));
        assert!(live.step.is_none());
    }

    #[test]
    fn an_item_no_run_started_has_no_live_view() {
        let events = vec![event(
            1,
            "work.imported",
            json!({"repo": "acme/app", "number": 7}),
        )];
        assert!(from_events(&events, &item(Status::Queued), false, |_| None).is_none());
    }

    #[test]
    fn a_log_from_before_run_step_still_reads() {
        let events = vec![
            event(
                1,
                EV_STARTED,
                json!({"repo": "acme/app", "number": 7, "env": "app-1"}),
            ),
            event(
                2,
                EV_ROUND,
                json!({"repo": "acme/app", "number": 7, "round": 1, "verdict": "changes",
                       "detail": "", "steps": "not a list"}),
            ),
        ];
        let live = from_events(&events, &item(Status::Active), true, |_| None).unwrap();
        assert_eq!((live.round, live.steps.len()), (1, 0));
        assert!(live.step.is_none());
        assert!(live.rounds[0].steps.is_empty());
    }
}
