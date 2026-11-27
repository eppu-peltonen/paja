//! Posti: tells the person when a run needs them, so nobody has to watch the terminal. Each
//! notice goes to the local notifier (a desktop notification, or the program `PAJA_NOTIFIER`
//! names) and, when one is set, to a push URL (`PAJA_NOTIFY`, else `.paja/notify` on the
//! default branch). Every attempt is recorded as `notify.sent` or `notify.failed`. A notice
//! that can't be delivered never stops the run: it says so on stderr, once per channel.

use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde_json::json;

use crate::home::Home;

/// A notice reached a channel: `{"repo", "number", "kind", "channel"}`.
pub const EV_SENT: &str = "notify.sent";
/// A notice didn't reach a channel: the same fields plus `error`.
pub const EV_FAILED: &str = "notify.failed";
/// Names the push URL, on the default branch.
pub const NOTIFY_FILE: &str = ".paja/notify";

/// How long a notifier may take before Paja stops it and carries on.
const LIMIT: Duration = Duration::from_secs(15);

/// What happened, for the person. Texts hold no secrets or local paths: a push leaves the
/// machine.
pub struct Notice {
    /// The repository as `owner/name`, or None when it isn't known.
    pub repo: Option<String>,
    /// The work item, when the notice is about one.
    pub number: Option<u64>,
    /// `escalated`, `pass_failed` or `stopped`.
    pub kind: &'static str,
    pub title: String,
    pub text: String,
}

impl Notice {
    /// An item that needs the person's decision.
    pub fn escalated(repo: &str, number: u64, title: &str) -> Self {
        Self {
            repo: Some(repo.to_string()),
            number: Some(number),
            kind: "escalated",
            title: format!("Paja: {repo}#{number} needs a decision"),
            text: format!(
                "{title}. The question is on the issue; answer with \
                 paja work answer {number} \"<your answer>\"."
            ),
        }
    }

    /// A pass of `paja run` failed; with `--every` the loop carries on (`keeps_going`).
    pub fn pass_failed(repo: Option<String>, keeps_going: bool) -> Self {
        let place = repo.clone().unwrap_or_else(|| "a repository".to_string());
        let text = if keeps_going {
            "A pass of paja run failed and Paja keeps going; the error is in the terminal."
        } else {
            "paja run failed; the error is in the terminal."
        };
        Self {
            repo,
            number: None,
            kind: "pass_failed",
            title: format!("Paja: a pass failed in {place}"),
            text: text.to_string(),
        }
    }

    /// `paja run --every` stopped for a person.
    pub fn stopped(repo: Option<String>) -> Self {
        let place = repo.clone().unwrap_or_else(|| "a repository".to_string());
        Self {
            repo,
            number: None,
            kind: "stopped",
            title: format!("Paja stopped in {place}"),
            text: "paja run stopped and needs you; the reason is in the terminal.".to_string(),
        }
    }
}

/// Which failed passes of `paja run --every` Posti tells of: the first in a row only, so an
/// error that lasts all night (gh signed out, the network down) is one notice, not one per
/// pass. A pass that doesn't fail starts the count again.
#[derive(Debug, Default)]
pub struct Failures {
    failing: bool,
}

impl Failures {
    /// A pass failed; whether to tell the person.
    pub fn failed(&mut self) -> bool {
        !std::mem::replace(&mut self.failing, true)
    }

    /// A pass didn't fail.
    pub fn passed(&mut self) {
        self.failing = false;
    }
}

/// Tell the person a pass of `paja run` in `checkout` failed; `keeps_going` for `--every`.
pub fn pass_failed(home: &Home, checkout: &Path, keeps_going: bool) {
    let notice = Notice::pass_failed(repo_of(home, checkout), keeps_going);
    send(home, checkout, &notice);
}

/// Tell the person `paja run` in `checkout` stopped for them.
pub fn stopped(home: &Home, checkout: &Path) {
    send(home, checkout, &Notice::stopped(repo_of(home, checkout)));
}

/// The checkout's `owner/name`, when Jono can tell it within Posti's time limit. Jono asks
/// `gh`, which has no limit of its own, so Paja stops it at the limit: a `gh` that hangs
/// costs the notice its repository name, not the run its next pass.
fn repo_of(home: &Home, checkout: &Path) -> Option<String> {
    let path = std::path::absolute(checkout).ok()?;
    crate::jono::repo_name_within(home, &path, Some(LIMIT)).ok()
}

static LOCAL_WARNED: AtomicBool = AtomicBool::new(false);
static PUSH_WARNED: AtomicBool = AtomicBool::new(false);

