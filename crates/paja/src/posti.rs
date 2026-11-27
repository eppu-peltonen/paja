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

/// A notice reached a channel: `{"repo", "number", "kind", "channel"}`, plus `subject` (the
/// roles it named) for a `skipped_role` notice.
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
    /// `escalated`, `pass_failed`, `stopped` or `skipped_role`.
    pub kind: &'static str,
    /// What the notice is about beyond the item, recorded with it: the roles a `skipped_role`
    /// notice names, so each role is told once.
    pub subject: Option<Vec<String>>,
    pub title: String,
    pub text: String,
}

impl Notice {
    /// An item set escalated by hand (`paja work set`, the `work_set` tool): its note stays in
    /// the log, since a push leaves the machine, and no question is on the issue.
    pub fn escalated_by_hand(repo: &str, number: u64, title: &str) -> Self {
        Self {
            text: format!(
                "{title}. It was set escalated by hand; paja work show {number} has the note. \
                 Answer with paja work answer {number} \"<your answer>\"."
            ),
            ..Self::escalated(repo, number, title)
        }
    }

    /// An item that needs the person's decision.
    pub fn escalated(repo: &str, number: u64, title: &str) -> Self {
        Self {
            repo: Some(repo.to_string()),
            number: Some(number),
            kind: "escalated",
            subject: None,
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
            subject: None,
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
            subject: None,
            title: format!("Paja stopped in {place}"),
            text: "paja run stopped and needs you; the reason is in the terminal.".to_string(),
        }
    }
}

/// The most characters a notice shows of one role, escaped, and the most roles it lists; the
/// rest are counted. So a notice's arguments and push body stay small whatever main's file
/// holds.
const ROLE_SHOWN: usize = 64;
const ROLES_SHOWN: usize = 10;

/// A role as a notice shows it: quoted and escaped at least as much as the run's warning, so a
/// control character in the file never reaches a terminal or a phone, and cut after
/// `ROLE_SHOWN` escaped characters, never inside one character's escape.
fn shown_role(role: &str) -> String {
    let mut shown = String::new();
    for c in role.chars() {
        // Escaped as `{:?}` escapes a one-character string: a quote `'` stays plain.
        let quoted = format!("{:?}", c.to_string());
        let escaped = &quoted[1..quoted.len() - 1];
        if shown.chars().count() + escaped.chars().count() > ROLE_SHOWN {
            return format!("\"{shown}\"…");
        }
        shown.push_str(escaped);
    }
    format!("\"{shown}\"")
}

impl Notice {
    /// `.paja/models` on `branch` names roles this Paja doesn't know, so their lines are
    /// skipped.
    pub fn skipped_roles(repo: &str, branch: &str, roles: &[String]) -> Self {
        let mut shown: Vec<String> = roles
            .iter()
            .take(ROLES_SHOWN)
            .map(|r| shown_role(r))
            .collect();
        if roles.len() > ROLES_SHOWN {
            shown.push(format!("and {} more", roles.len() - ROLES_SHOWN));
        }
        let (named, these) = if roles.len() == 1 {
            (format!("a line for role {}", shown[0]), "the role runs")
        } else {
            (
                format!("lines for roles {}", shown.join(", ")),
                "the roles run",
            )
        };
        Self {
            repo: Some(repo.to_string()),
            number: None,
            kind: "skipped_role",
            subject: Some(roles.to_vec()),
            title: format!("Paja: {repo} names a role this Paja doesn't know"),
            text: format!(
                ".paja/models on {branch} has {named}, which this Paja skips, so {these} on \
                 the default model or turn cap. If a role is new, install the newer Paja; if it \
                 is a typo, fix the line."
            ),
        }
    }
}

/// Tell the person, in one notice, of the roles in `skipped` (the `.paja/models` roles this
/// Paja doesn't know) that no earlier notice for `repo` named, so a runner left unattended
/// hears of them without anyone running `paja doctor`. A notice that was tried counts,
/// delivered or not, so a channel that is down doesn't bring the same notice back at every
/// pass. Posti's lock for `repo` is held from reading the log until the notice is recorded, so
/// processes starting together (a pool's lanes) send it once; a process that can't get it in
/// time leaves the notice to the next pass.
pub fn skipped_roles(home: &Home, checkout: &Path, repo: &str, branch: &str, skipped: &[String]) {
    if skipped.is_empty() {
        return;
    }
    let Ok(Some(_lock)) = home.posti_lock_within(repo, LIMIT) else {
        return;
    };
    let told = told_roles(home, repo);
    let new: Vec<String> = skipped
        .iter()
        .filter(|r| !told.contains(*r))
        .cloned()
        .collect();
    if !new.is_empty() {
        send(home, checkout, &Notice::skipped_roles(repo, branch, &new));
    }
}

