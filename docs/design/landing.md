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
  broken `CLAUDE.md` rule, or missing tests for the new behaviour. Every other finding (hardening ideas, edge cases
  outside the issue, nits) is a follow-up: once the change lands, Mestari files the landing round's follow-ups (at most
  five) as `paja-follow-up` issues. Jono doesn't import that label, so a person queues one by relabelling it `paja`:
  reviewers suggest work, people set intent. Each re-reviewer sees the earlier follow-ups and repeats the ones
  that still apply. If the change doesn't land after all (CI fails), the follow-ups stay in the event log unfiled.
  Follow-ups become public issues, so reviewers never copy secrets, environment details or run output into them.
- **A re-review judges the fix.** After a round of changes, the next reviewer gets the earlier blockers and reads only
  the diff since the commit that was reviewed, not the whole change again. A round that commits no fix goes back to the
  builder without a review. High-risk changes are the exception: both
  reviewers read the whole change every round, since they must agree on all of it.
- **Cheaper models where the review reads less.** The first reviewer of a round runs as the `review` role in
  `.paja/models` (Opus here). A normal-risk re-review, which judges only the fix against a fixed list of blockers, runs
  as `re-review`, and the second high-risk reviewer, a second opinion on a change the first also reads in full, runs as
  `review-2` (both Sonnet at medium effort here). A second reviewer on a different model also tends to disagree for
  different reasons than a copy of the first would.
- **Each review is a fresh, short session.** Reviewers never share context with the builder or with each other, which
  keeps them independent and keeps each session small.

## When a person is asked

A person hears about a chunk only through Posti, and only when:

- reviewer agents disagree, or a check can't reach a verdict;
- a fix loop hits its cap of three rounds and a rethink didn't help;
- a change would weaken a check (loosen a rule, drop a scenario, lower the eval pass rate). That is a question about
  intent, not code, so it is asked as one, with options and a recommendation.

## Rollout

The high-risk gate first runs in observe mode: it records what it would land, and an independent audit checks those
verdicts. It starts landing for real once its verdicts match the audit. Until Paja's own reviewer runs (build step 3),
Claude reviews and lands the hand-built seed pull requests under these same rules.
