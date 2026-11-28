use super::*;

/// A run that waits for another to finish landing in the same repository:
/// `{repo, number, round}`.
pub(super) const EV_MERGING: &str = "pool.merging";
/// A run that takes its turn to land: `{repo, number, round}`. It holds the turn until the
/// change lands, or goes back to its builder.
pub(super) const EV_LANDING: &str = "pool.landing";
/// GitHub's limit for a pull request's description, in characters.
const PR_BODY_CHARS: usize = 65_536;

/// How often a run waiting for its turn to land looks again.
const LAND_TICK: std::time::Duration = std::time::Duration::from_millis(500);

/// Wait for this repository's turn to land, taken under the home lock and held for as long as
/// the returned lock lives. Recorded as `pool.merging` while it waits, `pool.landing` once taken.
/// A `stop now` ends the wait.
pub(super) fn land_turn(home: &Home, item: &WorkItem, round: u32) -> Result<crate::home::RunLock> {
    let data = json!({ "repo": item.repo, "number": item.number, "round": round });
    let mut waiting = false;
    loop {
        {
            let lock = home.lock()?;
            if let Some(turn) = home.try_land_lock(&item.repo)? {
                home.events().append(&lock, EV_LANDING, data)?;
                return Ok(turn);
            }
            if !waiting {
                home.events().append(&lock, EV_MERGING, data.clone())?;
            }
        }
        if !waiting {
            waiting = true;
            progress("another change is landing in this repository; waiting for its turn");
        }
        stop_point()?;
        std::thread::sleep(LAND_TICK);
    }
}

/// How a landing attempt ended.
pub(super) enum Landing {
    /// Landed, or parked as a question for the person.
    Done(RunReport),
    /// CI failed on the pull request: what the builder needs to fix it in another round.
    Retry(String),
}

