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

/// Who may be the subject of a sentence that says a change weakens no check.
const SUBJECTS: [&[&str]; 9] = [
    &["it"],
    &["this"],
    &["this", "change"],
    &["the", "change"],
    &["this", "fix"],
    &["the", "fix"],
    &["the", "diff"],
    &["this", "diff"],
    &["the", "commit"],
];

/// How such a sentence says "does not".
const DENIALS: [&[&str]; 7] = [
    &["doesn't"],
    &["does", "not"],
    &["didn't"],
    &["did", "not"],
    &["won't"],
    &["will", "not"],
    &["never"],
];

/// The verbs such a sentence denies.
const DENIED: [&str; 4] = ["weaken", "weakens", "loosen", "loosens"];

/// What may follow the verb, to the end of the sentence.
const DENIED_OBJECTS: [&[&str]; 12] = [
    &[],
    &["anything"],
    &["any", "check"],
    &["any", "checks"],
    &["a", "check"],
    &["the", "check"],
    &["the", "checks"],
    &["any", "test"],
    &["any", "rule"],
    &["any", "gate"],
    &["the", "gate"],
    &["any", "of", "the", "checks"],
];

/// Whether text a reviewer or the builder wrote (the escalation's question, or what led to it)
/// talks about weakening a check: whether it has any of [`WEAKENING`] once a closing sentence
/// that only says the change weakens no check ([`denies_weakening`]) is taken out. Reviewers
/// are asked whether a change would weaken a check, so they often end with "It doesn't weaken
/// any check."; only the last sentence of the text, or of one reviewer's part of it, is taken
/// out, so a following "Except clippy, which it drops." still counts. Anything else, a negated
/// instruction ("don't skip the test") included, counts. The arbiter's own answer is held to
/// [`mentions_weakening`].
fn names_a_weaker_check(text: &str) -> bool {
    let text = text
        .to_lowercase()
        .replace(['\u{2018}', '\u{2019}', '\u{2bc}'], "'");
    let sentences = sentences(&text);
    let kept: Vec<&str> = sentences
        .iter()
        .enumerate()
        .filter(|&(i, s)| {
            let closing = sentences
                .get(i + 1)
                .is_none_or(|next| reviewer_prefix(next.trim_start()).is_some());
            !(closing && denies_weakening(s))
        })
        .map(|(_, s)| *s)
        .collect();
    mentions_weakening(&kept.join(" "))
}

/// Whether the arbiter's answer names weakening a check at all: any of [`WEAKENING`], said in
/// any way. The arbiter writes the answer the guard judges, so it gets no leniency; its prompt
/// tells it not to say what an answer doesn't weaken.
fn mentions_weakening(text: &str) -> bool {
    let text = text.to_lowercase();
    WEAKENING.iter().any(|w| text.contains(w))
}

/// Whether `sentence` is, word for word, a statement that the change weakens no check: one of
/// [`SUBJECTS`], one of [`DENIALS`], one of [`DENIED`] and one of [`DENIED_OBJECTS`], nothing
/// else, and no mark but apostrophes and a closing `.` or `!`. A reviewer's "Reviewer 1: land
/// — " in front is allowed. A question, a comma, a colon or any other word ("apart from",
/// "just", "unless") makes it count.
fn denies_weakening(sentence: &str) -> bool {
    let sentence = sentence.trim();
    let sentence = reviewer_prefix(sentence).unwrap_or(sentence).trim();
    let body = sentence
        .strip_suffix(['.', '!'])
        .unwrap_or(sentence)
        .trim_end();
    if !body
        .chars()
        .all(|c| c.is_alphabetic() || c == ' ' || c == '\'')
    {
        return false;
    }
    let words: Vec<&str> = body.split_whitespace().collect();
    let after = |words: &'static [&'static str], rest: &[&str]| -> Option<usize> {
        rest.starts_with(words).then_some(words.len())
    };
    SUBJECTS.iter().any(|subject| {
        let Some(n) = after(subject, &words) else {
            return false;
        };
        let rest = &words[n..];
        DENIALS.iter().any(|denial| {
            let Some(m) = after(denial, rest) else {
                return false;
            };
            let rest = &rest[m..];
            rest.first().is_some_and(|v| DENIED.contains(v))
                && DENIED_OBJECTS.iter().any(|object| &rest[1..] == *object)
        })
    })
}

/// What follows the "Reviewer N: verdict — " that `summaries()` in `mod.rs` puts before each
/// review, if `text` starts with one.
fn reviewer_prefix(text: &str) -> Option<&str> {
    let rest = text.strip_prefix("reviewer ")?;
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    let rest = rest[digits..].strip_prefix(": ").filter(|_| digits > 0)?;
    ["land", "changes", "escalate"]
        .iter()
        .find_map(|verdict| rest.strip_prefix(verdict)?.strip_prefix(" \u{2014} "))
}

