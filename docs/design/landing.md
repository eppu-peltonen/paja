# How changes land

No human reviews pull requests. People set intent, approve specs and answer escalations; agents review and land code.

## Gates

| Tier | What puts a change here | Gate before landing |
| --- | --- | --- |
| Standard | Everything not listed below | Green CI, Vasara's checks, and one independent reviewer agent that did not write the change |
| High risk | Sandbox and secrets handling, the deny-list hook, landing and revert logic, Vasara's rules, the harness evals, the event log's format; in target projects also auth, money, data migrations, public APIs and security | Green CI, Vasara's checks, two independent reviewer agents who must agree, and the full eval suite |

Automation sets the tier from the files and areas a change touches. A tier earns more autonomy only while its revert
rate stays low. Every landed change is re-verified on `main` and reverted automatically if it turns `main` red.

## Review rounds

Review has to converge, or a fresh reviewer finding one more edge case each time keeps a change open forever.

- **At most five rounds.** A change gets at most five build, check and review rounds, since every finding takes a
  round, not only blockers. After the fifth it lands only if the reviewers say `land`; otherwise it is escalated. A
  round that fails on a turn limit (the builder's, or a reviewer's after its one focused retry) counts against the
  five without a verdict. Builder and reviewer limit hits are counted separately: a second round failed by the same
  kind of step escalates.
- **Every finding in the change is fixed in it.** A reviewer judges the change against its item. It answers `changes`
  for a blocker (a bug in what the issue asks for, a security hole the change opens, a broken `CLAUDE.md` rule, or
  missing tests for the new behaviour) and lists every other finding beside its verdict, marked `bug` (a wrong result
  in the change), `risk` (a security or data-loss risk, or a missing test for behaviour that could break), `nit`
  (style, naming, comment wording, a small refactor) or `outside` (a real bug in code the change doesn't touch);
  anything that is none of these is left out, and so is a nit not worth a round. Every finding except `outside` goes
  back to the builder as the next round's feedback, even when the verdict is `land`; the change lands when a round
  has nothing left to fix, CI is green and, for a high-risk change, both reviewers agree. A re-review reads the fix
  and checks the earlier findings are settled (high risk: the whole change). Mestari files no follow-up issue.
- **At the round limit.** An open blocker escalates, as before. Any other finding still open lands with the change,
  recorded as `run.follow_up` (`not filed: round limit`) and listed in the pull request's description, and Posti tells
  the person (`left_findings`), since no issue holds them. An `outside` finding is not fixed here, since one concern
  goes in each pull request: it is recorded (`not filed: outside the change`) and listed in the pull request the same
  way. Each re-reviewer checks the earlier findings in the change as fixes, and sees the earlier `outside` findings
  (also after a red CI) and repeats the ones that still apply. Every review that reads the whole change after an
  earlier one sees them too, such as a high-risk one, one of a fix that merged or rewrote history, and the one after a
  conflict or failed checks at the turn to land. The log keeps them for a resumed run: a round recorded with no
  `prior` as `next.noted`, and a round whose review approved as `noted` (empty when none). A red CI at the round limit
  escalates, and what the change would have landed without is recorded as `not filed: the change did not land`. The
  description goes to `gh` in a file, since ten findings are past Windows' command-line limit, and is cut at GitHub's
  65,536 characters; a reused pull request gets the new round's description (if that edit fails, the landing goes on
  with the old one).
- **The follow-up label stays.** Jono still takes the follow-up issues that are already open, oldest first, and `paja
  triage` still closes duplicates and already-fixed ones. New ones are made only by people. The pull request's
  description is public, so reviewers never copy secrets, environment details or run output into findings, and Mestari
  checks for itself too: before it writes them, it replaces values that start like a known credential (GitHub, GitLab,
  Anthropic and other `sk-` keys, AWS, Slack, npm, PyPI, JSON Web Tokens) or follow `Bearer`, `token`, `password`,
  `secret` or `api_key` (past quotes, backticks, `:`, `=` or `,`, and past a code fence's info string, so the value on
  the fence's next line goes; an info string that isn't a plain language name goes too) with `<redacted>`, the whole
  value including any `.`, `+`, `/` or `=`, and the factory home, the repository and the user's home directory (also as
  `~/...`) with `<factory home>`, `<repository>` and `<home>`, where no letter or digit follows the path. Windows paths
  match in any letter case.
- **A re-review judges the fix.** After a round of changes, the next reviewer gets the earlier blockers and reads only
  the diff since the commit that was reviewed, not the whole change again. A round that commits no fix goes back to the
  builder without a review. High-risk changes are the exception: both
  reviewers read the whole change every round, since they must agree on all of it.