/// Push `judged` as the environment's branch and land it. `judged` is the commit Vasara's checks
/// and the reviewers saw, or that commit with the latest default branch merged in, which the
/// checks saw again; not whatever the branch points at now, since code run in the worktree could
/// have moved it. The caller holds the repository's turn to land.
#[allow(clippy::too_many_arguments)]
pub(super) fn land(
    home: &Home,
    repo: &Path,
    env: &Env,
    item: &WorkItem,
    rounds: u32,
    judged: &str,
    ci: CiMode,
    ci_wait: u64,
    target: &RepoSettings,
    unsettled: &[String],
) -> Result<Landing> {
    let gh = jono::gh_program();
    let branch = env.branch.as_str();
    let Some(merge_flag) = target.merge_flag() else {
        return escalated(home, repo, env, item, &no_merge_method(&item.repo), rounds)
            .map(Landing::Done);
    };
    git(
        &env.path,
        &[
            "push",
            "-q",
            // No `-u`: that would write the branch into the shared git configuration, which
            // other runs' guards watch.
            "origin",
            &format!("{judged}:refs/heads/{branch}"),
            &format!("--force-with-lease=refs/heads/{branch}"),
        ],
    )?;
    let title = format!("{} (#{})", item.title, item.number);
    let mut body = format!(
        "Closes #{}.\n\nBuilt and landed by Paja: Vasara's checks passed and an independent \
         reviewer agent approved it.",
        item.number
    );
    if !unsettled.is_empty() {
        body.push_str(
            "\n\nReviewers left these findings: ones outside this change, or still open at the \
             round limit. No issue was filed for them.\n",
        );
        for finding in unsettled {
            body.push_str(&format!("\n- {}", finding.replace('\n', "\n  ")));
        }
    }
    if ci == CiMode::None {
        body.push_str(&format!(
            "\n\nThis repository has no CI (`{CI_FILE}` is `none`), so Paja landed it on \
             Vasara's checks and the reviewers alone."
        ));
    }
    // The body goes in a file: with ten findings of up to 4000 characters it is past Windows'
    // command-line limit.
    let body = cut_short(body, PR_BODY_CHARS);
    let body_path = super::body_file(home, &format!("pr-{}", item.number), &body)?;
    let body_arg = body_path.to_string_lossy().into_owned();
    // Reuse this branch's pull request only while it is still open, with this round's body.
    let pr = match run_gh(
        repo,
        &[
            "pr",
            "view",
            branch,
            "--json",
            "url,state",
            "--jq",
            "select(.state == \"OPEN\") | .url",
        ],
    ) {
        // The description is only for people: a failed edit leaves last round's and the
        // landing carries on.
        Ok(url) if !url.is_empty() => {
            if let Err(err) = run_gh(repo, &["pr", "edit", &url, "--body-file", &body_arg]) {
                progress(&format!(
                    "couldn't update {url}'s description, so it lists last round's findings: \
                     {err:#}"
                ));
            }
            Ok(url)
        }
        _ => run_gh(
            repo,
            &[
                "pr",
                "create",
                "--base",
                &target.branch,
                "--head",
                branch,
                "--title",
                &title,
                "--body-file",
                &body_arg,
            ],
        ),
    };
    let _ = std::fs::remove_file(&body_path);
    let pr = pr?;
    // Someone following the run sees the pull request from here on.
    record(
        home,
        EV_STEP,
        json!({
            "repo": item.repo, "number": item.number, "round": rounds,
            "role": if ci == CiMode::None { "land" } else { "ci" }, "pr": pr,
        }),
    )?;
    // CI must be green before landing; a red CI is a failed check, not a merge.
    if ci == CiMode::None {
        progress(&format!("opened {pr}; the repository has no CI, landing"));
    } else {
        progress(&format!("opened {pr}; waiting for CI"));
        // A red CI is the builder's to fix, like a failed check: the pull request stays open
        // and the next round pushes to it.
        if let Err(err) = wait_for_ci(repo, &pr, ci_wait) {
            // `stop now` ended the wait: the run stops, and the next one resumes the item.
            if stopped(&err) {
                return Err(err);
            }
            if let Some(late) = err.downcast_ref::<CiTimedOut>() {
                let question = late.to_string();
                return escalated(home, repo, env, item, &question, rounds).map(Landing::Done);
            }
            let err = format!("{err:#}");
            // Only a failed check, or checks that never start because the default branch moved
            // on (GitHub runs none on a conflicting pull request), is the builder's to fix. A
            // `gh` or network error, or a repository whose workflows never run, is not.
            let failed_check = err.contains("\tfail\t");
            if !failed_check && !default_branch_moved(repo, &target.branch, judged) {
                let question =
                    format!("CI did not report on {pr}, so Paja did not land it:\n\n{err}");
                return escalated(home, repo, env, item, &question, rounds).map(Landing::Done);
            }
            let mut why = format!(
                "CI failed on {pr}, so Paja did not land it. CI runs on GitHub's machines, which \
                 differ from this one (no git identity, a fresh checkout, other temp folders), so \
                 fix the cause so the change passes there too:\n\n{err}"
            );
            if let Some(log) = failed_log(repo, &err) {
                why.push_str(&format!("\n\nThe end of the failed job's log:\n\n{log}"));
            }
            return Ok(Landing::Retry(why));
        }
    }
    // Free the worktree first: gh deletes the local branch too, which git refuses while a
    // worktree has it checked out.
    ahjo::remove(home, &env.name, true)?;
    if let Err(err) = run_gh(repo, &["pr", "merge", &pr, merge_flag, "--delete-branch"]) {
        let text = format!("{err:#}").to_lowercase();
        // Whether GitHub refused it is asked of GitHub, not read from the error's wording.
        if let Some(why) = merge_refused(repo, &item.repo, merge_flag) {
            let question =
                format!("GitHub refused to merge {pr} with `{merge_flag}`:\n\n{err:#}\n\n{why}");
            return escalated(home, repo, env, item, &question, rounds).map(Landing::Done);
        }
        // Only a hint on top of the error, for a repository with no CI: the wording is enough.
        if ci == CiMode::None && text.contains("status check") {
            return Err(err).with_context(|| {
                format!(
                    "{gh} pr merge {pr}: {CI_FILE} is `none`, but the default branch's protection \
                     still requires status checks; remove that requirement, or put `required` in \
                     {CI_FILE} and add a workflow"
                )
            });
        }
        return Err(err).with_context(|| {
            format!(
                "{gh} pr merge {pr} failed; unless gh's message says why (a network error, a \
                 conflict), check that the token `{gh} auth status` shows may write to {}, and \
                 the default branch's protection rules",
                item.repo
            )
        });
    }
    // Merged: from here nothing may turn this into an escalation.
    let _ = jono::set_status(home, &item.repo, item.number, Status::Landed, Some(&pr));
    // What the landing shows on a dashboard: best effort, since the merge is done either way.
    let merge_commit = run_gh(
        repo,
        &[
            "pr",
            "view",
            &pr,
            "--json",
            "mergeCommit",
            "--jq",
            ".mergeCommit.oid",
        ],
    )
    .ok()
    .map(|oid| oid.trim().to_string())
    .filter(|oid| (7..=64).contains(&oid.len()) && oid.chars().all(|c| c.is_ascii_hexdigit()));
    let checks_passed = home.events().read_all().ok().and_then(|events| {
        events
            .iter()
            .rev()
            .find(|e| {
                e.kind == EV_CHECKED
                    && e.data["repo"] == item.repo
                    && e.data["number"] == item.number
            })
            .and_then(|e| e.data["checks_passed"].as_u64())
    });
    let _ = record(
        home,
        EV_LANDED,
        json!({
            "repo": item.repo, "number": item.number, "pr": pr, "rounds": rounds,
            "merge_commit": merge_commit, "checks_passed": checks_passed,
        }),
    );
    progress(&format!("landed #{} ({pr})", item.number));
    Ok(Landing::Done(RunReport::Landed {
        number: item.number,
        pr,
        rounds,
    }))
}

