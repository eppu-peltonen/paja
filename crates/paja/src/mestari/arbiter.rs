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
}

/// Files whose change would weaken the checks that judge a change: Vasara's commands, the
/// high-risk paths and the deny-list hook.
const GUARDS: [&str; 3] = [CHECKS_FILE, HIGH_RISK_FILE, "crates/paja/src/hook.rs"];

/// Wordings that say an escalation, or an answer to it, is about weakening a check.
const WEAKENING: [&str; 14] = [
    ".paja/checks",
    ".paja/high-risk",
    "deny-list",
    "hook.rs",
    "weaken",
    "loosen",
    "relax the check",
    "skip the check",
    "skip the test",
    "disable the test",
    "delete the test",
    "remove the test",
    "edit the failing test",
    "edit the test",
];

/// The wordings in [`WEAKENING`] that name a guarded file or the hook rather than an act. A
/// negator never makes them harmless: an answer or escalation that names them goes to the person.
const NAMED_GUARDS: [&str; 4] = [".paja/checks", ".paja/high-risk", "deny-list", "hook.rs"];

/// Words that negate a wording when they come right before it: "it doesn't weaken a check",
/// "don't edit the failing test", "never skip the test".
const NEGATORS: [&str; 7] = [
    "not", "never", "doesn't", "don't", "didn't", "won't", "mustn't",
];

/// Words that, anywhere in a negated wording's sentence, make the negation a doubt, a question
/// or a condition rather than a plain "no": "why don't we skip the test", "no reason not to
/// weaken it", "don't edit the failing test unless the assertion is wrong", "it doesn't weaken
/// the gate much". The sentence then counts as about weakening a check.
const HEDGES: [&str; 26] = [
    "why",
    "how",
    "if",
    "unless",
    "reason",
    "need",
    "should",
    "would",
    "could",
    "shall",
    "except",
    "until",
    "other",
    "but",
    "beyond",
    "when",
    "only",
    "instead",
    "rather",
    "besides",
    "much",
    "significantly",
    "entirely",
    "mostly",
    "partly",
    "really",
];

/// The words that may follow a negated wording to the end of its clause: a plain object, as in
/// "doesn't weaken any check" or "don't edit the failing test". Anything else after it ("don't
/// remove the test entirely", "won't loosen anything beyond the flaky case") keeps it counting.
const PLAIN_OBJECTS: [&str; 26] = [
    "a", "an", "any", "the", "this", "that", "these", "those", "its", "it", "them", "one", "ones",
    "check", "checks", "gate", "gates", "rule", "rules", "test", "tests", "failing", "existing",
    "anything", "here", "either",
];

/// Whether `text` talks about weakening a check. It fails closed: a wording counts unless one
/// of [`NEGATORS`] is the word right before it and the clause's only negator, nothing but a
/// plain object ([`PLAIN_OBJECTS`]) follows it in its clause, its sentence has no hedge
/// ([`HEDGES`]) and doesn't end in `?`, and the wording is an act, not one of [`NAMED_GUARDS`].
/// So a reviewer's "this doesn't weaken any check" or an answer's "don't edit the failing test"
/// is not a request to weaken one, while "don't hesitate to edit the failing test" or "don't
/// skip the test unless it's flaky" still is.
fn names_a_weaker_check(text: &str) -> bool {
    let text = text
        .to_lowercase()
        .replace(['\u{2018}', '\u{2019}', '\u{2bc}'], "'");
    sentences(&text).into_iter().any(|sentence| {
        let hedged =
            sentence.trim_end().ends_with('?') || words(sentence).any(|w| HEDGES.contains(&w));
        clauses(sentence).into_iter().any(|clause| {
            WEAKENING.iter().any(|w| {
                clause.match_indices(w).any(|(at, _)| {
                    hedged
                        || NAMED_GUARDS.contains(w)
                        || !negated(&clause[..at])
                        || !plain_object(&clause[at + w.len()..])
                })
            })
        })
    })
}

/// The words of `text`, without the punctuation around them; a hyphen or apostrophe inside a
/// word ("no-verify", "doesn't") stays.
fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split_whitespace()
        .map(|w| w.trim_matches(|c: char| !(c.is_alphanumeric() || c == '\'')))
        .filter(|w| !w.is_empty())
}

/// Whether what follows a wording to the end of its clause is a plain object. The rest of the
/// matched word ("weaken" in "weakens") is skipped first.
fn plain_object(after: &str) -> bool {
    let after = after.trim_start_matches(|c: char| c.is_alphanumeric());
    words(after).all(|w| PLAIN_OBJECTS.contains(&w))
}

/// `text` cut into sentences, each keeping its closing `?` or `!`: at `!`, `?`, a line break
/// and a `.` not followed by a letter or digit, so `.paja/checks` and `hook.rs` stay whole.
fn sentences(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        let ends = match c {
            '!' | '?' | '\n' => true,
            '.' => chars.peek().is_none_or(|(_, next)| !next.is_alphanumeric()),
            _ => false,
        };
        if ends {
            out.push(&text[start..at + c.len_utf8()]);
            start = at + c.len_utf8();
        }
    }
    out.push(&text[start..]);
    out
}

