use super::*;

/// The label follow-up issues get. Jono imports only `paja`, so a follow-up waits until a
/// person relabels it: reviewers suggest work, people set intent.
pub const FOLLOW_UP_LABEL: &str = "paja-follow-up";

/// At most this many follow-up issues per change; the rest are folded into the last one.
pub(super) const MAX_FOLLOW_UPS: usize = 5;

/// File each non-blocking review finding as a `paja-follow-up` issue, once the change has
/// landed. A failure here is recorded, never turned into an escalation: the change is already
/// merged.
pub(super) fn file_follow_ups(
    home: &Home,
    repo: &Path,
    item: &WorkItem,
    pr: &str,
    findings: &[String],
) {
    // Review text came from an agent that read untrusted code: keep each finding short, and
    // never let it mention anyone.
    let mut findings: Vec<String> = findings
        .iter()
        .map(|finding| {
            let mut text: String = finding.chars().take(4000).collect();
            if text.len() < finding.len() {
                text.push_str("\n\n(cut short)");
            }
            text.replace('@', "@\u{200b}")
        })
        .collect();
    if findings.len() > MAX_FOLLOW_UPS {
        let rest = findings.split_off(MAX_FOLLOW_UPS - 1);
        findings.push(format!(
            "More review follow-ups for #{}\n\n{}",
            item.number,
            rest.join("\n\n---\n\n")
        ));
    }
    for finding in &findings {
        let title: String = finding
            .lines()
            .map(|line| {
                line.trim()
                    .trim_matches(['#', '-', '*', '_', '`', ' '])
                    .trim()
            })
            .find(|line| !line.is_empty())
            .unwrap_or("Review follow-up")
            .chars()
            .take(100)
            .collect();
        let body = format!(
            "{finding}\n\nA reviewer found this while reviewing #{number} ({pr}). It did not \
             block landing. Relabel it `paja` to queue it.",
            number = item.number
        );
        let filed = run_gh(
            repo,
            &[
                "issue",
                "create",
                "--label",
                FOLLOW_UP_LABEL,
                "--title",
                &title,
                "--body",
                &body,
            ],
        );
        match &filed {
            Ok(issue) => record_follow_up(home, item, finding, Some(issue.as_str()), ""),
            Err(err) => record_follow_up(
                home,
                item,
                finding,
                None,
                &format!(
                    "{err:#}; if the label is missing, create it with `gh label create \
                     {FOLLOW_UP_LABEL}` and file this finding by hand"
                ),
            ),
        }
    }
}

/// Keep a follow-up in the log, filed or not, so no finding is lost.
pub(super) fn record_follow_up(
    home: &Home,
    item: &WorkItem,
    finding: &str,
    issue: Option<&str>,
    error: &str,
) {
    let _ = record(
        home,
        EV_FOLLOW_UP,
        json!({
            "number": item.number,
            "finding": finding,
            "issue": issue,
            "error": (!error.is_empty()).then_some(error),
        }),
    );
}