/// Whether `branch` on origin has moved past what `judged` holds. A fetch that fails counts
/// as not moved, so the caller escalates rather than spending a round.
fn default_branch_moved(repo: &Path, branch: &str, judged: &str) -> bool {
    let Ok(tip) = super::fetch_tip(repo, branch) else {
        return false;
    };
    git(repo, &["merge-base", "--is-ancestor", &tip, judged]).is_err()
}

/// The last lines of the first failed job's log that `gh pr checks` linked to, if `gh` can
/// read it.
fn failed_log(repo: &Path, checks: &str) -> Option<String> {
    const LINES: usize = 60;
    let job = checks.split("/job/").nth(1)?;
    let job: String = job.chars().take_while(char::is_ascii_digit).collect();
    if job.is_empty() {
        return None;
    }
    let log = run_gh(repo, &["run", "view", "--job", &job, "--log-failed"]).ok()?;
    let lines: Vec<&str> = log.lines().collect();
    let tail = lines[lines.len().saturating_sub(LINES)..].join("\n");
    (!tail.trim().is_empty()).then_some(tail)
}

/// Why GitHub won't let the signed-in `gh` account merge with `flag` in `repo`, asked after a
/// merge failed: its permission there is below write, or the method is now off. None when
/// GitHub shows neither, or `gh` can't say, so the failure is an error instead.
fn merge_refused(checkout: &Path, repo: &str, flag: &str) -> Option<String> {
    let out = run_gh(
        checkout,
        &[
            "repo",
            "view",
            "--json",
            "viewerPermission,squashMergeAllowed,mergeCommitAllowed,rebaseMergeAllowed",
        ],
    )
    .ok()?;
    let v: Value = serde_json::from_str(&out).ok()?;
    merge_refusal(&v, repo, flag)
}

/// [`merge_refused`]'s reading of `gh repo view`'s answer `v`. A field it leaves out says
/// nothing.
fn merge_refusal(v: &Value, repo: &str, flag: &str) -> Option<String> {
    if let Some(permission) = v["viewerPermission"].as_str().filter(|p| !p.is_empty()) {
        if !matches!(permission, "ADMIN" | "MAINTAIN" | "WRITE") {
            return Some(format!(
                "The signed-in `gh` account has {permission} permission on {repo}, and merging \
                 needs write access. Give it write access (or sign `gh` in as an account that \
                 has it), then answer that it is changed."
            ));
        }
    }
    let methods = [
        (SQUASH, "squashMergeAllowed", "squash merging"),
        (REBASE, "rebaseMergeAllowed", "rebase merging"),
        (MERGE, "mergeCommitAllowed", "merge commits"),
    ];
    let off = |key: &str| v[key].as_bool() == Some(false);
    let (_, key, name) = methods.iter().find(|(f, ..)| *f == flag)?;
    if !off(key) {
        return None;
    }
    if methods.iter().all(|(_, key, _)| off(key)) {
        return Some(no_merge_method(repo));
    }
    Some(format!(
        "{repo} doesn't allow {name} now. Answer to land it with a method {repo} allows; the \
         next run picks one."
    ))
}

