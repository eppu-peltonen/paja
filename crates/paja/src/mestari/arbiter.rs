//! The arbiter: before Mestari escalates, one fresh read-only agent session reads the
//! escalation and either answers it (when the issue, the diff and the repository's rules decide
//! it) or sharpens the question for the person. What it may never settle is enforced here, in
//! code, not only in its prompt.

use super::*;

/// What the arbiter is shown about a run and what it needs to start: the checkout of the item,
/// the default branch's commit its diff is read against, and the models.
pub(super) struct Scene<'a> {
    pub home: &'a Home,
    pub repo: &'a Path,
    pub env: &'a Env,
    pub base: &'a str,
    pub item: &'a WorkItem,
    pub models: &'a Models,
}

/// The arbiter's own verdict, as its schema shapes it.
#[derive(Debug, Deserialize)]
struct Ruling {
    /// `answer` or `escalate`.
    verdict: String,
    /// The decision, when the verdict is `answer`.
    #[serde(default)]
    answer: String,
    /// Why it decided so.
    #[serde(default)]
    reason: String,
    /// The question for the person, when the verdict is `escalate`.
    #[serde(default)]
    question: String,
    /// Two to four options for the person to choose from.
    #[serde(default)]
    options: Vec<String>,
    #[serde(default)]
    recommendation: String,
    /// The arbiter's own flag: whether its answer changes, drops, skips or relaxes a test or a
    /// check. An answer without it is overruled, so a missing flag never reads as "no".
    #[serde(default)]
    weakens_a_check: Option<bool>,
}

/// Files whose change would weaken the checks that judge a change: Vasara's commands, the
/// high-risk paths and the deny-list hook.
const GUARDS: [&str; 3] = [CHECKS_FILE, HIGH_RISK_FILE, "crates/paja/src/hook.rs"];

/// The files and rules that judge a change, named in the question or the answer: a decision
/// about them is a person's. The reviewers' summaries (`context`) are not searched for them,
/// because a review names them only to say what the change is about, and touching them is
/// caught from the diff.
const GUARD_NAMES: [&str; 5] = [
    ".paja/checks",
    ".paja/high-risk",
    "deny-list",
    "hook.rs",
    ".github/workflows",
];

/// Wordings that say an escalation, or an answer to it, is about weakening a check. A word list
/// misses paraphrases, so it is one of several rules: the arbiter's own `weakens_a_check` flag,
/// test files and check commands named in the answer, and the guard files in the diff.
const WEAKENING: [&str; 22] = [
    "weaken",
    "loosen",
    "relax",
    "skip the check",
    "skip the test",
    "disable the test",
    "delete the test",
    "remove the test",
    "edit the failing test",
    "edit the test",
    "drop the assert",
    "remove the assert",
    "delete the assert",
    "drop the check",
    "remove the check",
    "ci config",
    "ci workflow",
    "skip ci",
    "continue-on-error",
    "allow_failure",
    "#[ignore]",
    "xfail",
];

/// Whether `text` talks about weakening a check.
fn names_a_weaker_check(text: &str) -> bool {
    let text = text.to_lowercase();
    WEAKENING.iter().any(|w| text.contains(w))
}

/// The path-like words in `text`: what is left of a word, without quotes, brackets and trailing
/// punctuation, that holds a `/` or a `.` between other characters.
fn paths_in(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| c.is_whitespace() || "`'\"()[]{}<>,;".contains(c))
        .map(|w| w.trim_end_matches(['.', ':', '!', '?']))
        .filter(|w| w.contains('/') || w.trim_start_matches('.').contains('.') && !w.ends_with('.'))
}

/// Whether `path` looks like a test: a `test`, `tests`, `spec` or `__tests__` folder, or a file
/// named for one (`test_x.py`, `x_test.go`, `x.test.ts`, `x.spec.js`).
fn is_test_path(path: &str) -> bool {
    let path = path.to_lowercase().replace('\\', "/");
    let mut parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    let Some(file) = parts.pop() else {
        return false;
    };
    let stem = file.split('.').next().unwrap_or(file);
    parts
        .iter()
        .any(|p| matches!(*p, "test" | "tests" | "spec" | "specs" | "__tests__"))
        || matches!(stem, "test" | "tests" | "spec")
        || stem.starts_with("test_")
        || stem.ends_with("_test")
        || stem.ends_with("_tests")
        || stem.ends_with("_spec")
        || file.contains(".test.")
        || file.contains(".spec.")
}

