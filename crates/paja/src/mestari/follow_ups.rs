use super::*;

/// At most this many follow-up issues per change; the rest are folded into the last one.
pub(super) const MAX_FOLLOW_UPS: usize = 5;

/// File each non-blocking review finding as an issue with the repository's follow-up label,
/// once the change has landed. Jono imports only the work label, so a follow-up waits until a
/// person relabels it: reviewers suggest work, people set intent. A failure here is recorded,
/// never turned into an escalation: the change is already merged.
pub(super) fn file_follow_ups(
    home: &Home,
    repo: &Path,
    item: &WorkItem,
    pr: &str,
    findings: &[String],
    labels: &jono::Labels,
) {
    // Review text came from an agent that read untrusted code: keep each finding short, never
    // let it mention anyone, and keep tokens and local paths out of a public issue.
    let user_home = ["HOME", "USERPROFILE"]
        .iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    let mut places = vec![(home.root(), "<factory home>"), (repo, "<repository>")];
    places.extend(user_home.iter().map(|dir| (dir.as_path(), "<home>")));
    let mut findings: Vec<String> = findings
        .iter()
        .map(|finding| {
            let finding = redact(finding, &places);
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
             block landing. Relabel it `{work}` to queue it.",
            number = item.number,
            work = labels.work
        );
        let filed = run_gh(
            repo,
            &[
                "issue",
                "create",
                "--label",
                &labels.follow_up,
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
                     {}` and file this finding by hand",
                    labels.follow_up
                ),
            ),
        }
    }
}

/// Tokens that look like credentials become `<redacted>`, and each path in `places` (and
/// anything under it) becomes its label. Longer paths go first, so a worktree under the factory
/// home is named as the factory home, not cut in half. A path is matched only where no letter
/// or digit follows it, so `/home/me` leaves `/home/meg` alone. A place under the user's
/// home (the last entry labelled `<home>`) is also matched in its `~/...` spelling.
pub(super) fn redact(text: &str, places: &[(&Path, &str)]) -> String {
    let user_homes: Vec<String> = places
        .iter()
        .filter(|(_, label)| *label == "<home>")
        .map(|(path, _)| spelling(path))
        .collect();
    let mut spellings: Vec<(String, &str)> = Vec::new();
    for (path, label) in places {
        let shown = spelling(path);
        let mut forms = vec![shown.clone()];
        for user_home in &user_homes {
            if let Some(rest) = shown.strip_prefix(user_home.as_str()) {
                if rest.starts_with(['/', '\\']) {
                    forms.push(format!("~{rest}"));
                }
            }
        }
        for form in forms {
            // Both separators, since a finding may quote either spelling of a Windows path.
            for each in [form.replace('\\', "/"), form.replace('/', "\\"), form] {
                // A path of one or two characters (`/`, `C:`) would redact far too much.
                if each.len() > 2 {
                    spellings.push((each, *label));
                }
            }
        }
    }
    spellings.sort_by_key(|(spelling, _)| std::cmp::Reverse(spelling.len()));
    spellings.dedup();
    let mut text = text.to_string();
    for (spelling, label) in &spellings {
        text = replace_path(&text, spelling, label);
    }
    redact_tokens(&text)
}

/// `path` as text, without a trailing separator.
fn spelling(path: &Path) -> String {
    path.to_string_lossy()
        .trim_end_matches(['/', '\\'])
        .to_string()
}

/// `text` with each `path` that ends at a boundary (anything but a letter or digit) replaced by
/// `label`.
fn replace_path(text: &str, path: &str, label: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(path) {
        let after = &rest[at + path.len()..];
        // Only a letter or digit continues a name: `_` and `-` may be Markdown or punctuation
        // around the path, so a path before one is still redacted.
        let ends = !after.starts_with(char::is_alphanumeric);
        out.push_str(&rest[..at]);
        out.push_str(if ends { label } else { path });
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Prefixes of credentials that are safe to recognise by their start.
const TOKEN_PREFIXES: &[&str] = &[
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "sk-",
    "AKIA",
    "xoxb-",
    "xoxp-",
    "npm_",
    "pypi-",
];

/// Words that introduce a credential: the next long word after one of them, past spaces, `:`
/// or `=`, is redacted whatever it looks like.
const TOKEN_LEADS: &[&str] = &["bearer", "token", "password", "secret", "apikey", "api_key"];

/// The scan visits each word once, so its cost is linear in the text.
fn redact_tokens(text: &str) -> String {
    let token_char = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    // Whether the previous word leads a credential and only spaces, `:` or `=` came since.
    let mut led = false;
    while !rest.is_empty() {
        let start = rest.find(token_char).unwrap_or(rest.len());
        let gap = &rest[..start];
        out.push_str(gap);
        if !gap
            .chars()
            .all(|c| c.is_whitespace() || c == ':' || c == '=')
        {
            led = false;
        }
        rest = &rest[start..];
        if rest.is_empty() {
            break;
        }
        let end = rest.find(|c: char| !token_char(c)).unwrap_or(rest.len());
        let word = &rest[..end];
        let secret = word.len() >= 16
            && (led || TOKEN_PREFIXES.iter().any(|prefix| word.starts_with(prefix)));
        out.push_str(if secret { "<redacted>" } else { word });
        led = TOKEN_LEADS
            .iter()
            .any(|lead| word.eq_ignore_ascii_case(lead));
        rest = &rest[end..];
    }
    out
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
            "repo": item.repo,
            "number": item.number,
            "finding": finding,
            "issue": issue,
            "error": (!error.is_empty()).then_some(error),
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn findings_lose_tokens_and_local_paths() {
        let home = Path::new("/home/me/.paja");
        let repo = Path::new("/home/me/src/paja");
        let user = Path::new("/home/me");
        let places = [
            (home, "<factory home>"),
            (repo, "<repository>"),
            (user, "<home>"),
        ];
        // Built at run time, so secret scanners don't take the test's fake tokens for real ones.
        let token = format!("ghp_{}", "A1".repeat(12));
        let bearer = "b2".repeat(13);
        let text = format!(
            "See /home/me/.paja/envs/work-3/src/a.rs and /home/me/src/paja/b.rs, /home/me/notes, \
             token {token} and Authorization: Bearer {bearer}. Keep ghp_ and #12."
        );
        let shown = redact(&text, &places);
        assert_eq!(
            shown,
            "See <factory home>/envs/work-3/src/a.rs and <repository>/b.rs, <home>/notes, \
             token <redacted> and Authorization: Bearer <redacted>. Keep ghp_ and #12."
        );
    }

    #[test]
    fn redaction_covers_more_shapes_and_keeps_other_paths() {
        let user = Path::new("/home/me");
        let home = Path::new("/home/me/.paja");
        let places = [(home, "<factory home>"), (user, "<home>")];
        // Built at run time, so secret scanners don't take the test's fake tokens for real ones.
        let long = "c3".repeat(10);
        let key = format!("sk-{long}");
        let text = format!(
            "See ~/.paja/envs/x, /home/meg/a, _/home/me_, _~/.paja_, /home/me- and /home/me. Token: {long}, password={long}, \
             Bearer:{long}, key {key}, commit 0123456789abcdef0123 and a token of ours."
        );
        assert_eq!(
            redact(&text, &places),
            "See <factory home>/envs/x, /home/meg/a, _<home>_, _<factory home>_, <home>- and <home>. Token: <redacted>, \
             password=<redacted>, Bearer:<redacted>, key <redacted>, commit \
             0123456789abcdef0123 and a token of ours."
        );
    }

    #[test]
    fn redacting_a_long_finding_stays_fast() {
        let text = "word ".repeat(200_000);
        let started = std::time::Instant::now();
        assert_eq!(redact_tokens(&text), text);
        // A scan that re-read the text for every word would take minutes here.
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }

    #[test]
    fn windows_paths_are_redacted_in_either_spelling() {
        let home = Path::new(r"C:\Users\me\.paja");
        let places = [(home, "<factory home>"), (Path::new("C:"), "<drive>")];
        let shown = redact(
            r"C:\Users\me\.paja\envs\x and C:/Users/me/.paja/y on C:",
            &places,
        );
        assert_eq!(shown, r"<factory home>\envs\x and <factory home>/y on C:");
    }
}
