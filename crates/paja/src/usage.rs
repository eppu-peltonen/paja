//! `paja usage`: what each work item and each day spent, read back from the event log.
//!
//! Only `run.round` events carry spend (their `steps`); `run.landed` and `run.escalated` give
//! the outcome. A step whose token figures couldn't be read (any of input, output, cache read
//! or cache creation missing) is counted as unknown and adds nothing to any figure, so a total
//! is never a guess that looks exact. Days are UTC dates of the round's event.
//!
//! The median and 90th percentile per landed item are nearest-rank. The token and wall-time
//! ones leave out landed items that have an unknown step (they would be understated) and say
//! how many.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::Value;

use crate::events::Event;
use crate::home::Home;
use crate::jono;
use crate::mestari::{StepUsage, EV_ESCALATED, EV_LANDED, EV_ROUND};

const DAY_MS: u64 = 86_400_000;

/// What a set of steps spent. Unknown steps are counted, never added.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Spend {
    pub steps: u64,
    /// Steps whose usage couldn't be read.
    pub unknown_steps: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub turns: u64,
    pub wall_ms: u64,
}

impl Spend {
    fn add(&mut self, step: &StepUsage) {
        self.steps += 1;
        let (Some(i), Some(o), Some(r), Some(c)) = (
            step.input_tokens,
            step.output_tokens,
            step.cache_read_input_tokens,
            step.cache_creation_input_tokens,
        ) else {
            self.unknown_steps += 1;
            return;
        };
        self.input_tokens += i;
        self.output_tokens += o;
        self.cache_read_input_tokens += r;
        self.cache_creation_input_tokens += c;
        self.turns += step.num_turns.unwrap_or(0);
        self.wall_ms += step.duration_ms.unwrap_or(0);
    }

    /// Every token kind added up.
    pub fn tokens(&self) -> u64 {
        self.input_tokens
            + self.output_tokens
            + self.cache_read_input_tokens
            + self.cache_creation_input_tokens
    }
}

/// The steps of one role on one model.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RoleSpend {
    pub role: String,
    /// The model `.paja/models` asked for; null for the account's default.
    pub model: Option<String>,
    pub steps: u64,
    pub unknown_steps: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ItemUsage {
    pub number: u64,
    pub title: Option<String>,
    /// `landed`, `escalated`, `failed` (escalated after an error) or `active`.
    pub outcome: String,
    pub rounds: u64,
    pub by_role: Vec<RoleSpend>,
    #[serde(flatten)]
    pub spend: Spend,
}

#[derive(Debug, Clone, Serialize)]
pub struct DayUsage {
    /// UTC date, `YYYY-MM-DD`.
    pub date: String,
    #[serde(flatten)]
    pub spend: Spend,
}

