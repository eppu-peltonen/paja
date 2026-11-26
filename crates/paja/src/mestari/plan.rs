//! `paja pool plan`: which files a work item will likely touch, so the pool admits to a lane
//! only items that don't overlap a running one. The item's `Touches:` line names them when it
//! has one; otherwise a short, read-only planning agent reads the default branch and lists them.

use super::*;

/// A plan of an item: `{repo, number, paths, source, high_risk, error?}`. `source` is `spec`
/// (its `Touches:` line), `plan` (the planning agent) or `failed`, which the pool treats as
/// touching everything.
pub const EV_POOL_PLANNED: &str = "pool.planned";

/// The most paths a plan keeps: one that names more is as good as "everything".
const MAX_PATHS: usize = 50;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Plan {
    pub repo: String,
    pub number: u64,
    /// Repository-relative paths, `/`-separated; a path ending in `/` is a directory.
    pub paths: Vec<String>,
    pub source: String,
    /// Whether any path is under `.paja/high-risk`, as main has it.
    pub high_risk: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The paths a spec's `Touches:` line names, comma- or space-separated, quotes and backticks
/// dropped. None without such a line.
pub fn touches_line(body: &str) -> Option<Vec<String>> {
    let line = body.lines().find_map(|line| {
        let line = line.trim().trim_start_matches(['-', '*', ' ']);
        let (head, rest) = line.split_once(':')?;
        head.trim()
            .trim_matches('*')
            .eq_ignore_ascii_case("touches")
            .then_some(rest)
    })?;
    let paths: Vec<String> = line
        .split([',', ' ', '\t'])
        .map(|p| clean(p.trim_matches(['`', '"', '\'', '*'])))
        .filter(|p| !p.is_empty())
        .collect();
    (!paths.is_empty()).then_some(paths)
}

/// A path as plans compare it: `/`-separated, no leading `./` or `/`.
fn clean(path: &str) -> String {
    let path = path.trim().replace('\\', "/");
    let path = path.trim_start_matches("./").trim_start_matches('/');
    path.to_string()
}

/// Whether two paths overlap: the same file, or one a directory holding the other.
pub fn overlap(a: &str, b: &str) -> bool {
    let (a, b) = (a.trim_end_matches('/'), b.trim_end_matches('/'));
    a == b
        || a.is_empty()
        || b.is_empty()
        || b.strip_prefix(a).is_some_and(|rest| rest.starts_with('/'))
        || a.strip_prefix(b).is_some_and(|rest| rest.starts_with('/'))
}

/// Why `plan` can't run beside the `running` plans: the first one it overlaps, and how. A plan
/// that failed touches everything, and two high-risk plans never run at once.
pub fn blocked_by(plan: &Plan, running: &[&Plan]) -> Option<(u64, String)> {
    for other in running {
        let why = if plan.source == "failed" {
            Some("its files are unknown".to_string())
        } else if other.source == "failed" {
            Some(format!("#{}'s files are unknown", other.number))
        } else if plan.high_risk && other.high_risk {
            Some("both touch high-risk paths".to_string())
        } else {
            plan.paths.iter().find_map(|a| {
                other.paths.iter().find(|b| overlap(a, b)).map(|b| {
                    if a == b {
                        a.clone()
                    } else {
                        format!("{a} and {b}")
                    }
                })
            })
        };
        if let Some(why) = why {
            return Some((other.number, why));
        }
    }
    None
}

/// Plan item `number` of the repository checked out at `repo`, and record it as
/// `pool.planned`. A plan that fails is recorded as `failed` before the error is returned.
pub fn plan(home: &Home, repo: &Path, number: u64) -> Result<Plan> {
    let repo = std::path::absolute(repo)?;
    let name = jono::repo_name(home, &repo)?;
    let made = make(home, &repo, &name, number);
    let plan = match made {
        Ok(plan) => plan,
        Err(err) => {
            let failed = Plan {
                repo: name.clone(),
                number,
                paths: Vec::new(),
                source: "failed".to_string(),
                high_risk: false,
                error: Some(format!("{err:#}")),
            };
            // A tampered repository gets no more git or gh, and no event that could hide it.
            if !is_tampered(&err) {
                let _ = record(home, EV_POOL_PLANNED, serde_json::to_value(&failed)?);
            }
            return Err(err);
        }
    };
    record(home, EV_POOL_PLANNED, serde_json::to_value(&plan)?)?;
    Ok(plan)
}

fn make(home: &Home, repo: &Path, name: &str, number: u64) -> Result<Plan> {
    ahjo::ensure_untampered(home, repo)?;
    let item = jono::list(home, name)?
        .into_iter()
        .find(|item| item.number == number)
        .with_context(|| {
            format!("{name} has no work item #{number}; `paja work list` shows the queue")
        })?;
    let target = RepoSettings::read(repo)?;
    let branch = target.branch.as_str();
    let base = fetch_tip(repo, branch).with_context(|| {
        format!(
            "fetching origin/{branch}; check the network and that `origin` is the GitHub \
             repository"
        )
    })?;
    let high_risk = lines(&read_from_base(repo, &base, HIGH_RISK_FILE)?.unwrap_or_default());
    let (paths, source) = match touches_line(&item.body) {
        Some(paths) => (paths, "spec"),
        None => {
            let models = Models::parse_on(
                &read_from_base(repo, &base, MODELS_FILE)?.unwrap_or_default(),
                branch,
            )?;
            crate::sandbox::activate(start_sandbox(home, repo, &base)?);
            (ask(home, repo, &base, &item, &models)?, "plan")
        }
    };
    let mut paths: Vec<String> = paths.iter().map(|p| clean(p)).collect();
    paths.retain(|p| !p.is_empty());
    paths.sort();
    paths.dedup();
    if paths.len() > MAX_PATHS {
        bail!(
            "the plan of #{number} names {} paths, more than {MAX_PATHS}; name the item's files in \
             a `Touches:` line, or split it",
            paths.len()
        );
    }
    if paths.is_empty() {
        bail!("the plan of #{number} names no paths; name them in a `Touches:` line");
    }
    Ok(Plan {
        repo: name.to_string(),
        number,
        // A directory holding a high-risk path is high risk too.
        high_risk: paths
            .iter()
            .any(|p| is_high_risk(p, &high_risk) || high_risk.iter().any(|h| overlap(p, h))),
        paths,
        source: source.to_string(),
        error: None,
    })
}

fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "paths": {"type": "array", "items": {"type": "string"}}
        },
        "required": ["paths"]
    })
}