/// The question for a repository whose settings leave Paja no way to merge.
fn no_merge_method(repo: &str) -> String {
    format!(
        "Paja can't land this change: {repo} allows no merge method Paja can use. Under \
         Settings > General > Pull Requests, allow squash merging (preferred), rebase merging \
         or merge commits, and make sure the signed-in `gh` account may merge there, then \
         answer that it is changed."
    )
}

pub(super) fn escalated(
    home: &Home,
    repo: &Path,
    env: &Env,
    item: &WorkItem,
    question: &str,
    rounds: u32,
) -> Result<RunReport> {
    // The branch keeps the work; the worktree and its ports are freed.
    let _ = ahjo::remove(home, &env.name, true);
    let failed = escalate(home, repo, item, question, rounds)?;
    progress(&parked_line(
        &format!("#{} needs a decision", item.number),
        item.number,
        failed,
    ));
    Ok(RunReport::Escalated {
        number: item.number,
        question: question.to_string(),
        rounds,
    })
}

/// The progress line for a parked item: where the question is and how to retry come first,
/// so a long gh error at the end can't push them out of the line.
pub(super) fn parked_line(what: &str, number: u64, comment_error: Option<anyhow::Error>) -> String {
    let retry = format!("answer with `paja work answer {number} \"<your answer>\"`");
    match comment_error {
        None => format!("{what}; asked on the issue ({retry})"),
        Some(err) => format!(
            "{what}; couldn't comment on the issue, so see `paja work show {number}` for the \
             question ({retry}); gh said: {err:#}"
        ),
    }
}

/// Ask the person on the issue, and park the item until they answer. Returns the comment's
/// error when it didn't reach the issue.
pub(super) fn escalate(
    home: &Home,
    repo: &Path,
    item: &WorkItem,
    question: &str,
    rounds: u32,
) -> Result<Option<anyhow::Error>> {
    // Park the item first, so it never stays active if the comment fails.
    jono::set_status(
        home,
        &item.repo,
        item.number,
        Status::Escalated,
        Some(question),
    )?;
    record(
        home,
        EV_ESCALATED,
        json!({ "repo": item.repo, "number": item.number, "rounds": rounds }),
    )?;
    // The item is parked either way; a failed comment must not escalate it a second time.
    // The caller reports it, cleaned for the terminal.
    let asked = comment(home, repo, item, question).err();
    tell(home, repo, item);
    Ok(asked)
}

/// Tell the person the item needs them (Posti), next to the comment on the issue, which
/// GitHub doesn't notify them about when it is posted with their own token.
pub(super) fn tell(home: &Home, repo: &Path, item: &WorkItem) {
    let notice = crate::posti::Notice::escalated(&item.repo, item.number, &item.title);
    tell_with(home, repo, item, &notice);
}

/// [`tell`], with `notice` as what Posti sends.
pub(super) fn tell_with(home: &Home, repo: &Path, item: &WorkItem, notice: &crate::posti::Notice) {
    // The notice first: labelling makes up to three `gh` calls, which a slow GitHub could hold
    // up.
    crate::posti::send(home, repo, notice);
    if let Err(err) = jono::mark_needs_answer(home, repo, item.number) {
        progress(&format!(
            "couldn't label #{} as waiting for an answer: {err:#}",
            item.number
        ));
    }
}

/// Ask the person on the issue. The question can quote CI logs or review text, so it is
/// redacted first: the issue is public.
pub(super) fn comment(home: &Home, repo: &Path, item: &WorkItem, question: &str) -> Result<()> {
    let question =
        follow_ups::redact_for_comment(question, repo, home.root(), follow_ups::QUOTED_CHARS);
    let body = format!(
        "**Paja needs a decision on this issue.**\n\n{question}\n\nAnswer with \
         `paja work answer {n} \"<your answer>\"` (or `/paja:answer` in Claude Code). That records \
         the answer for Paja's next run and queues the item again; a reply here alone doesn't \
         reach the next run.",
        n = item.number
    );
    follow_ups::comment_on_issue(home, repo, item.number, &body)
}

/// Whether `err` is a step that `stop now` (or a second Ctrl-C) ended.
fn stopped(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::Interrupted)
    })
}

/// CI still going at the limit `.paja/ci`'s `wait` sets: the question for the person. Never
/// the builder's to fix, so the run escalates it whatever else happened during the wait.
#[derive(Debug)]
pub(super) struct CiTimedOut(pub(super) String);