/// The guard file or rule `text` names, if any.
fn names_a_guard(text: &str) -> Option<&'static str> {
    let text = text.to_lowercase();
    GUARD_NAMES.iter().copied().find(|g| text.contains(g))
}

/// What in an answer would have a person decide: a test file, or one of Vasara's check commands
/// (a line of `.paja/checks` with arguments, matched as whole words).
fn answer_names_a_check(answer: &str, checks: &[String]) -> Option<String> {
    let lower = answer.to_lowercase();
    if let Some(path) = paths_in(answer).find(|p| is_test_path(p)) {
        return Some(format!("the answer names the test file {path}"));
    }
    checks
        .iter()
        .map(|c| c.trim())
        .filter(|c| c.contains(char::is_whitespace))
        .find(|c| contains_words(&lower, &c.to_lowercase()))
        .map(|c| format!("the answer names the check `{c}`"))
}

/// Whether `needle` is in `text` with no letter, digit or `_` right before or after it.
fn contains_words(text: &str, needle: &str) -> bool {
    let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    text.match_indices(needle).any(|(at, _)| {
        !word(text[..at].chars().next_back()) && !word(text[at + needle.len()..].chars().next())
    })
}

/// Why Mestari won't let the arbiter answer this escalation, if it won't.
fn overruled(scene: &Scene, question: &str, context: &str, ruling: &Ruling) -> Option<String> {
    if scene.item.answers.iter().any(jono::Answer::by_arbiter) {
        return Some(
            "the arbiter already answered this item once, and it answers at most once".into(),
        );
    }
    if ruling.answer.trim().is_empty() {
        return Some("the arbiter gave no answer".into());
    }
    if let Ok(names) = git(
        &scene.env.path,
        &[
            "diff",
            "--name-only",
            "--no-renames",
            "-z",
            &format!("{}...HEAD", scene.base),
        ],
    ) {
        if let Some(path) = names
            .split('\0')
            .find(|p| GUARDS.iter().any(|g| p.eq_ignore_ascii_case(g)))
        {
            return Some(format!(
                "the change touches {path}, so a person decides anything about it"
            ));
        }
    }
    if ruling.weakens_a_check != Some(false) {
        return Some(
            "the arbiter didn't say its answer leaves every test and check as it is".into(),
        );
    }
    if names_a_weaker_check(question)
        || names_a_weaker_check(context)
        || names_a_weaker_check(&ruling.answer)
    {
        return Some("the escalation or the answer is about weakening a check".into());
    }
    if let Some(name) = names_a_guard(question).or_else(|| names_a_guard(&ruling.answer)) {
        return Some(format!(
            "the escalation or the answer is about weakening a check (it names {name})"
        ));
    }
    let checks = read_from_base(scene.repo, scene.base, CHECKS_FILE)
        .ok()
        .flatten()
        .map(|text| lines(&text))
        .unwrap_or_default();
    answer_names_a_check(&ruling.answer, &checks)
        .map(|why| format!("{why}, so it may weaken a check"))
}

