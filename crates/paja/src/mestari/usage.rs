//! What a step spent, and what the usage window looked like afterwards.
//!
//! The window: `claude -p --output-format stream-json --verbose` prints the account's rate-limit
//! state as `rate_limit_event` messages, whose `rate_limit_info` holds `status` (`allowed`,
//! `allowed_warning` or `rejected`) and `resetsAt` (epoch seconds) for the limit that binds now
//! (`rateLimitType`, such as `five_hour`), and `unifiedWindows`: each window by name, with its
//! `utilization` (the share used, 0 to 1), `resetsAt` and `status`. A top-level `utilization`
//! is read too, when there is one. `agent::fold_stream` reduces the stream to the final `result`
//! message and adds the latest event's `rate_limit_info` to it unless the result has one. Paja
//! records `status`, the used percent and the reset time on the step (`window_status`,
//! `window_used_percent`, `window_resets_at`; see [`window_of`] for which window's). A Claude Code
//! whose result has no `rate_limit_info` leaves all three `None`, and then nothing is held back:
//! the window is only ever judged on what was reported, and `paja doctor` warns while none of the
//! latest steps has a window.
//!
//! Before a `build` or `fix` session, and before a new item is claimed, Mestari reads the latest
//! recorded state ([`window_nearly_used`]); `window <percent>` in `.paja/models` on the default
//! branch sets the used share that counts as nearly used (default 90).

use super::*;

/// What one agent step spent, kept from the `claude -p` result. A field the result doesn't
/// have (an older Claude Code, a fake in tests) stays `None`: unknown, never zero.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StepUsage {
    /// `build` or `review`.
    pub step: String,
    /// Which reviewer of the round (1 or 2), for a review.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer: Option<u32>,
    /// The role's line in `.paja/models` (`build`, `fix`, `review` and so on).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// The model `.paja/models` asked for, or None for the account's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// What each model the session used spent, by model name (Claude Code's `modelUsage`).
    #[serde(default)]
    pub models: std::collections::BTreeMap<String, ModelUsage>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    /// Claude Code's estimate of the step's cost in US dollars.
    pub cost_usd: Option<f64>,
    pub num_turns: Option<u64>,
    pub duration_ms: Option<u64>,
    pub session_id: Option<String>,
    /// The step resumed an earlier session of the same run (a builder's fix round).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub resumed: bool,
    /// The `--fallback-model` the step was given, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
    /// The fallback model served part of the step: it spent something in `models`, so the role
    /// didn't run only on the model `.paja/models` chose for it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fell_back: bool,
    /// The usage window's status as the step's result reported it (`allowed`, `allowed_warning`,
    /// `rejected`), if it reported one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_status: Option<String>,
    /// How much of the window was used, in percent, if the result said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_used_percent: Option<f64>,
    /// When the window resets, in milliseconds since the epoch, if the result said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_resets_at: Option<u64>,
}

/// The status, used percent and reset time (milliseconds since the epoch) a `rate_limit_info`
/// reports. The status is the top-level one, the limit that binds now. The used share is the
/// top-level `utilization` when there is one. Otherwise it comes from `unifiedWindows`: while the
/// status is a warning (or rejected), the binding window's (`rateLimitType`), with the top-level
/// reset time, so the hold lasts until that limit resets; else the fullest window's, with its own
/// reset time, so a window that is nearly used holds builds back until it resets. Every window
/// counts, whichever model it is for, and `isUsingOverage` (extra usage that keeps work going
/// past a full window) is not read: both err on holding back.
pub fn window_of(info: &Value) -> (Option<String>, Option<f64>, Option<u64>) {
    let as_ms = |n: u64| if n < 100_000_000_000 { n * 1000 } else { n };
    let status = info["status"].as_str().map(str::to_string);
    let top_reset = info["resetsAt"].as_u64().map(as_ms);
    if let Some(used) = info["utilization"].as_f64() {
        return (status, Some(used * 100.0), top_reset);
    }
    let windows = info["unifiedWindows"].as_object();
    let warned = status
        .as_deref()
        .is_some_and(|s| s.contains("warning") || s == "rejected");
    let binding = info["rateLimitType"]
        .as_str()
        .and_then(|kind| windows?.get(kind));
    if warned || windows.is_none() {
        let used = binding.and_then(|w| w["utilization"].as_f64());
        let resets = top_reset.or_else(|| binding.and_then(|w| w["resetsAt"].as_u64()).map(as_ms));
        return (status, used.map(|u| u * 100.0), resets);
    }
    // Only a window with its own reset time: another window's would end the hold too early.
    let fullest = windows
        .into_iter()
        .flat_map(|w| w.values())
        .filter_map(|w| Some((w["utilization"].as_f64()?, w["resetsAt"].as_u64()?)))
        .max_by(|a, b| a.0.total_cmp(&b.0));
    match fullest {
        Some((used, resets)) => (status, Some(used * 100.0), Some(as_ms(resets))),
        None => (status, None, top_reset),
    }
}