impl std::fmt::Display for CiTimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CiTimedOut {}

/// Wait until GitHub has registered checks on the pull request, then until they finish, for
/// at most `minutes` in all (`.paja/ci`'s `wait`; debug builds: `PAJA_TEST_CI_WAIT_MS`). A
/// pull request that gets no checks at all is not treated as green, and neither is CI still
/// going at the limit, which is a [`CiTimedOut`]: the run holds the repository's turn to land
/// while it waits, so a CI that hangs would otherwise hold up every landing there. `stop now`
/// ends the wait at once.
pub(super) fn wait_for_ci(repo: &Path, pr: &str, minutes: u64) -> Result<()> {
    const APPEAR: std::time::Duration = std::time::Duration::from_secs(300);
    const AGAIN: std::time::Duration = std::time::Duration::from_secs(10);
    let limit = std::env::var("PAJA_TEST_CI_WAIT_MS")
        .ok()
        .filter(|_| cfg!(debug_assertions))
        .and_then(|ms| ms.parse().ok())
        .map_or(
            std::time::Duration::from_secs(minutes * 60),
            std::time::Duration::from_millis,
        );
    let start = std::time::Instant::now();
    let deadline = start + limit;
    let gh = jono::gh_program();
    let args = ["pr", "checks", pr, "--watch", "--fail-fast"];
    loop {
        let mut cmd = Command::new(&gh);
        cmd.env("GH_PROMPT_DISABLED", "1")
            .current_dir(repo)
            .args(args)
            .stdin(std::process::Stdio::null());
        let ended =
            crate::platform::output_until_stopped_or(&mut cmd, deadline).with_context(|| {
                format!("running `{gh}` (install the GitHub CLI and run `gh auth login`)")
            })?;
        if ended.timed_out {
            return Err(still_going(pr, limit, minutes, &tail(&ended.output)).into());
        }
        if ended.output.status.success() {
            return Ok(());
        }
        let err = format!("`{gh} {}` failed: {}", args.join(" "), tail(&ended.output));
        if !err.contains("no checks reported") {
            bail!(err);
        }
        // No checks yet: that is the workflows' to fix, not a hung CI, so it keeps its own
        // message, cut to the limit when that is shorter.
        let waited = start.elapsed();
        if waited > APPEAR || waited + AGAIN >= limit {
            bail!(
                "no CI checks appeared on {pr} within {}; check the workflows",
                long(APPEAR.min(limit))
            );
        }
        // In short steps, so `stop now` isn't left waiting between two tries.
        let next = std::time::Instant::now() + AGAIN;
        while std::time::Instant::now() < next {
            if crate::platform::interrupts() >= 2 {
                return Err(crate::platform::stopped_error().into());
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
}

/// The question for CI on `pr` still going after `limit`, with the checks as `gh` last showed
/// them.
fn still_going(pr: &str, limit: std::time::Duration, minutes: u64, checks: &str) -> CiTimedOut {
    let mut text = format!(
        "CI on {pr} was still going after {}, so Paja stopped waiting for it and did not land \
         it. Look at the pull request's checks on GitHub: a job may be stuck, or queued with no \
         runner to take it. If this repository's CI takes longer, put `wait <minutes>` in \
         {CI_FILE} on the default branch (it is now {}).\n\nThe change itself needs nothing: \
         once the checks are unstuck or the wait is raised, answer, and Paja checks this pull \
         request's CI again before it lands.",
        long(limit),
        long(std::time::Duration::from_secs(minutes * 60))
    );
    let checks = last_table(checks);
    if !checks.is_empty() {
        // A fence longer than any run of backticks in the table, so none can end it.
        let mut fence = "```".to_string();
        while checks.contains(fence.as_str()) {
            fence.push('`');
        }
        text.push_str(&format!(
            "\n\nThe checks when Paja stopped waiting:\n\n{fence}\n{checks}\n{fence}"
        ));
    }
    CiTimedOut(text)
}

/// The last table in `gh pr checks --watch`'s output: without a terminal it prints the whole
/// table again at each refresh, with a blank line between them.
fn last_table(output: &str) -> &str {
    output
        .trim()
        .rsplit("\n\n")
        .map(str::trim)
        .find(|table| !table.is_empty())
        .unwrap_or_default()
}

/// `limit` as a person reads it: whole minutes, or seconds below one.
fn long(limit: std::time::Duration) -> String {
    match limit.as_secs() {
        1 => "1 second".to_string(),
        s if s < 60 => format!("{s} seconds"),
        60..=119 => "1 minute".to_string(),
        s => format!("{} minutes", s / 60),
    }
}

/// `body` cut to at most `limit` characters, ending "(cut short)" when it was longer.
fn cut_short(body: String, limit: usize) -> String {
    if body.chars().count() <= limit {
        return body;
    }
    let cut = "\n\n(cut short)";
    let mut kept: String = body.chars().take(limit - cut.chars().count()).collect();
    kept.push_str(cut);
    kept
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_merge_refusal_is_read_from_github_s_settings() {
        use serde_json::json;
        let why = |v: serde_json::Value, flag: &str| super::merge_refusal(&v, "o/r", flag);
        // Write access and the method allowed: not a refusal, whatever the error said.
        let fine = json!({"viewerPermission": "WRITE", "squashMergeAllowed": true});
        assert_eq!(why(fine, super::SQUASH), None);
        // Fields gh leaves out say nothing.
        assert_eq!(why(json!({}), super::SQUASH), None);
        for permission in ["READ", "TRIAGE"] {
            let v = json!({ "viewerPermission": permission });
            let text = why(v, super::SQUASH).unwrap();
            assert!(
                text.contains(&format!("has {permission} permission on o/r")),
                "{text}"
            );
        }
        // The method Paja chose was switched off, but another is still on: an answer is enough.
        for (flag, key, name) in [
            (super::SQUASH, "squashMergeAllowed", "squash merging"),
            (super::REBASE, "rebaseMergeAllowed", "rebase merging"),
            (super::MERGE, "mergeCommitAllowed", "merge commits"),
        ] {
            let v = json!({ "viewerPermission": "ADMIN", key: false });
            let text = why(v, flag).unwrap();
            assert!(
                text.contains(&format!("doesn't allow {name} now")),
                "{text}"
            );
            assert!(text.contains("the next run picks one"), "{text}");
        }
        // All three off: the setting to change.
        let none = json!({"squashMergeAllowed": false, "rebaseMergeAllowed": false,
                          "mergeCommitAllowed": false});
        assert!(why(none, super::REBASE)
            .unwrap()
            .contains("allow squash merging"));
        // An empty permission says nothing either.
        assert_eq!(why(json!({"viewerPermission": ""}), super::SQUASH), None);
        // Another method being off says nothing about this one.
        let v = json!({"viewerPermission": "ADMIN", "rebaseMergeAllowed": false});
        assert_eq!(why(v, super::SQUASH), None);
    }

    #[test]
    fn the_timeout_question_shows_only_the_last_table_in_a_fence_it_cannot_end() {
        let watched = "Refreshing checks status every 10 seconds.\n\ncheck\tpending\t0\n\n\
                       check\tpending\t10s\nlint\tpass\t5s\n";
        assert_eq!(
            super::last_table(watched),
            "check\tpending\t10s\nlint\tpass\t5s"
        );
        assert_eq!(super::last_table("  \n"), "");
        let question = super::still_going(
            "https://example.com/pull/1",
            std::time::Duration::from_secs(60),
            1,
            "odd ``` name\tpending\t0",
        )
        .to_string();
        assert!(question.contains("after 1 minute,") && question.contains("(it is now 1 minute)"));
        assert!(
            question.ends_with("\n````\nodd ``` name\tpending\t0\n````"),
            "{question}"
        );
        let quiet = super::still_going(
            "https://example.com/pull/1",
            std::time::Duration::from_secs(1),
            1,
            "",
        );
        assert!(!quiet.to_string().contains("The checks when"), "{quiet}");
        assert!(quiet.to_string().contains("after 1 second,"), "{quiet}");
    }

    #[test]
    fn a_body_past_the_limit_is_cut_to_it_by_characters() {
        let body = "ä".repeat(100);
        let cut = super::cut_short(body.clone(), 50);
        assert_eq!(cut.chars().count(), 50);
        assert!(cut.ends_with("\n\n(cut short)"));
        assert!(cut.starts_with("ää"));
        assert_eq!(super::cut_short(body.clone(), 100), body);
    }
}