- **The reviewer gets the diff in its prompt.** Mestari computes `git diff --stat` and the diff for the scope the
  reviewer judges (`base...HEAD`, or `reviewed..HEAD` for a re-review) at the checked commit, so the reviewer spends no
  turns running git to find the change. Above 60 000 characters the prompt holds the stat and whole files up to the
  cap, and names the files the reviewer must read itself. The issue, answers and diff come first and the role-specific
  text last, so a second reviewer on the same model reuses the cached prefix. Main's `CLAUDE.md` is already in the
  reviewer's system prompt, so the prompt doesn't ask for it again. The reviewer's tools, hook and verdict schema are
  unchanged.
- **Cheaper models where the review reads less.** The first reviewer of a round runs as the `review` role in
  `.paja/models` (Opus here). A normal-risk re-review, which judges only the fix against a fixed list of blockers, runs
  as `re-review`, and the second high-risk reviewer, a second opinion on a change the first also reads in full, runs as
  `review-2` (both Sonnet at medium effort here). A second reviewer on a different model also tends to disagree for
  different reasons than a copy of the first would.
- **Each review is a fresh, short session.** Reviewers never share context with the builder or with each other, which
  keeps them independent and keeps each session small.
- **One change lands at a time, on the latest default branch.** A change the reviewers approved waits for the
  repository's turn to land, merges in whatever landed meanwhile, and lands only if Vasara's checks pass again on
  that merge. A conflict, or checks that fail there, is a round for the builder like any other.

## Escalate, don't wait

When Mestari would escalate an item (a builder's question, a reviewer's `escalate`, a high-risk split, a third failed
round, a second turn-limit hit), it first starts one fresh read-only session of the `arbiter` role in `.paja/models`
(the `review` model when there is no line). The arbiter runs under the reviewers' hook and tools, so it changes no
file and writes nothing to GitHub. Its prompt holds the issue, the escalation, what led to it (the reviewers'
verdicts), the diff stat, earlier answers and main's `CLAUDE.md`, and it returns `answer` or `escalate`.

- **`answer`** is for a reviewer split whose blocker the diff and the issue settle, a scope or wording call the
  repository's rules already decide (`Part of` versus `Closes` for a bundle issue), a builder question the issue, the
  code or `CLAUDE.md` answers, or one more round after a failure that can be retried. Mestari records the text as the
  item's answer, marked `by: arbiter`, queues the item again and comments once on the issue with what was decided and
  why. Posti sends nothing and the issue gets no needs-answer label. `paja work show` and Paja Desk say `answered by
  arbiter`, and `paja work answer N "..."` overrides the answer until the item runs again; the later answer comes last
  in the agents' prompts.
- **`escalate`** goes to the person as before, but the arbiter's question replaces the raw one: what is undecided,
  two to four options and a recommendation, so one line answers it.
- **Always escalated, enforced in code:** an answer is overruled to `escalate` when the item already has an arbiter
  answer (at most one per item, so it can't loop), when the change touches `.paja/checks`, `.paja/high-risk` or the
  deny-list hook, or when the escalation or the answer is about weakening a check (editing the failing test, those
  files, the hook's rules). The person then gets the raw question and the reason. The wording check fails closed: a
  wording is let go only when a negator ("not", "doesn't", "don't", "never", ...) is the word right before it and the
  clause's only negator, nothing but a plain object follows it in its clause ("any check", "the failing test"), and
  its sentence neither asks nor hedges ("why", "should", "unless", "except", "only", "much", a closing `?`). So "this
  doesn't weaken any check" is not a request to weaken one, while "don't hesitate to edit the failing test" and
  "don't skip the test unless it's flaky" are. A mention of `.paja/checks`, `.paja/high-risk`, `hook.rs` or the
  deny-list always counts, negated or not. The prompt also tells the arbiter to escalate anything that changes the
  issue's intent or scope, secrets, actions outside the repository and any question that asks for the person's
  preference.
- An arbiter that can't run or gives no usable verdict leaves the escalation as it was. Escalations that are not
  decisions (a usage limit, an error Mestari can't fix, landing, CI and merge problems) skip the arbiter.

Each pass is recorded as `run.arbitrated` (`number`, `verdict`, `answer` or `question`, `model`, `overruled` when
Mestari overruled an answer, and the arbiter's step under `steps`, which `paja usage` counts as its own role).

## When a person is asked

A person hears about a chunk only through Posti, after the arbiter has had its look, and only when:

- reviewer agents disagree, or a check can't reach a verdict, and the arbiter can't settle it;
- a fix loop hits its cap of three rounds and a rethink didn't help;
- a change would weaken a check (loosen a rule, drop a scenario, lower the eval pass rate). That is a question about
  intent, not code, so it is asked as one, with options and a recommendation.

## Rollout

The high-risk gate first runs in observe mode: it records what it would land, and an independent audit checks those
verdicts. It starts landing for real once its verdicts match the audit. Until Paja's own reviewer runs (build step 3),
Claude reviews and lands the hand-built seed pull requests under these same rules.