/// `text` cut into sentences, each keeping its closing mark: after a `.`, `!` or `?` that is
/// followed by whitespace or the end. A line break alone doesn't end one, nor does a dot in a
/// run of dots ("...") or one closing an abbreviation with dots in it ("e.g."), so a hedge on
/// the next line stays in the sentence it hedges.
fn sentences(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        let followed = chars.peek().is_none_or(|(_, next)| next.is_whitespace());
        let ends = followed
            && match c {
                '!' | '?' => true,
                '.' => {
                    // The word the dot closes: no dot inside it, and more than one letter.
                    let word = text[start..at].split_whitespace().last().unwrap_or("");
                    !word.contains('.') && word.chars().count() > 1
                }
                _ => false,
            };
        if ends {
            out.push(&text[start..at + c.len_utf8()]);
            start = at + c.len_utf8();
        }
    }
    if !text[start..].trim().is_empty() {
        out.push(&text[start..]);
    }
    out
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
        || mentions_weakening(&ruling.answer)
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
         check that passed on re-run). Write the answer as an instruction to the builder, and \
         leave out what it doesn't change: an answer that names weakening a check in any way, \
         even to say it doesn't, goes to the person.\n\n\
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
    fn a_sentence_that_only_says_no_check_is_weakened_does_not_count() {
        for text in [
            "Adds two smoke cases. It doesn't weaken any check.",
            "This change does not weaken the gate.",
            "The fix doesn\u{2019}t weaken anything",
            "Reviewer 1: land \u{2014} It doesn't weaken any check.",
            "Reviewer 2: changes \u{2014} Two tests are missing. The diff never weakens a check.\nReviewer 1: land \u{2014} Fine.",
        ] {
            assert!(!names_a_weaker_check(text), "{text}");
        }
    }

    #[test]
    fn anything_else_that_names_weakening_a_check_counts() {
        // Each of these asks for, permits, hedges or reports a weaker check, or says what isn't
        // weakened in more words than a plain denial: all go to the person.
        for text in [
            "Don't skip the test.",
            "Add tests; don't edit the failing test.",
            "Never skip the test.",
            "Don't hesitate to edit the failing test.",
            "Don't skip the test, disable it.",
            "Don't skip the test. Change its expected value to 3.",
            "It doesn't weaken the check, apart from the flaky case.",
            "It does not weaken the check, though it narrows it.",
            "Doesn't weaken the gate, just narrows it",
            "It doesn't weaken the gate much.",
            "This doesn't weaken any check unless you count the flaky case.",
            "Doesn't it weaken the check?",
            "It doesn't weaken any check?",
            "Why not skip the test?",
            "There's no reason not to weaken the gate.",
            "The builder didn't fix it and weakened clippy.",
            "It doesn't weaken any check. Edit .paja/checks to drop clippy.",
            "It doesn't touch hook.rs.",
            "Change nothing except .paja/checks.",
            "It doesn't weaken the deny-list.",
            // A hedge after a line break, a mark or in the next sentence.
            "It doesn't weaken the check,\napart from the flaky case.",
            "It doesn't weaken the check\nexcept the flaky case is now skipped.",
            "It doesn't weaken any check:\nonly the flaky test is now ignored.",
            "It doesn't weaken any check;\nit only ignores the flaky test.",
            "It doesn't weaken any check (\nyet).",
            "It doesn't weaken the check... for now.",
            "Drop clippy from CI, e.g. it won't loosen anything.",
            "It doesn't weaken any check. Except clippy, which it drops.",
            "It doesn't weaken any check. Yet.",
            "It doesn't weaken any check\u{ff1f}",
            "Reviewer 1 weaken it doesn't weaken anything \u{2014}",
            "Reviewer 1: hook.rs \u{2014} It doesn't weaken any check.",
            "Reviewer 1: .paja/checks \u{2014} It doesn't weaken any check.",
            "Reviewer 1: deny-list \u{2014} it doesn't weaken any check.",
            "It doesn't weaken any check\u{203d}",
        ] {
            assert!(names_a_weaker_check(text), "{text}");
        }
    }

    #[test]
    fn the_arbiters_answer_counts_any_mention_of_weakening() {
        assert!(mentions_weakening(
            "One more round. This doesn't weaken any check."
        ));
        assert!(mentions_weakening("Don't edit the failing test."));
        assert!(!mentions_weakening(
            "One more round: add the missing smoke test."
        ));
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