/// A sentence cut into clauses: at `,`, `;`, `:`, the sentence's closing mark, brackets and
/// dashes. A hyphen inside a word ("no-verify") is no dash, and a dot inside a name
/// (`.paja/checks`) no closing mark.
fn clauses(sentence: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = sentence.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        let ends = match c {
            ',' | ';' | ':' | '!' | '?' | '\n' | '(' | ')' | '[' | ']' | '\u{2013}'
            | '\u{2014}' => true,
            '.' => chars.peek().is_none_or(|(_, next)| !next.is_alphanumeric()),
            '-' => sentence[..at].ends_with(' ') && chars.peek().is_some_and(|(_, n)| *n == ' '),
            _ => false,
        };
        if ends {
            out.push(&sentence[start..at]);
            start = at + c.len_utf8();
        }
    }
    out.push(&sentence[start..]);
    out
}

/// Whether the clause up to a wording (`before`) negates it: its last word is one of
/// [`NEGATORS`], and it is the only negator in the clause so far ("no reason not to" is none).
fn negated(before: &str) -> bool {
    let words: Vec<&str> = words(before).collect();
    let negators = words.iter().filter(|w| is_negator(w)).count();
    words.last().is_some_and(|w| NEGATORS.contains(w)) && negators == 1
}

/// Any word that negates, for counting double negatives: [`NEGATORS`] and the other `n't`s,
/// "no", "nothing" and the like.
fn is_negator(word: &str) -> bool {
    NEGATORS.contains(&word)
        || word.ends_with("n't")
        || [
            "no", "nothing", "none", "nor", "neither", "without", "cannot",
        ]
        .contains(&word)
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
    if names_a_weaker_check(question)
        || names_a_weaker_check(context)
        || names_a_weaker_check(&ruling.answer)
    {
        return Some("the escalation or the answer is about weakening a check".into());
    }
    None
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
         check that passed on re-run). Write the answer as an instruction to the builder.\n\n\
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
            "recommendation": {"type": "string"}
        },
        "required": ["verdict"]
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
        assert!(names_a_weaker_check("Edit .paja/checks to drop clippy"));
        assert!(names_a_weaker_check("Should we Weaken the gate?"));
        assert!(names_a_weaker_check("edit the failing test"));
        assert!(!names_a_weaker_check("Use `Part of` for a bundle issue"));
    }

    #[test]
    fn a_negated_wording_is_not_a_request_to_weaken_a_check() {
        // What reviewers and the arbiter say about a change that only adds tests.
        for text in [
            "It adds two smoke cases and doesn't weaken any check.",
            "This does not weaken the gate.",
            "Add tests; don't edit the failing test.",
            "Reviewer 1 is right. This doesn't weaken any check.",
            "The change doesn\u{2019}t weaken anything.",
            "Never skip the test.",
            "The builder won't loosen a rule here",
        ] {
            assert!(!names_a_weaker_check(text), "{text}");
        }
    }

    #[test]
    fn a_request_to_weaken_a_check_counts_whatever_negators_are_near() {
        // Each of these asks for, or reports, a weaker check: the guard must still catch it.
        for text in [
            // A negator on another verb, or before the wording but not right before it.
            "Don't hesitate to edit the failing test.",
            "The assertion is not needed so delete the test.",
            "It isn't flaky so skip the test.",
            "The builder didn't fix it and weakened clippy.",
            "Without delay edit the failing test.",
            "Doesn't matter.Weaken the gate",
            "Not fixable \u{2014} skip the test.",
            "Not a real bug - loosen the rule.",
            "Not flaky (skip the test)",
            // Double negatives, questions and doubts.
            "There's no reason not to weaken the gate.",
            "Why don't we just skip the test?",
            "Why not skip the test?",
            "Isn't it simpler to weaken the check?",
            "If not, weaken the gate",
            "If you don't edit the test it stays red",
            "Unless it's flaky, don't skip the test, edit the failing test instead.",
            "Should we not skip the test",
            // A condition, an exception or a measure on the negation.
            "Don't edit the failing test unless the assertion itself is wrong.",
            "Don't skip the test unless it's flaky.",
            "Unless it's flaky, don't skip the test.",
            "Never skip the test, except this once.",
            "Don't edit the failing test except to fix the typo.",
            "Do not edit the test other than the snapshot.",
            "Don't skip the test until CI is fixed",
            "This does not weaken the gate, unless you count the flaky case.",
            "This doesn't weaken the gate much.",
            "It does not weaken the check significantly.",
            "It won't loosen anything beyond the flaky case",
            "Don't remove the test entirely; mark it ignored.",
            // Hyphenated words aren't negators.
            "Commit with --no-verify to skip the check",
            // The guarded files count even when negated.
            "Edit the no-unwrap rule in .paja/checks",
            "Change nothing except .paja/checks.",
            "Don't touch anything but hook.rs.",
            "It doesn't touch the deny-list hook.",
            "Change .paja/checks.",
            "Allow it in hook.rs",
        ] {
            assert!(names_a_weaker_check(text), "{text}");
        }
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