/// The roles earlier `skipped_role` notices for `repo` named, from the log. A log that can't be
/// read tells nothing, so the notice goes (again, at worst).
fn told_roles(home: &Home, repo: &str) -> Vec<String> {
    let Ok(events) = home.events().read_all() else {
        return Vec::new();
    };
    events
        .iter()
        .filter(|e| e.kind == EV_SENT || e.kind == EV_FAILED)
        .filter(|e| e.data["kind"] == "skipped_role" && e.data["repo"] == repo)
        .filter_map(|e| e.data["subject"].as_array())
        .flatten()
        .filter_map(|r| r.as_str().map(str::to_string))
        .collect()
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
    if url.starts_with("https://") {
        return Ok(Some(url));
    }
    if url.strip_prefix("http://").is_some_and(loopback) {
        return Ok(Some(url));
    }
    Err(format!(
        "the push URL in PAJA_NOTIFY or {NOTIFY_FILE} must start with https:// (http:// only for \
         this machine: localhost, 127.x.x.x or [::1]), since a notice crosses the network"
    ))
}

/// Whether the rest of an `http://` URL names this machine: `localhost`, `127.x.x.x` or
/// `[::1]`, with or without a port.
fn loopback(rest: &str) -> bool {
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    // Credentials before an `@` don't name the host.
    let host = authority.rsplit('@').next().unwrap_or_default();
    let host = match host.strip_prefix('[') {
        Some(v6) => match v6.split_once(']') {
            Some((addr, port)) if port.is_empty() || port.starts_with(':') => {
                return addr == "::1" && port_ok(port);
            }
            _ => return false,
        },
        None => host,
    };
    let (name, port) = match host.split_once(':') {
        Some((name, port)) => (name, format!(":{port}")),
        None => (host, String::new()),
    };
    let ipv4 = name
        .parse::<std::net::Ipv4Addr>()
        .is_ok_and(|ip| ip.is_loopback());
    (name.eq_ignore_ascii_case("localhost") || ipv4) && port_ok(&port)
}

/// An empty port part, or `:` and digits.
fn port_ok(port: &str) -> bool {
    match port.strip_prefix(':') {
        None => port.is_empty(),
        Some(digits) => !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()),
    }
}

/// POST the notice's title and text to `url` with `curl`, which Windows 10 and later, macOS
/// and most Linux systems have.
fn push(url: &str, notice: &Notice) -> Result<(), String> {
    let body = format!("{}\n{}", notice.title, notice.text);
    let mut cmd = std::process::Command::new("curl");
    // Plain http is only ever this machine: never through a proxy, where it would cross the
    // network in clear text.
    if url.starts_with("http://") {
        cmd.args(["--noproxy", "*"]);
    }
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
    if let Some(subject) = &notice.subject {
        data["subject"] = json!(subject);
    }
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
    fn plain_http_is_only_for_this_machine() {
        for ok in [
            "localhost",
            "localhost:8080/paja",
            "LOCALHOST/x",
            "127.0.0.1:80",
            "127.1.2.3/t",
            "[::1]:2586/t",
            "[::1]",
            "me:pw@localhost/t",
        ] {
            assert!(loopback(ok), "{ok}");
        }
        for bad in [
            "ntfy.example/paja",
            "localhost.example/t",
            "127.0.0.1.example/t",
            "localhost@ntfy.example/t",
            "[::2]/t",
            "[::1]x/t",
            "localhost:/t",
            "localhost:80x",
            "10.0.0.1/t",
            "",
        ] {
            assert!(!loopback(bad), "{bad}");
        }
    }

    #[test]
    fn skipped_roles_are_named_escaped_and_cut() {
        let n = Notice::skipped_roles("acme/app", "main", &["rev\u{1b}[2Jiewer".into()]);
        assert_eq!(n.kind, "skipped_role");
        assert_eq!(n.subject, Some(vec!["rev\u{1b}[2Jiewer".to_string()]));
        assert!(!n.text.contains('\u{1b}'), "{}", n.text);
        assert!(n.text.contains(r#"role "rev\u{1b}[2Jiewer""#), "{}", n.text);
        assert!(n.text.contains(".paja/models on main"), "{}", n.text);
        // Several roles go in one notice; a long one is cut in the text, not in the record.
        let long = "x".repeat(500);
        let n = Notice::skipped_roles("acme/app", "main", &["a".into(), long.clone()]);
        assert!(n.text.contains(r#"roles "a", "xxx"#), "{}", n.text);
        assert!(n.text.contains("x\"…"), "{}", n.text);
        assert!(n.text.len() < 400, "{}", n.text);
        assert_eq!(n.subject, Some(vec!["a".to_string(), long]));
        // Escapes count toward the cut, and only so many roles are listed.
        let escapes = "\u{1b}".repeat(500);
        assert_eq!(shown_role(&escapes).chars().count(), 2 + 60 + 1);
        assert_eq!(shown_role("it's"), format!("{:?}", "it's"));
        let many: Vec<String> = (0..1000).map(|i| format!("{escapes}{i}")).collect();
        let n = Notice::skipped_roles("acme/app", "main", &many);
        assert!(n.text.len() < 1500, "{}", n.text.len());
        assert!(n.text.contains("and 990 more"), "{}", n.text);
        assert_eq!(n.subject.map(|s| s.len()), Some(1000));
    }

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