/// Nearest-rank median and 90th percentile of one figure over landed items.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Percentiles {
    pub median: u64,
    pub p90: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct LandedStats {
    pub items: u64,
    pub rounds: Option<Percentiles>,
    /// All token kinds added up, over landed items without an unknown step.
    pub tokens: Option<Percentiles>,
    pub wall_ms: Option<Percentiles>,
    /// Landed items left out of `tokens` and `wall_ms` because a step's usage was unknown.
    pub left_out_unknown: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageReport {
    pub repo: String,
    /// The `--since` date, if one was given.
    pub since: Option<String>,
    pub items: Vec<ItemUsage>,
    pub days: Vec<DayUsage>,
    pub total: Spend,
    pub landed: LandedStats,
}

/// Days since 1970-01-01 of a civil date (proleptic Gregorian).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// The UTC date of a time in milliseconds since the epoch.
pub fn date_of(ts_ms: u64) -> String {
    let (y, m, d) = civil_from_days((ts_ms / DAY_MS) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

/// The start of a `YYYY-MM-DD` date (UTC), in milliseconds since the epoch.
pub fn parse_date(text: &str) -> Result<u64> {
    let bad = || format!("--since {text:?} is not a date; use YYYY-MM-DD, for example 2026-01-31");
    let parts: Vec<&str> = text.split('-').collect();
    let [y, m, d] = parts.as_slice() else {
        bail!("{}", bad());
    };
    let (y, m, d): (i64, i64, i64) = (
        y.parse().with_context(bad)?,
        m.parse().with_context(bad)?,
        d.parse().with_context(bad)?,
    );
    let days = days_from_civil(y, m, d);
    // Reject 2026-02-31 and the like: the date must survive the round trip.
    if y < 1970 || civil_from_days(days) != (y, m, d) {
        bail!("{}", bad());
    }
    Ok(days as u64 * DAY_MS)
}

fn percentile(sorted: &[u64], percent: usize) -> Option<u64> {
    let rank = (sorted.len() * percent).div_ceil(100).max(1);
    sorted.get(rank - 1).copied()
}

fn percentiles(mut values: Vec<u64>) -> Option<Percentiles> {
    values.sort_unstable();
    Some(Percentiles {
        median: percentile(&values, 50)?,
        p90: percentile(&values, 90)?,
    })
}

/// What `repo`'s recorded steps spent, from `events`. `item` keeps one work item; `since_ms`
/// keeps the rounds recorded at or after that time (the outcome is still the item's latest).
pub fn summarise(
    events: &[Event],
    repo: &str,
    titles: &BTreeMap<u64, String>,
    item: Option<u64>,
    since_ms: Option<u64>,
) -> UsageReport {
    #[derive(Default)]
    struct Acc {
        outcome: Option<&'static str>,
        last_verdict_error: bool,
        rounds: u64,
        spend: Spend,
        roles: BTreeMap<(String, Option<String>), (u64, u64)>,
    }
    let legacy = jono::legacy_repo(events, repo);
    let mut items: BTreeMap<u64, Acc> = BTreeMap::new();
    let mut days: BTreeMap<String, Spend> = BTreeMap::new();
    for event in events {
        if !matches!(event.kind.as_str(), EV_ROUND | EV_LANDED | EV_ESCALATED)
            || jono::event_repo(event, legacy) != repo
        {
            continue;
        }
        let Some(number) = event.data["number"].as_u64() else {
            continue;
        };
        if item.is_some_and(|n| n != number) {
            continue;
        }
        let acc = items.entry(number).or_default();
        match event.kind.as_str() {
            EV_ROUND => {
                acc.rounds = acc.rounds.max(event.data["round"].as_u64().unwrap_or(0));
                acc.last_verdict_error = event.data["verdict"] == "error";
                // A round after an outcome is the item running again.
                acc.outcome = None;
                if since_ms.is_some_and(|since| event.ts_ms < since) {
                    continue;
                }
                let steps = event.data["steps"].as_array().map_or(&[][..], |s| &s[..]);
                for step in steps {
                    let Ok(step) = serde_json::from_value::<StepUsage>(step.clone()) else {
                        continue;
                    };
                    acc.spend.add(&step);
                    days.entry(date_of(event.ts_ms)).or_default().add(&step);
                    let slot = acc
                        .roles
                        .entry((step.role.clone().unwrap_or(step.step.clone()), step.model))
                        .or_default();
                    slot.0 += 1;
                    slot.1 += u64::from(
                        step.input_tokens.is_none()
                            || step.output_tokens.is_none()
                            || step.cache_read_input_tokens.is_none()
                            || step.cache_creation_input_tokens.is_none(),
                    );
                }
            }
            EV_LANDED => {
                acc.rounds = acc.rounds.max(event.data["rounds"].as_u64().unwrap_or(0));
                acc.outcome = Some("landed");
            }
            _ => {
                acc.rounds = acc.rounds.max(event.data["rounds"].as_u64().unwrap_or(0));
                acc.outcome = Some(if acc.last_verdict_error {
                    "failed"
                } else {
                    "escalated"
                });
            }
        }
    }
    let mut total = Spend::default();
    let mut out = Vec::new();
    let (mut rounds, mut tokens, mut wall) = (Vec::new(), Vec::new(), Vec::new());
    let (mut landed, mut left_out) = (0, 0);
    for (number, acc) in items {
        // With --since, an item that spent nothing in the window isn't shown.
        if since_ms.is_some() && acc.spend.steps == 0 {
            continue;
        }
        let outcome = acc.outcome.unwrap_or("active");
        total.steps += acc.spend.steps;
        total.unknown_steps += acc.spend.unknown_steps;
        total.input_tokens += acc.spend.input_tokens;
        total.output_tokens += acc.spend.output_tokens;
        total.cache_read_input_tokens += acc.spend.cache_read_input_tokens;
        total.cache_creation_input_tokens += acc.spend.cache_creation_input_tokens;
        total.turns += acc.spend.turns;
        total.wall_ms += acc.spend.wall_ms;
        if outcome == "landed" {
            landed += 1;
            rounds.push(acc.rounds);
            if acc.spend.unknown_steps == 0 {
                tokens.push(acc.spend.tokens());
                wall.push(acc.spend.wall_ms);
            } else {
                left_out += 1;
            }
        }
        out.push(ItemUsage {
            number,
            title: titles.get(&number).cloned(),
            outcome: outcome.to_string(),
            rounds: acc.rounds,
            by_role: acc
                .roles
                .into_iter()
                .map(|((role, model), (steps, unknown_steps))| RoleSpend {
                    role,
                    model,
                    steps,
                    unknown_steps,
                })
                .collect(),
            spend: acc.spend,
        });
    }
    UsageReport {
        repo: repo.to_string(),
        since: since_ms.map(date_of),
        items: out,
        days: days
            .into_iter()
            .map(|(date, spend)| DayUsage { date, spend })
            .collect(),
        total,
        landed: LandedStats {
            items: landed,
            rounds: percentiles(rounds),
            tokens: percentiles(tokens),
            wall_ms: percentiles(wall),
            left_out_unknown: left_out,
        },
    }
}

/// The usage report for `repo` (`owner/name`) from the home's event log. `since` is a
/// `YYYY-MM-DD` date.
pub fn report(
    home: &Home,
    repo: &str,
    item: Option<u64>,
    since: Option<&str>,
) -> Result<UsageReport> {
    let since_ms = since.map(parse_date).transpose()?;
    let events = home.events().read_all()?;
    let titles = jono::list(home, repo)
        .map(|items| items.into_iter().map(|i| (i.number, i.title)).collect())
        .unwrap_or_default();
    Ok(summarise(&events, repo, &titles, item, since_ms))
}

fn figure(spend: &Spend) -> String {
    let unknown = if spend.unknown_steps > 0 {
        format!(", {} unknown", spend.unknown_steps)
    } else {
        String::new()
    };
    format!(
        "{} steps{unknown}, in {} out {} cache read {} cache write {}, {} turns, {}",
        spend.steps,
        spend.input_tokens,
        spend.output_tokens,
        spend.cache_read_input_tokens,
        spend.cache_creation_input_tokens,
        spend.turns,
        wall(spend.wall_ms),
    )
}

fn wall(ms: u64) -> String {
    let s = ms / 1000;
    format!("{}m{:02}s", s / 60, s % 60)
}

/// The report as text for a terminal.
pub fn describe(report: &UsageReport) -> String {
    if report.items.is_empty() {
        return format!("no recorded steps for {}", report.repo);
    }
    let mut lines = Vec::new();
    for item in &report.items {
        lines.push(format!(
            "#{} {} ({}, {} rounds): {}",
            item.number,
            item.title.as_deref().unwrap_or(""),
            item.outcome,
            item.rounds,
            figure(&item.spend)
        ));
        for r in &item.by_role {
            let unknown = if r.unknown_steps > 0 {
                format!(" ({} unknown)", r.unknown_steps)
            } else {
                String::new()
            };
            lines.push(format!(
                "    {} on {}: {} steps{unknown}",
                r.role,
                r.model.as_deref().unwrap_or("default model"),
                r.steps
            ));
        }
    }
    for day in &report.days {
        lines.push(format!("{}: {}", day.date, figure(&day.spend)));
    }
    lines.push(format!("total: {}", figure(&report.total)));
    let landed = &report.landed;
    if let (Some(rounds), Some(tokens)) = (&landed.rounds, &landed.tokens) {
        let (wall_median, wall_p90) = landed
            .wall_ms
            .as_ref()
            .map_or((0, 0), |w| (w.median, w.p90));
        lines.push(format!(
            "per landed item ({}): rounds median {} p90 {}, tokens median {} p90 {}, wall median {} p90 {}{}",
            landed.items,
            rounds.median,
            rounds.p90,
            tokens.median,
            tokens.p90,
            wall(wall_median),
            wall(wall_p90),
            if landed.left_out_unknown > 0 {
                format!(
                    "; {} left out of tokens and wall time: unknown steps",
                    landed.left_out_unknown
                )
            } else {
                String::new()
            }
        ));
    }
    lines.join("\n")
}

/// The JSON `paja usage --json` and the MCP tool `usage` print.
pub fn to_json(report: &UsageReport) -> Result<Value> {
    Ok(serde_json::to_value(report)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_round_trip_and_bad_ones_are_refused() {
        assert_eq!(parse_date("1970-01-02").unwrap(), DAY_MS);
        assert_eq!(date_of(parse_date("2026-11-27").unwrap() + 5), "2026-11-27");
        assert!(parse_date("2026-02-31").is_err());
        assert!(parse_date("yesterday").is_err());
    }

    #[test]
    fn percentiles_are_nearest_rank() {
        let p = percentiles(vec![5, 1, 3, 2, 4, 6, 7, 8, 9, 10]).unwrap();
        assert_eq!((p.median, p.p90), (5, 9));
        assert_eq!(percentiles(vec![]), None);
    }
}