/// The planning agent's prompt. It carries the marker the smoke tests' fake `claude` looks for.
fn prompt(item: &WorkItem) -> String {
    format!(
        "PAJA-PLAN. You are Paja's planner. Below is a work item for this repository, which is \
         checked out at the default branch. Don't change anything and don't plan the work in \
         detail: read only what you need to list the repository-relative paths of the files the \
         change will most likely create or edit, tests and docs included. Name a directory, \
         ending in `/`, only when the change could touch many files in it. Keep it short: a \
         handful of paths, at most {MAX_PATHS}.\n\nWork item #{}: {}\n\n{}",
        item.number, item.title, item.body
    )
}

/// Run the planning agent in a checkout of the default branch and read its paths.
fn ask(
    home: &Home,
    repo: &Path,
    base: &str,
    item: &WorkItem,
    models: &Models,
) -> Result<Vec<String>> {
    let name = format!("plan-{}", item.number);
    // A checkout left behind by a plan that was killed is reused, put back on the default branch.
    let env = match ahjo::get(home, &name) {
        Ok(env) => env,
        Err(_) => ahjo::create(
            home,
            CreateOptions {
                name: &name,
                repo,
                base: Some(base),
            },
        )?,
    };
    let result = (|| {
        discard(&env, base)?;
        let tools = [
            "Read",
            "Glob",
            "Grep",
            "Bash(git status*)",
            "Bash(git diff*)",
            "Bash(git log*)",
            "Bash(git show*)",
            "Bash(tail *)",
            "Bash(head *)",
            "Bash(grep *)",
            "Bash(wc *)",
            "Bash(pwd)",
        ];
        let out = claude(
            home,
            &env,
            base,
            &prompt(item),
            &tools,
            Some(&schema()),
            true,
            &models.args("plan"),
        )?;
        let paths = out["structured_output"]["paths"]
            .as_array()
            .context("the planning agent returned no paths")?
            .iter()
            .filter_map(|p| p.as_str().map(str::to_string))
            .collect();
        Ok(paths)
    })();
    if !matches!(&result, Err(err) if is_tampered(err)) {
        let _ = ahjo::remove(home, &name, true);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_touches_line_names_the_paths() {
        let body = "Do it.\n\nTouches: `src/a.rs`, docs/ ./src/b.rs\nMore.";
        assert_eq!(
            touches_line(body),
            Some(vec!["src/a.rs".into(), "docs/".into(), "src/b.rs".into()])
        );
        assert_eq!(
            touches_line("- **Touches:** src/x.rs"),
            Some(vec!["src/x.rs".into()])
        );
        assert_eq!(touches_line("Nothing here.\nTouches:"), None);
        assert_eq!(touches_line("It touches: nothing"), None);
    }

    #[test]
    fn paths_overlap_when_the_same_or_one_holds_the_other() {
        assert!(overlap("src/a.rs", "src/a.rs"));
        assert!(overlap("src/", "src/a.rs"));
        assert!(overlap("src/a.rs", "src"));
        assert!(!overlap("src/a", "src/ab.rs"));
        assert!(!overlap("src/a.rs", "docs/a.rs"));
    }

    #[test]
    fn a_plan_waits_for_an_overlapping_failed_or_second_high_risk_one() {
        let plan = |number, paths: &[&str], source: &str, high_risk| Plan {
            repo: "acme/app".into(),
            number,
            paths: paths.iter().map(|p| p.to_string()).collect(),
            source: source.into(),
            high_risk,
            error: None,
        };
        let a = plan(5, &["src/a.rs"], "spec", false);
        let b = plan(6, &["src/"], "plan", false);
        let c = plan(7, &["docs/c.md"], "plan", false);
        assert_eq!(
            blocked_by(&b, &[&c, &a]),
            Some((5, "src/ and src/a.rs".into()))
        );
        assert_eq!(blocked_by(&c, &[&a]), None);
        let failed = plan(8, &[], "failed", false);
        assert_eq!(
            blocked_by(&failed, &[&a]),
            Some((5, "its files are unknown".into()))
        );
        assert_eq!(
            blocked_by(&c, &[&failed]),
            Some((8, "#8's files are unknown".into()))
        );
        assert_eq!(blocked_by(&failed, &[]), None);
        let (h1, h2) = (
            plan(9, &["x"], "spec", true),
            plan(10, &["y"], "spec", true),
        );
        assert_eq!(
            blocked_by(&h2, &[&h1]),
            Some((9, "both touch high-risk paths".into()))
        );
        assert_eq!(blocked_by(&h2, &[&c]), None);
    }
}