impl StepUsage {
    /// Whether the step's result reported the usage window at all.
    pub fn reports_window(&self) -> bool {
        self.window_status.is_some()
            || self.window_used_percent.is_some()
            || self.window_resets_at.is_some()
    }

    /// The agent steps the log's `run.round` events recorded, oldest first.
    pub fn recorded(events: &[crate::events::Event]) -> impl Iterator<Item = StepUsage> + '_ {
        events
            .iter()
            .filter(|e| e.kind == EV_ROUND)
            .filter_map(|e| e.data["steps"].as_array())
            .flatten()
            .filter_map(|s| serde_json::from_value::<StepUsage>(s.clone()).ok())
    }
}

/// The window state the latest step that reported one recorded, when it says the window is nearly
/// used up (a warning or rejected status, or at least `threshold_percent` used) and its reset
/// time, still ahead of `now_ms`, is known: that reset time, in milliseconds since the epoch.
/// A state that has already reset, or that has no reset time, never holds anything back.
pub fn window_nearly_used(
    events: &[crate::events::Event],
    now_ms: u64,
    threshold_percent: u32,
) -> Option<u64> {
    let latest = StepUsage::recorded(events)
        .filter(StepUsage::reports_window)
        .last()?;
    let warned = latest
        .window_status
        .as_deref()
        .is_some_and(|s| s.contains("warning") || s == "rejected");
    let used = latest
        .window_used_percent
        .is_some_and(|p| p >= f64::from(threshold_percent));
    (warned || used)
        .then_some(latest.window_resets_at)
        .flatten()
        .filter(|&resets| resets > now_ms)
}

/// Whether `fallback` spent anything in a step's `models`. Claude Code names models by their
/// full id, so an alias such as `sonnet` matches any id that contains it.
pub(super) fn served_by(
    models: &std::collections::BTreeMap<String, ModelUsage>,
    fallback: &str,
) -> bool {
    models.iter().any(|(name, used)| {
        name.contains(fallback)
            && (used.output_tokens.is_some_and(|n| n > 0) || used.cost_usd.is_some_and(|c| c > 0.0))
    })
}

/// One model's share of a step, from Claude Code's `modelUsage`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub cache_read_input_tokens: Option<u64>,
    pub cache_creation_input_tokens: Option<u64>,
    pub cost_usd: Option<f64>,
}

impl StepUsage {
    /// This step's own share of a resumed session. Claude Code reports `total_cost_usd` and
    /// `modelUsage` for the whole session so far, so the earlier result's values come off;
    /// `usage` already counts only this run.
    pub fn since(&self, earlier: &StepUsage) -> Self {
        fn less<T: std::ops::Sub<Output = T> + PartialOrd + Copy>(
            now: Option<T>,
            before: Option<T>,
        ) -> Option<T> {
            match (now, before) {
                (Some(now), Some(before)) if now >= before => Some(now - before),
                // Unknown, or a total that went down: say unknown, never guess.
                (Some(_), Some(_)) | (Some(_), None) | (None, _) => None,
            }
        }
        let mut step = self.clone();
        step.cost_usd = less(self.cost_usd, earlier.cost_usd);
        for (name, used) in &mut step.models {
            let before = earlier.models.get(name).cloned().unwrap_or(ModelUsage {
                input_tokens: Some(0),
                output_tokens: Some(0),
                cache_read_input_tokens: Some(0),
                cache_creation_input_tokens: Some(0),
                cost_usd: Some(0.0),
            });
            used.input_tokens = less(used.input_tokens, before.input_tokens);
            used.output_tokens = less(used.output_tokens, before.output_tokens);
            used.cache_read_input_tokens =
                less(used.cache_read_input_tokens, before.cache_read_input_tokens);
            used.cache_creation_input_tokens = less(
                used.cache_creation_input_tokens,
                before.cache_creation_input_tokens,
            );
            used.cost_usd = less(used.cost_usd, before.cost_usd);
        }
        step.resumed = true;
        // Judged on this step's own share: an earlier round's fallback doesn't carry over.
        step.fell_back = step
            .fallback
            .as_deref()
            .is_some_and(|f| served_by(&step.models, f));
        step
    }