/// Send `notice` on every channel that is on, for the checkout at `checkout`. Never fails.
pub fn send(home: &Home, checkout: &Path, notice: &Notice) {
    if let Some((channel, mut cmd)) = local_notifier(notice) {
        let result = crate::platform::run_briefly(&mut cmd, LIMIT)
            .map_err(|e| format!("the {channel} notifier didn't run: {e}"));
        outcome(home, notice, channel, result, &LOCAL_WARNED);
    }
    match push_url(home, checkout) {
        Ok(None) => {}
        Ok(Some(url)) => {
            let result = push(&url, notice);
            outcome(home, notice, "push", result, &PUSH_WARNED);
        }
        Err(err) => outcome(home, notice, "push", Err(err), &PUSH_WARNED),
    }
}

/// The local channel: `PAJA_NOTIFIER` names a program (run with the title and the text), or
/// `desktop`, or `none`. Unset, it is a desktop notification when stderr is a terminal, so a
/// script or a test never pops one up.
fn local_notifier(notice: &Notice) -> Option<(&'static str, std::process::Command)> {
    let desktop = || {
        (
            "desktop",
            crate::platform::desktop_notification(&notice.title, &notice.text),
        )
    };
    let Some(chosen) = std::env::var_os("PAJA_NOTIFIER") else {
        return std::io::IsTerminal::is_terminal(&std::io::stderr()).then(desktop);
    };
    if chosen.is_empty() || chosen == "none" {
        return None;
    }
    if chosen == "desktop" {
        return Some(desktop());
    }
    let mut cmd = std::process::Command::new(chosen);
    cmd.args([&notice.title, &notice.text]);
    Some(("PAJA_NOTIFIER", cmd))
}

/// The push URL: `PAJA_NOTIFY`, else the first line of `.paja/notify` on the default branch
/// that isn't empty or a `#` comment. A repository the tamper lock holds isn't read.
fn push_url(home: &Home, checkout: &Path) -> Result<Option<String>, String> {
    let url = match std::env::var("PAJA_NOTIFY") {
        Ok(url) if !url.trim().is_empty() => url.trim().to_string(),
        _ => {
            let Ok(ok) = crate::ahjo::untampered(home, checkout) else {
                return Ok(None);
            };
            let text = crate::ahjo::show_on_default_branch(&ok, NOTIFY_FILE)
                .map_err(|e| format!("couldn't read {NOTIFY_FILE}: {e:#}"))?;
            match text.as_deref().and_then(|t| {
                t.lines()
                    .map(str::trim)
                    .find(|l| !l.is_empty() && !l.starts_with('#'))
            }) {
                Some(line) => line.to_string(),
                None => return Ok(None),
            }
        }
    };
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(format!(
            "the push URL in PAJA_NOTIFY or {NOTIFY_FILE} must start with https:// (or http://)"
        ));
    }
    Ok(Some(url))
}

/// POST the notice's title and text to `url` with `curl`, which Windows 10 and later, macOS
/// and most Linux systems have.
fn push(url: &str, notice: &Notice) -> Result<(), String> {
    let body = format!("{}\n{}", notice.title, notice.text);
    let mut cmd = std::process::Command::new("curl");
    cmd.args([
        "-fsS",
        "-m",
        "10",
        "-H",
        "Title: Paja",
        "--data-raw",
        &body,
        url,
    ]);
    crate::platform::run_briefly(&mut cmd, LIMIT)
        .map_err(|e| format!("couldn't POST to the push URL with curl (is curl installed?): {e}"))
}

fn outcome(
    home: &Home,
    notice: &Notice,
    channel: &str,
    result: Result<(), String>,
    warned: &AtomicBool,
) {
    let mut data = json!({
        "repo": notice.repo,
        "number": notice.number,
        "kind": notice.kind,
        "channel": channel,
    });
    let kind = match &result {
        Ok(()) => EV_SENT,
        Err(err) => {
            data["error"] = json!(err);
            if !warned.swap(true, Ordering::SeqCst) {
                // A closed stderr must not stop the run either.
                let _ = writeln!(
                    std::io::stderr(),
                    "paja: {err}; Paja carries on without this notice (see Notifications in docs/setup.md)"
                );
            }
            EV_FAILED
        }
    };
    // The notice is the point; a log that can't be written mustn't stop the run either, nor
    // hold it up for long: a pass that failed waiting for the home lock may find it held still.
    if let Ok(lock) = home.lock_within(LIMIT) {
        let _ = home.events().append(&lock, kind, data);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_escalation_names_the_issue_and_the_answer_command() {
        let n = Notice::escalated("acme/app", 7, "Add a login page");
        assert_eq!(n.title, "Paja: acme/app#7 needs a decision");
        assert!(n.text.starts_with("Add a login page."), "{}", n.text);
        assert!(n.text.contains("paja work answer 7"), "{}", n.text);
    }

    #[test]
    fn only_the_first_failed_pass_in_a_row_is_told() {
        let mut f = Failures::default();
        assert!(f.failed());
        assert!(!f.failed());
        assert!(!f.failed());
        f.passed();
        assert!(f.failed());
    }

    #[test]
    fn a_pass_without_a_known_repository_still_reads() {
        let n = Notice::pass_failed(None, true);
        assert_eq!(n.title, "Paja: a pass failed in a repository");
    }
}