/// What the arbiter is asked.
fn arbiter_prompt(scene: &Scene, question: &str, context: &str, stat: &str) -> String {
    let item = scene.item;
    let context = if context.trim().is_empty() {
        String::new()
    } else {
        format!("\nWhat led to it:\n\n{context}\n")
    };
    format!(
        "Issue #{number}: {title}\n\n{body}\n{answers}\n\
         Paja is about to ask the person this:\n\n{question}\n{context}\n\
         Diff stat of the change so far (`git diff {base}...HEAD`):\n\n{stat}\n\n\
         You are Paja's arbiter. You did not write this change. Decide whether the question is \
         one an agent can settle from the issue, the diff and the repository's rules (the \
         CLAUDE.md in your system prompt is the default branch's copy). You are read-only: you \
         can't change files, merge or write to GitHub, and Bash is limited to read-only local \
         git.\n\n\
         Verdict `answer` with the decision in `answer` and your reasons in `reason`, only for: \
         a reviewer split where the blocker can be checked against the diff and the issue; a \
         scope or wording call the repository's rules already decide (such as `Part of` versus \
         `Closes` for a bundle issue); a builder question the issue, the code or CLAUDE.md \
         answers; or one more round after a failure that can be retried (a turn limit, a flaky \
         check that passed on re-run). Write the answer as an instruction to the builder. Set \
         `weakens_a_check` to true when the answer would change, drop, skip or relax any test, \
         assertion, check command, CI configuration or the hook's rules, in whatever words, and \
         to false only when it leaves every one as it is.\n\n\
         Verdict `escalate` for everything else, and always for: anything that changes the \
         issue's intent or scope beyond what it says; anything that would weaken a check \
         (editing the failing test, `.paja/checks`, `.paja/high-risk` or the hook's rules); \
         secrets or credentials; actions outside the repository; a question that names the \
         person or asks for their preference. With `escalate`, give the person a better \
         question: `question` (what is undecided, in plain words), two to four `options`, and \
         your `recommendation`, so they can answer in one line. Never copy secrets or run \
         output into any of it, since Paja may post it on the public issue.\n",
        number = item.number,
        title = item.title,
        body = item.body,
        answers = answers(item),
        base = scene.base,
    )
}

/// The question the person is asked when the arbiter sharpened it: its question, the options
/// and the recommendation. None when it gave no question, so the raw one stands.
fn sharpened(ruling: &Ruling) -> Option<String> {
    if ruling.question.trim().is_empty() {
        return None;
    }
    let mut text = format!(
        "Paja's arbiter looked at this first and couldn't settle it.\n\n{}",
        ruling.question.trim()
    );
    let options: Vec<&str> = ruling
        .options
        .iter()
        .map(|o| o.trim())
        .filter(|o| !o.is_empty())
        .take(4)
        .collect();
    if !options.is_empty() {
        text.push_str("\n\nOptions:");
        for (i, option) in options.iter().enumerate() {
            text.push_str(&format!("\n{}. {option}", i + 1));
        }
    }
    if !ruling.recommendation.trim().is_empty() {
        text.push_str(&format!(
            "\n\nRecommendation: {}",
            ruling.recommendation.trim()
        ));
    }
    Some(text)
}

/// Run the arbiter on an escalation. Err only for what must stop the run (tampering, Ctrl-C);
/// an agent that fails or gives no verdict is no reason to hold the person's question back.
fn consult(
    scene: &Scene,
    question: &str,
    context: &str,
    round: u32,
) -> Result<Option<(Option<Ruling>, StepUsage)>> {
    let stat = git(
        &scene.env.path,
        &[
            "diff",
            "--stat",
            "--no-ext-diff",
            &format!("{}...HEAD", scene.base),
        ],
    )
    .unwrap_or_default();
    let prompt = arbiter_prompt(scene, question, context, &stat);
    let schema = json!({
        "type": "object",
        "properties": {
            "verdict": {"type": "string", "enum": ["answer", "escalate"]},
            "answer": {"type": "string"},
            "reason": {"type": "string"},
            "question": {"type": "string"},
            "options": {"type": "array", "items": {"type": "string"}},
            "recommendation": {"type": "string"},
            "weakens_a_check": {"type": "boolean"}
        },
        "required": ["verdict", "weakens_a_check"]
    });
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
    record_step(
        scene.home,
        scene.item,
        round,
        "arbiter",
        Some(scene.models),
        None,
    )?;
    let out = claude(
        scene.home,
        scene.env,
        scene.base,
        &prompt,
        &tools,
        Some(&schema),
        true,
        &scene.models.args("arbiter"),
    );
    let out = match out {
        Ok(out) => out,
        Err(err) if is_tampered(&err) => return Err(err),
        Err(err) => {
            stop_point()?;
            progress(&format!(
                "the arbiter couldn't run ({err:#}); asking the person directly"
            ));
            return Ok(None);
        }
    };
    // Its spend counts as the `arbiter` role in `paja usage`, kept in `run.arbitrated`.
    let used = StepUsage::from_result("arbiter", None, scene.models, "arbiter", &out);
    let ruling = out
        .get("structured_output")
        .cloned()
        .and_then(|v| serde_json::from_value::<Ruling>(v).ok())
        .filter(|r| matches!(r.verdict.as_str(), "answer" | "escalate"));
    if ruling.is_none() {
        progress("the arbiter gave no usable verdict; asking the person directly");
    }
    Ok(Some((ruling, used)))
}