    /// Read the usage fields from a `claude -p` JSON result.
    pub fn from_result(
        step: &str,
        reviewer: Option<u32>,
        chosen: &Models,
        role: &str,
        result: &Value,
    ) -> Self {
        let usage = &result["usage"];
        let models = result["modelUsage"]
            .as_object()
            .map(|models| {
                models
                    .iter()
                    .map(|(name, m)| {
                        let used = ModelUsage {
                            input_tokens: m["inputTokens"].as_u64(),
                            output_tokens: m["outputTokens"].as_u64(),
                            cache_read_input_tokens: m["cacheReadInputTokens"].as_u64(),
                            cache_creation_input_tokens: m["cacheCreationInputTokens"].as_u64(),
                            cost_usd: m["costUSD"].as_f64(),
                        };
                        (name.clone(), used)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let fallback = chosen.fallback_for(role).map(str::to_string);
        // The window, if the result reports it: see the module doc comment.
        let (window_status, window_used_percent, window_resets_at) =
            window_of(&result["rate_limit_info"]);
        StepUsage {
            window_status,
            window_used_percent,
            window_resets_at,
            step: step.to_string(),
            reviewer,
            role: Some(role.to_string()),
            model: chosen.chosen(role).map(|m| m.model.clone()),
            input_tokens: usage["input_tokens"].as_u64(),
            output_tokens: usage["output_tokens"].as_u64(),
            cache_read_input_tokens: usage["cache_read_input_tokens"].as_u64(),
            cache_creation_input_tokens: usage["cache_creation_input_tokens"].as_u64(),
            cost_usd: result["total_cost_usd"].as_f64(),
            num_turns: result["num_turns"].as_u64(),
            duration_ms: result["duration_ms"].as_u64(),
            session_id: result["session_id"].as_str().map(str::to_string),
            resumed: false,
            fell_back: fallback.as_deref().is_some_and(|f| served_by(&models, f)),
            fallback,
            models,
        }
    }
}

/// What the recorded `run.round` steps of `session` add up to, or None when none was recorded
/// (a session that was killed before its result) or a figure is unknown.
pub(super) fn recorded_totals(
    events: &[crate::events::Event],
    repo: &str,
    number: u64,
    session: &str,
) -> Option<StepUsage> {
    fn plus<T: std::ops::Add<Output = T>>(a: Option<T>, b: Option<T>) -> Option<T> {
        Some(a? + b?)
    }
    let mut total: Option<StepUsage> = None;
    for event in crate::jono::item_events(events, repo, number).filter(|e| e.kind == EV_ROUND) {
        let Some(steps) = event.data["steps"].as_array() else {
            continue;
        };
        for step in steps {
            let Ok(step) = serde_json::from_value::<StepUsage>(step.clone()) else {
                continue;
            };
            if step.step != "build" || step.session_id.as_deref() != Some(session) {
                continue;
            }
            total = Some(match total {
                None => step,
                Some(mut sum) => {
                    sum.cost_usd = plus(sum.cost_usd, step.cost_usd);
                    for (name, used) in step.models {
                        let slot = sum.models.entry(name).or_insert_with(|| ModelUsage {
                            input_tokens: Some(0),
                            output_tokens: Some(0),
                            cache_read_input_tokens: Some(0),
                            cache_creation_input_tokens: Some(0),
                            cost_usd: Some(0.0),
                        });
                        slot.input_tokens = plus(slot.input_tokens, used.input_tokens);
                        slot.output_tokens = plus(slot.output_tokens, used.output_tokens);
                        slot.cache_read_input_tokens =
                            plus(slot.cache_read_input_tokens, used.cache_read_input_tokens);
                        slot.cache_creation_input_tokens = plus(
                            slot.cache_creation_input_tokens,
                            used.cache_creation_input_tokens,
                        );
                        slot.cost_usd = plus(slot.cost_usd, used.cost_usd);
                    }
                    sum
                }
            });
        }
    }
    total.map(|mut sum| {
        sum.session_id = Some(session.to_string());
        sum
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_example_models_file_for_a_smaller_plan_parses() {
        let text = include_str!("../../../../docs/usage-models.example");
        let models = super::Models::parse(text).unwrap();
        assert_eq!(models.roles["build"].model, "sonnet");
        assert_eq!(models.roles["fix"].model, "haiku");
        assert_eq!(models.max_turns["build"], 50);
        assert_eq!(models.window_percent(), 80);
    }

    #[test]
    fn the_window_is_read_from_the_unified_windows_claude_code_reports() {
        use super::window_of;
        use serde_json::json;
        // The shape a current Claude Code prints: no top-level utilization.
        let info = |status: &str, five: f64, seven: f64| {
            json!({"status": status, "resetsAt": 1_000, "rateLimitType": "five_hour",
                "isUsingOverage": false, "unifiedWindows": {
                    "five_hour": {"utilization": five, "resetsAt": 1_000, "status": status},
                    "seven_day": {"utilization": seven, "resetsAt": 9_000, "status": "allowed"}}})
        };
        let percent = |u: f64| Some(u * 100.0);
        // Allowed: the fullest window, with its own reset time.
        assert_eq!(
            window_of(&info("allowed", 0.66, 0.43)),
            (Some("allowed".into()), percent(0.66), Some(1_000_000))
        );
        assert_eq!(
            window_of(&info("allowed", 0.2, 0.95)),
            (Some("allowed".into()), percent(0.95), Some(9_000_000))
        );
        // A warning: the binding window, until the binding limit resets.
        assert_eq!(
            window_of(&info("allowed_warning", 0.91, 0.95)),
            (
                Some("allowed_warning".into()),
                percent(0.91),
                Some(1_000_000)
            )
        );
        // A top-level utilization is read as it is; a bare status keeps its reset time.
        assert_eq!(
            window_of(&json!({"status": "allowed", "utilization": 0.5, "resetsAt": 2})),
            (Some("allowed".into()), Some(50.0), Some(2_000))
        );
        assert_eq!(
            window_of(&json!({"status": "rejected", "resetsAt": 3})),
            (Some("rejected".into()), None, Some(3_000))
        );
        assert_eq!(window_of(&serde_json::Value::Null), (None, None, None));
        // A warning with no top-level reset waits for the binding window's own.
        let warned = json!({"status": "allowed_warning", "rateLimitType": "five_hour",
            "unifiedWindows": {"five_hour": {"utilization": 0.9, "resetsAt": 4}}});
        assert_eq!(window_of(&warned).2, Some(4_000));
        // A window with no reset time of its own is passed over for the fullest that has one.
        let unknown = json!({"status": "allowed", "resetsAt": 1, "unifiedWindows": {
            "five_hour": {"utilization": 0.3, "resetsAt": 1},
            "other": {"utilization": 0.99}}});
        assert_eq!(
            window_of(&unknown),
            (Some("allowed".into()), percent(0.3), Some(1_000))
        );
    }

    #[test]
    fn the_latest_reported_window_decides_whether_it_is_nearly_used() {
        use super::{window_nearly_used, Models, StepUsage};
        let none = Models::default();
        let step = |info: serde_json::Value| {
            let result = serde_json::json!({ "rate_limit_info": info });
            let step = StepUsage::from_result("build", None, &none, "build", &result);
            crate::events::Event {
                v: 1,
                seq: 1,
                ts_ms: 0,
                kind: super::EV_ROUND.to_string(),
                data: serde_json::json!({ "steps": [step] }),
            }
        };
        let now = 1_000_000;
        let warned = step(serde_json::json!({"status": "allowed_warning", "resetsAt": 5_000}));
        assert_eq!(
            window_nearly_used(std::slice::from_ref(&warned), now, 90),
            Some(5_000_000)
        );
        // A used share over the threshold counts; under it, or already reset, does not.
        let used =
            step(serde_json::json!({"status": "allowed", "utilization": 0.91, "resetsAt": 5_000}));
        let used_only = std::slice::from_ref(&used);
        assert_eq!(window_nearly_used(used_only, now, 90), Some(5_000_000));
        assert_eq!(window_nearly_used(used_only, now, 95), None);
        assert_eq!(window_nearly_used(used_only, 6_000_000, 90), None);
        // Without a reset time, or with nothing reported, nothing is held back.
        let unknown = step(serde_json::json!({"status": "allowed_warning"}));
        assert_eq!(window_nearly_used(&[unknown], now, 90), None);
        assert_eq!(
            window_nearly_used(&[step(serde_json::json!(null))], now, 90),
            None
        );
        // A later report that the window is fine replaces an earlier warning.
        let fine =
            step(serde_json::json!({"status": "allowed", "utilization": 0.2, "resetsAt": 5_000}));
        assert_eq!(window_nearly_used(&[warned, fine], now, 90), None);
    }

    #[test]
    fn a_resumed_step_counts_only_its_own_spend() {
        use super::{Models, StepUsage};
        let none = Models::default();
        let first = StepUsage::from_result(
            "build",
            None,
            &none,
            "build",
            &serde_json::json!({"total_cost_usd": 0.5, "usage": {"input_tokens": 10},
                "modelUsage": {"m": {"inputTokens": 10, "costUSD": 0.5}}}),
        );
        let later = StepUsage::from_result(
            "build",
            None,
            &none,
            "fix",
            &serde_json::json!({"total_cost_usd": 0.75, "usage": {"input_tokens": 4},
                "modelUsage": {"m": {"inputTokens": 14, "costUSD": 0.75}}}),
        );
        let step = later.since(&first);
        assert!(step.resumed);
        assert_eq!(step.cost_usd, Some(0.25));
        // `usage` already counts only this run; `modelUsage` is the session's running total.
        assert_eq!(step.input_tokens, Some(4));
        assert_eq!(step.models["m"].input_tokens, Some(4));
        assert_eq!(step.models["m"].cost_usd, Some(0.25));
        // An earlier total that is unknown leaves this step's cost unknown, never a guess.
        let unknown = StepUsage::from_result("build", None, &none, "build", &serde_json::json!({}));
        assert_eq!(later.since(&unknown).cost_usd, None);
    }

    #[test]
    fn a_step_says_when_the_fallback_model_served_it() {
        use super::{Models, StepUsage};
        let models = Models::parse("build opus\nfix sonnet\nfallback sonnet\n").unwrap();
        let result = |sonnet_cost: f64| {
            serde_json::json!({"modelUsage": {
                "claude-opus-x": {"outputTokens": 5, "costUSD": 0.5},
                "claude-sonnet-x": {"outputTokens": 0, "costUSD": sonnet_cost}}})
        };
        let opus_only = StepUsage::from_result("build", None, &models, "build", &result(0.0));
        assert_eq!(opus_only.fallback.as_deref(), Some("sonnet"));
        assert!(!opus_only.fell_back);
        let fell = StepUsage::from_result("build", None, &models, "build", &result(0.1));
        assert!(fell.fell_back);
        // A role already on the fallback model gets none, so it never falls back.
        let fix = StepUsage::from_result("build", None, &models, "fix", &result(0.1));
        assert_eq!(fix.fallback, None);
        assert!(!fix.fell_back);
        // A resumed step is judged on its own share: the earlier fallback doesn't carry over.
        let later = StepUsage::from_result("build", None, &models, "build", &result(0.1));
        assert!(!later.since(&fell).fell_back);
        assert!(later.since(&opus_only).fell_back);
    }
}
