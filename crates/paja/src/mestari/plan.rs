//! `paja pool plan`: which files a work item will likely touch, so the pool admits to a lane
//! only items that don't overlap a running one. The item's `Touches:` line names them when it
//! has one; otherwise a short, read-only planning agent reads the default branch and lists them.

use super::*;

/// A plan of an item: `{repo, number, paths, source, high_risk, basis?, error?}`. `source` is `spec`
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
    /// What the plan was made from: [`text_basis`] of the item's title and text as the plan read
    /// them, so a pool can tell a plan of the item as it is now from one of older text. Absent
    /// from a plan that failed before it read the item, and from plans older releases recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub basis: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// A short, stable fingerprint of an item's title and text (64-bit FNV-1a, in hex): the same
/// in every release and on every platform, so a plan's `basis` can be compared with the item
/// as a later sync left it.
pub fn text_basis(title: &str, body: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    // A NUL between them, which no GitHub title holds, keeps "a" + "bc" apart from "ab" + "c".
    for byte in title.bytes().chain([0]).chain(body.bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// Whether a plan of `paths` is high risk by the `high_risk` prefixes: a path under one, or a
/// directory holding one.
pub fn judge_high_risk(paths: &[String], high_risk: &[String]) -> bool {
    paths
        .iter()
        .any(|p| is_high_risk(p, high_risk) || high_risk.iter().any(|h| overlap(p, h)))
}

/// The paths a spec's `Touches:` line names, comma- or space-separated, quotes, backticks and
/// trailing punctuation dropped, and the words `and`, `or`, `none` and `nothing` skipped. None
/// without such a line, or when it names no path.
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
        .map(token)
        .filter(|p| {
            !p.is_empty()
                && !["and", "or", "&", "+", "none", "nothing"]
                    .iter()
                    .any(|word| p.eq_ignore_ascii_case(word))
        })
        .map(clean)
        .collect();
    (!paths.is_empty()).then_some(paths)
}

/// The punctuation around a path in a sentence, other than brackets and a full stop.
const PUNCTUATION: [char; 6] = ['`', '"', '\'', '*', ';', ':'];

/// One word of a `Touches:` line without the sentence around it: quotes and punctuation, a
/// full stop (but not `.` itself, the whole repository), and a bracket only when it has no
/// partner in the word, so `src/app/(auth)/` keeps its brackets. A word wrapped whole, such as
/// `(src/a.rs)`, loses that pair when the inside looks like a path (has a `/` or `.`).
fn token(word: &str) -> &str {
    let mut p = word;
    loop {
        let before = p;
        p = p.trim_matches(PUNCTUATION);
        if p.len() > 1 && p.ends_with('.') && !p.ends_with("..") {
            p = &p[..p.len() - 1];
        }
        let open = p.matches('(').count();
        let close = p.matches(')').count();
        if open > close {
            p = p.strip_prefix('(').unwrap_or(p);
        } else if close > open {
            p = p.strip_suffix(')').unwrap_or(p);
        } else if let Some(inner) = p.strip_prefix('(').and_then(|p| p.strip_suffix(')')) {
            if wraps_whole(p) && inner.contains(['/', '.']) {
                p = inner;
            }
        }
        if p == before {
            return p;
        }
    }
}

/// Whether a word's first `(` is closed only by its last `)`, as in `(src/(a)/b.rs)` but not
/// `(a)/(b)`.
fn wraps_whole(word: &str) -> bool {
    let mut depth = 0usize;
    for (i, c) in word.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return i == word.len() - 1;
                }
            }
            _ => {}
        }
    }
    false
}

/// The path plans use for the whole repository, which overlaps every other.
pub const WHOLE: &str = "./";

/// A path as plans compare it: `/`-separated, no leading `/`, no empty or `.` parts, and a
/// trailing `/` kept for a directory. `.`, and any path with a `..` part (which could leave
/// the directory it names), is the whole repository ([`WHOLE`]).
fn clean(path: &str) -> String {
    let path = path.trim().replace('\\', "/");
    if path.is_empty() {
        return String::new();
    }
    let dir = path.ends_with('/');
    let parts: Vec<&str> = path
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect();
    if parts.is_empty() || parts.contains(&"..") {
        return WHOLE.to_string();
    }
    let mut clean = parts.join("/");
    if dir {
        clean.push('/');
    }
    clean
}