/// Settle an escalation of the item, or take it to the person. `question` is what the person
/// would be asked; `context` is what led to it (the reviewers' verdicts, say).
pub(super) fn settle(
    scene: &Scene,
    question: &str,
    context: &str,
    round: u32,
) -> Result<RunReport> {
    let Scene {
        home,
        repo,
        env,
        item,
        models,
        ..
    } = *scene;
    progress("the arbiter is reading the escalation");
    let Some((ruling, used)) = consult(scene, question, context, round)? else {
        // The arbiter didn't run: nothing to record, the person is asked as before.
        return escalated(home, repo, env, item, question, round);
    };
    let model = models.chosen("arbiter").map(|m| m.model.clone());
    let event = |verdict: &str, extra: Value| {
        let mut data = json!({
            "repo": item.repo, "number": item.number, "round": round,
            "verdict": verdict, "model": model, "steps": [used],
        });
        if let (Some(data), Some(extra)) = (data.as_object_mut(), extra.as_object()) {
            data.extend(extra.clone());
        }
        record(home, EV_ARBITRATED, data)
    };
    let Some(ruling) = ruling else {
        event("escalate", json!({ "question": clip(question) }))?;
        return escalated(home, repo, env, item, question, round);
    };
    let refusal = if ruling.verdict == "answer" {
        overruled(scene, question, context, &ruling)
    } else {
        None
    };
    if ruling.verdict == "answer" && refusal.is_none() {
        let answer = ruling.answer.trim().to_string();
        event(
            "answer",
            json!({ "answer": answer, "reason": clip(&ruling.reason) }),
        )?;
        jono::answer_by_arbiter(home, &item.repo, item.number, question, &answer)?;
        // The branch keeps the work; the worktree and its ports are freed, as for an escalation.
        let _ = ahjo::remove(home, &env.name, true);
        let said = comment_answer(home, repo, item, question, &answer, &ruling.reason);
        progress(&match said {
            Ok(()) => format!(
                "the arbiter answered #{}: {}; it is queued again, and `paja work answer {} \
                 \"<your answer>\"` overrides the answer",
                item.number,
                clip(&answer),
                item.number
            ),
            Err(err) => format!(
                "the arbiter answered #{} and it is queued again, but the comment on the issue \
                 failed: {err:#}",
                item.number
            ),
        });
        return Ok(RunReport::Arbitrated {
            number: item.number,
            answer,
            rounds: round,
        });
    }
    // To the person: the arbiter's sharper question when it has one, else the raw one (with
    // the reason when Mestari overruled an answer).
    let asked = match (&refusal, sharpened(&ruling)) {
        (None, Some(sharper)) => sharper,
        (None, None) => question.to_string(),
        (Some(why), sharper) => {
            progress(&format!("the arbiter's answer is overruled: {why}"));
            sharper.unwrap_or_else(|| {
                format!(
                    "{question}\n\n(Paja's arbiter looked at this first and wanted to answer it, \
                     but a person has to: {why}.)"
                )
            })
        }
    };
    event(
        "escalate",
        json!({
            "question": clip(&asked),
            "overruled": refusal,
        }),
    )?;
    escalated(home, repo, env, item, &asked, round)
}

