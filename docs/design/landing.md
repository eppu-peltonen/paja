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

- **At most three rounds.** A change gets at most three build, check and review rounds. After the third it lands only if
  the reviewers say `land`; otherwise it is escalated. A round that fails on a turn limit (the builder's, or a
  reviewer's after its one focused retry) counts against the three without a verdict. Builder and reviewer limit hits
  are counted separately: a second round failed by the same kind of step escalates.
- **Only blockers hold a change up.** A blocker is a bug in what the issue asks for, a security hole the change opens, a
  broken `CLAUDE.md` rule, or missing tests for the new behaviour. Every other finding is a follow-up, which the
  reviewer marks `bug` (a wrong result outside what the issue asks), `risk` (a security or data-loss risk, or a missing
  test for behaviour that could break) or `nit` (style, naming, comment wording, a small refactor); anything that is
  none of these is left out. Once the change lands, Mestari files the landing round's bugs and risks together as one
  follow-up issue (the follow-up label of `.paja/labels`, `paja-follow-up` in this repository), so a change asks for
  at most one more piece of work, and files nothing when there are none. Nits are never filed: when a round asks for
  changes anyway, the builder gets them to fix if a line or two does it, and otherwise they are dropped, kept only in
  the event log. Jono doesn't import the follow-up label, so a person queues the issue by relabelling it with the work
  label (`paja` here): reviewers suggest work, people set intent. Each re-reviewer sees the earlier follow-ups and
  repeats the ones that still apply. If the change doesn't land in that round (CI fails and the builder gets another round), the follow-ups stay
  in the event log unfiled, and the next round's reviewers file their own.
  Follow-ups become public issues, so reviewers never copy secrets, environment details or run output into them, and
  Mestari checks for itself too: before filing, it replaces values that start like a known credential (GitHub,
  GitLab, Anthropic and other `sk-` keys, AWS, Slack, npm, PyPI, JSON Web Tokens) or follow `Bearer`, `token`,
  `password`, `secret` or `api_key` (past quotes, backticks, `:`, `=` or `,`) with `<redacted>`, the whole value
  including any `.`, `+`, `/` or `=`, and the factory home, the repository and the user's home directory (also as
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
  files, the hook's rules). The person then gets the raw question and the reason. The prompt also tells the arbiter
  to escalate anything that changes the issue's intent or scope, secrets, actions outside the repository and any
  question that asks for the person's preference.
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