/// Whether two paths overlap: the same file, or one a directory holding the other.
pub fn overlap(a: &str, b: &str) -> bool {
    let whole = |p: &str| p.trim_end_matches('/').trim_end_matches('.').is_empty();
    if whole(a) || whole(b) {
        return true;
    }
    let (a, b) = (a.trim_end_matches('/'), b.trim_end_matches('/'));
    a == b
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

/// Take item `number`'s plan lock, trying for a moment, since a pool probing whether a plan
/// runs holds it for an instant. None when another plan holds it still, or once a stop comes.
fn plan_lock(home: &Home, repo: &str, number: u64) -> Result<Option<crate::home::RunLock>> {
    for _ in 0..20 {
        if let Some(lock) = home.try_plan_lock(repo, number)? {
            return Ok(Some(lock));
        }
        if crate::platform::interrupts() > 0 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    Ok(None)
}

/// Plan item `number` of the repository checked out at `repo`, and record it as
/// `pool.planned`. A plan that fails is recorded as `failed` before the error is returned; one
/// stopped now (a second Ctrl-C, or `stop now` from the pool) records nothing, so whoever stopped
/// it says why.
pub fn plan(home: &Home, repo: &Path, number: u64) -> Result<Plan> {
    let repo = std::path::absolute(repo)?;
    let name = jono::repo_name(home, &repo)?;
    // Taken before anything is recorded: a plan that finds another one running records nothing,
    // so it never replaces that plan's result.
    let Some(_lock) = plan_lock(home, &name, number)? else {
        if crate::platform::interrupts() > 0 {
            bail!("stopped before planning #{number}");
        }
        bail!(
            "another `paja pool plan {number}` of {name} is running (a pool's planner, or one \
             started by hand); wait for it, then `paja pool status` shows its plan"
        );
    };
    // The item's title and text as the plan read them, which a failed plan records too: an edit
    // a sync records while the plan runs then makes even a failed plan one of old text.
    let mut basis = None;
    let made = make(home, &repo, &name, number, &mut basis);
    let plan = match made {
        Ok(plan) => plan,
        Err(err) => {
            let failed = Plan {
                repo: name.clone(),
                number,
                paths: Vec::new(),
                source: "failed".to_string(),
                high_risk: false,
                basis,
                error: Some(format!("{err:#}")),
            };
            // A tampered repository gets no more git or gh, and no event that could hide it.
            if !is_tampered(&err) && crate::platform::interrupts() < 2 {
                let _ = record(home, EV_POOL_PLANNED, serde_json::to_value(&failed)?);
            }
            return Err(err);
        }
    };
    record(home, EV_POOL_PLANNED, serde_json::to_value(&plan)?)?;
    Ok(plan)
}

/// Make the plan, setting `basis` from the item as soon as it is read.
fn make(
    home: &Home,
    repo: &Path,
    name: &str,
    number: u64,
    basis: &mut Option<String>,
) -> Result<Plan> {
    ahjo::ensure_untampered(home, repo)?;
    let item = jono::list(home, name)?
        .into_iter()
        .find(|item| item.number == number)
        .with_context(|| {
            format!("{name} has no work item #{number}; `paja work list` shows the queue")
        })?;
    *basis = Some(text_basis(&item.title, &item.body));
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
            let _sandbox = crate::sandbox::enter(start_sandbox(home, repo, &base)?);
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
        high_risk: judge_high_risk(&paths, &high_risk),
        paths,
        source: source.to_string(),
        basis: basis.clone(),
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
    // The repository's name and hash in the name, as an item's environment has, so two
    // repositories sharing a home never plan their #5 in one checkout.
    let name = jono::env_name_for(&item.repo, &format!("plan-{}", item.number));
    // Releases before planner names carried the repository named it `plan-<n>`. One left by a
    // killed plan of this repository holds nothing, so it goes, with its branch and port block.
    let old = format!("plan-{}", item.number);
    if let Ok(env) = ahjo::get(home, &old) {
        if ahjo::belongs_to(home, &env, repo)? && ahjo::remove(home, &old, true).is_ok() {
            let _ = git(repo, &["branch", "-q", "-D", &format!("paja/{old}")]);
        }
    }
    // A checkout or branch left behind by a plan that was killed is reused; `discard` puts it
    // back on the default branch.
    let branch = format!("refs/heads/paja/{name}");
    let env = match ahjo::get(home, &name) {
        // Any checkout or worktree of the repository counts, as the plan may run from one.
        Ok(env) if !ahjo::belongs_to(home, &env, repo)? => bail!(
            "environment {name} belongs to the repository at {}, not {}; two repositories got \
             the same planner name, so remove it with `paja env remove {name}` if it is stale",
            env.repo.display(),
            repo.display()
        ),
        Ok(env) => env,
        Err(_) => {
            let left = git(repo, &["rev-parse", "--verify", "--quiet", &branch]).is_ok();
            ahjo::create(
                home,
                CreateOptions {
                    name: &name,
                    repo,
                    base: if left { None } else { Some(base) },
                },
            )?
        }
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
        // The plan's branch holds nothing: removed too, so the next plan starts fresh.
        if ahjo::remove(home, &name, true).is_ok() {
            let _ = git(repo, &["branch", "-q", "-D", &format!("paja/{name}")]);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plan_lock_held_for_a_moment_is_waited_for_and_one_held_on_is_not() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home")).unwrap();
        let probe = home.try_plan_lock("o/a", 7).unwrap().unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            drop(probe);
        });
        assert!(plan_lock(&home, "o/a", 7).unwrap().is_some());
        release.join().unwrap();
        let _other = home.try_plan_lock("o/a", 8).unwrap().unwrap();
        assert!(plan_lock(&home, "o/a", 8).unwrap().is_none());
    }

    #[test]
    fn a_planner_checkout_another_repository_holds_is_refused_and_an_old_one_of_ours_goes() {
        let tmp = tempfile::tempdir().unwrap();
        let home = Home::at(tmp.path().join("home")).unwrap();
        let (a, b) = (
            super::super::tests::repo_with_commit(tmp.path(), "a"),
            super::super::tests::repo_with_commit(tmp.path(), "b"),
        );
        let item = super::super::tests::work_item("o/a", 7);
        let name = jono::env_name_for("o/a", "plan-7");
        let theirs = ahjo::create(
            &home,
            CreateOptions {
                name: &name,
                repo: &b,
                base: None,
            },
        )
        .unwrap();
        // An older release's `plan-7` of this repository, left by a killed plan, goes.
        let old = ahjo::create(
            &home,
            CreateOptions {
                name: "plan-7",
                repo: &a,
                base: None,
            },
        )
        .unwrap();
        let models = Models::parse_on("", "main").unwrap();
        let err = format!("{:#}", ask(&home, &a, "HEAD", &item, &models).unwrap_err());
        assert!(err.contains(&format!("paja env remove {name}")), "{err}");
        assert!(theirs.path.exists());
        assert_eq!(ahjo::get(&home, &name).unwrap().repo, theirs.repo);
        assert!(ahjo::get(&home, "plan-7").is_err());
        assert!(!old.path.exists());
        assert!(git(
            &a,
            &["rev-parse", "--verify", "--quiet", "refs/heads/paja/plan-7"]
        )
        .is_err());
    }

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
        // Words and punctuation of a sentence aren't paths.
        assert_eq!(
            touches_line("Touches: src/a.rs and docs/ (maybe src/b.rs)."),
            Some(vec![
                "src/a.rs".into(),
                "docs/".into(),
                "maybe".into(),
                "src/b.rs".into()
            ])
        );
        assert_eq!(touches_line("Touches: none"), None);
        // A bracket that belongs to the path stays; one from the sentence goes.
        assert_eq!(
            touches_line(
                "Touches: src/app/(auth) (auth)/x.tsx (src/a.rs), (src/app/(b)/c.ts). (a)/(b)"
            ),
            Some(vec![
                "src/app/(auth)".into(),
                "(auth)/x.tsx".into(),
                "src/a.rs".into(),
                "src/app/(b)/c.ts".into(),
                "(a)/(b)".into()
            ])
        );
        // A quoted path wrapped in brackets, and a parenthetical that ends a sentence.
        assert_eq!(
            touches_line("Touches: (\"src/a.rs\") (see src/b.rs)."),
            Some(vec!["src/a.rs".into(), "see".into(), "src/b.rs".into()])
        );
        assert_eq!(touches_line("Touches: `.`"), Some(vec![WHOLE.into()]));
    }

    #[test]
    fn a_path_is_cleaned_and_one_leaving_its_folder_is_everything() {
        assert_eq!(clean("./src//a/./b.rs"), "src/a/b.rs");
        assert_eq!(clean("\\docs\\"), "docs/");
        assert_eq!(clean("src/../../etc"), WHOLE);
        assert_eq!(clean("."), WHOLE);
        assert!(overlap(WHOLE, "src/a.rs"));
        assert!(overlap("docs/x.md", WHOLE));
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
            basis: None,
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