/// Say on the issue what the arbiter decided and why, and how to override it. The text can quote
/// CI logs or review text, so it is redacted first: the issue is public.
fn comment_answer(
    home: &Home,
    repo: &Path,
    item: &WorkItem,
    question: &str,
    answer: &str,
    reason: &str,
) -> Result<()> {
    let public = |text: &str| {
        follow_ups::redact_for_comment(text, repo, home.root(), follow_ups::QUOTED_CHARS)
    };
    let (answer, reason) = (public(answer), public(reason));
    let why = if reason.trim().is_empty() {
        String::new()
    } else {
        format!("\n\nWhy: {}", reason.trim())
    };
    let body = format!(
        "**Paja's arbiter answered this instead of asking you.**\n\nThe question was:\n\n{}\n\n\
         Decided: {answer}{why}\n\nThe item is queued again with that answer. To decide \
         otherwise before the next run, answer with `paja work answer {n} \"<your answer>\"`.",
        public(question),
        n = item.number
    );
    follow_ups::comment_on_issue(home, repo, item.number, &body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ruling(question: &str, options: &[&str], recommendation: &str) -> Ruling {
        Ruling {
            verdict: "escalate".into(),
            answer: String::new(),
            reason: String::new(),
            question: question.into(),
            options: options.iter().map(|o| o.to_string()).collect(),
            recommendation: recommendation.into(),
            weakens_a_check: None,
        }
    }

    #[test]
    fn the_arbiter_runs_on_the_review_model_unless_it_has_a_line() {
        let models = Models::parse("review opus high\n").unwrap();
        assert_eq!(models.chosen("arbiter"), models.roles.get("review"));
        assert!(models.args("arbiter").contains(&"opus".to_string()));
        let own = Models::parse("review opus\narbiter sonnet\n").unwrap();
        assert_eq!(own.chosen("arbiter").unwrap().model, "sonnet");
        assert_eq!(Models::default().chosen("arbiter"), None);
    }

    #[test]
    fn weakening_wordings_are_found_in_any_case() {
        assert!(names_a_guard("Edit .paja/checks to drop clippy").is_some());
        assert!(names_a_guard("Change the rule in crates/paja/src/hook.rs").is_some());
        assert!(names_a_guard("Edit .github/workflows/ci.yml").is_some());
        assert!(names_a_weaker_check("Should we Weaken the gate?"));
        assert!(names_a_weaker_check("edit the failing test"));
        assert!(!names_a_weaker_check("Use `Part of` for a bundle issue"));
        // Paraphrases the old list let through.
        assert!(names_a_weaker_check("Relax the assertion so it passes"));
        assert!(names_a_weaker_check("Drop that line from the CI config"));
        assert!(names_a_weaker_check("Drop the assert_eq on line 40"));
        // A file name alone is not wording: a review may name the hook only to say what changed.
        assert!(!names_a_weaker_check(
            "The change touches hook.rs and adds a rule"
        ));
    }

    #[test]
    fn an_answer_naming_a_guard_a_test_file_or_a_check_command_is_found() {
        let checks = vec!["cargo test".to_string(), "true".to_string()];
        let found = |a: &str| answer_names_a_check(a, &checks);
        for answer in [
            "Change the expected value in tests/smoke.rs.",
            "Update `src/parser_test.go`",
            "see (web/app.test.ts)",
            "fix spec/models/user_spec.rb, then go on",
            "Rewrite test_api.py",
            r"Edit tests\unit.rs",
        ] {
            let why = found(answer).unwrap_or_default();
            assert!(why.contains("test file"), "{answer}: {why}");
        }
        let why = found("Run `cargo test --release` instead").unwrap_or_default();
        assert!(why.contains("`cargo test`"), "{why}");
        // Whole words only, and one-word checks are left to the other rules.
        assert_eq!(found("Use cargo testing tools"), None);
        assert_eq!(found("Use v2; it is true that the issue names it."), None);
        assert_eq!(found("Rename src/latest.rs and bump to v2.1."), None);
        assert_eq!(found("Use `Part of` for a bundle issue."), None);
    }

    #[test]
    fn a_sharper_question_lists_at_most_four_options_and_a_recommendation() {
        let r = ruling("Which?", &["a", " ", "b", "c", "d", "e"], "b");
        let text = sharpened(&r).unwrap();
        assert!(text.contains("Which?"), "{text}");
        assert!(text.contains("1. a\n2. b\n3. c\n4. d"), "{text}");
        assert!(!text.contains("e\n") && !text.contains("5."), "{text}");
        assert!(text.contains("Recommendation: b"), "{text}");
        assert_eq!(sharpened(&ruling("  ", &["a"], "a")), None);
    }
}
