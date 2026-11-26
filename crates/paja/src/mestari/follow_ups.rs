use super::*;

/// What kind of finding a reviewer reports beside its verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    /// A wrong result outside what the issue asks.
    Bug,
    /// A security or data-loss risk, or a missing test for behaviour that could break.
    Risk,
    /// Style, naming, comment wording or a small refactor: fixed in a round that runs anyway,
    /// never filed.
    Nit,
}

/// One non-blocking review finding. A plain string, as reviewers wrote them before findings
/// were marked, reads as a `risk`, so it is still filed rather than lost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub kind: FindingKind,
    #[serde(rename = "finding")]
    pub text: String,
}

impl<'de> Deserialize<'de> for Finding {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Shape {
            Plain(String),
            Marked { kind: FindingKind, finding: String },
        }
        Ok(match Shape::deserialize(deserializer)? {
            Shape::Plain(text) => Finding {
                kind: FindingKind::Risk,
                text,
            },
            Shape::Marked { kind, finding } => Finding {
                kind,
                text: finding,
            },
        })
    }
}

impl Finding {
    /// The finding with its kind, as a reviewer or the log sees it.
    pub fn shown(&self) -> String {
        let kind = match self.kind {
            FindingKind::Bug => "bug",
            FindingKind::Risk => "risk",
            FindingKind::Nit => "nit",
        };
        format!("[{kind}] {}", self.text)
    }
}

/// At most this many findings go in a change's follow-up issue; the rest stay in the log.
pub(super) const MAX_FILED: usize = 10;

/// The findings worth an issue (bugs and risks) and the nits, which are never filed.
pub(super) fn worth_filing(findings: &[Finding]) -> (Vec<&Finding>, Vec<&Finding>) {
    findings.iter().partition(|f| f.kind != FindingKind::Nit)
}

/// File the landed change's bugs and risks together as one issue with the repository's
/// follow-up label, so one change asks for at most one more round of work. Nits are recorded in
/// the log and dropped; with nothing else, no issue is filed. Jono imports only the work label,
/// so the follow-up waits until a person relabels it: reviewers suggest work, people set intent.
/// A failure here is recorded, never turned into an escalation: the change is already merged.
pub(super) fn file_follow_ups(
    home: &Home,
    repo: &Path,
    item: &WorkItem,
    pr: &str,
    findings: &[Finding],
    labels: &jono::Labels,
) {
    // Review text came from an agent that read untrusted code: keep each finding short, never
    // let it mention anyone, and keep tokens and local paths out of a public issue and the log.
    let user_home = ["HOME", "USERPROFILE"]
        .iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    let mut places = vec![(home.root(), "<factory home>"), (repo, "<repository>")];
    places.extend(user_home.iter().map(|dir| (dir.as_path(), "<home>")));
    let (filed, nits) = worth_filing(findings);
    for nit in &nits {
        record_follow_up(
            home,
            item,
            &redact(&nit.shown(), &places),
            None,
            "not filed: a nit",
        );
    }
    let (filed, left) = filed.split_at(filed.len().min(MAX_FILED));
    for finding in left {
        record_follow_up(
            home,
            item,
            &redact(&finding.shown(), &places),
            None,
            &format!("not filed: the issue holds the first {MAX_FILED} findings"),
        );
    }
    if filed.is_empty() {
        return;
    }
    let shown: Vec<String> = filed
        .iter()
        .map(|finding| {
            let text = redact(&finding.shown(), &places);
            let mut cut: String = text.chars().take(4000).collect();
            if cut.len() < text.len() {
                cut.push_str("\n\n(cut short)");
            }
            cut.replace('@', "@\u{200b}")
        })
        .collect();
    let first: String = shown[0]
        .lines()
        .map(|line| {
            line.trim()
                .trim_start_matches("[bug]")
                .trim_start_matches("[risk]")
                .trim_matches(['#', '-', '*', '_', '`', ' '])
                .trim()
        })
        .find(|line| !line.is_empty())
        .unwrap_or("Review follow-up")
        .chars()
        .take(90)
        .collect();
    let title = match shown.len() {
        1 => first,
        n => format!("{first} (+{} more)", n - 1),
    };
    let body = format!(
        "{}\n\nReviewers found these while reviewing #{number} ({pr}). They did not block \
         landing. Relabel this issue `{work}` to queue them as one piece of work.",
        shown.join("\n\n---\n\n"),
        number = item.number,
        work = labels.work
    );
    // The body goes in a file: up to ten findings would not fit a Windows command line.
    let body_file = home.root().join(format!(
        "follow-ups-{}-{}.md",
        item.number,
        std::process::id()
    ));
    let issue = std::fs::write(&body_file, &body)
        .with_context(|| format!("writing {}", body_file.display()))
        .and_then(|()| {
            run_gh(
                repo,
                &[
                    "issue",
                    "create",
                    "--label",
                    &labels.follow_up,
                    "--title",
                    &title,
                    "--body-file",
                    &body_file.to_string_lossy(),
                ],
            )
        });
    let _ = std::fs::remove_file(&body_file);
    for finding in &shown {
        match &issue {
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

/// Whether `path` is spelled the Windows way (a drive letter or a `\`), where case doesn't
/// matter.
fn is_windows_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    path.contains('\\') || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
}

/// `text` with each `path` that ends at a boundary (anything but a letter or digit) replaced by
/// `label`. A Windows path matches in any letter case, as Windows itself does.
fn replace_path(text: &str, path: &str, label: &str) -> String {
    // ASCII lowercasing keeps every byte offset, so a match in `haystack` is one in `text`.
    let fold = is_windows_path(path);
    let (haystack, needle) = if fold {
        (text.to_ascii_lowercase(), path.to_ascii_lowercase())
    } else {
        (text.to_string(), path.to_string())
    };
    let mut out = String::with_capacity(text.len());
    let mut from = 0;
    while let Some(found) = haystack[from..].find(&needle) {
        let at = from + found;
        let end = at + needle.len();
        // Only a letter or digit continues a name: `_` and `-` may be Markdown or punctuation
        // around the path, so a path before one is still redacted.
        let ends = !text[end..].starts_with(char::is_alphanumeric);
        out.push_str(&text[from..at]);
        out.push_str(if ends { label } else { &text[at..end] });
        from = end;
    }
    out.push_str(&text[from..]);
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
    // A JSON Web Token's header.
    "eyJ",
];

/// Words that introduce a credential: the next long value after one of them, past spaces,
/// quotes, backticks, `:`, `=` or `,`, is redacted whatever it looks like.
const TOKEN_LEADS: &[&str] = &["bearer", "token", "password", "secret", "apikey", "api_key"];

/// The scan visits each word once, so its cost is linear in the text. A redacted value runs on
/// through `.`, `+`, `/` and `=`, so a dotted token (a JWT) or a base64 secret goes whole.
fn redact_tokens(text: &str) -> String {
    let token_char = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    let value_char = |c: char| token_char(c) || matches!(c, '.' | '+' | '/' | '=');
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    // Whether the previous word leads a credential and only spaces, quotes, backticks, `:`, `=`
    // or `,` came since.
    let mut led = false;
    while !rest.is_empty() {
        // After a lead, a value may also start with base64's `+` or `/`.
        let start = rest
            .find(|c: char| token_char(c) || (led && matches!(c, '+' | '/')))
            .unwrap_or(rest.len());
        let gap = &rest[..start];
        out.push_str(gap);
        if !gap
            .chars()
            .all(|c| c.is_whitespace() || matches!(c, ':' | '=' | ',' | '"' | '\'' | '`'))
        {
            led = false;
        }
        rest = &rest[start..];
        if rest.is_empty() {
            break;
        }
        // A short value that starts with `+` or `/` is left as it is, one character at a time.
        let end = match rest.find(|c: char| !token_char(c)).unwrap_or(rest.len()) {
            0 => rest.chars().next().map_or(0, char::len_utf8),
            end => end,
        };
        let word = &rest[..end];
        let prefixed =
            word.len() >= 16 && TOKEN_PREFIXES.iter().any(|prefix| word.starts_with(prefix));
        if prefixed || led {
            // The whole value, up to the first character no token holds; a sentence's full
            // stop stays outside it. Only a lead or a prefix starts this scan, and a redacted
            // value is skipped, so each character is still read a bounded number of times.
            let value_end = rest[..rest.find(|c: char| !value_char(c)).unwrap_or(rest.len())]
                .trim_end_matches('.')
                .len();
            if prefixed || value_end >= 16 {
                out.push_str("<redacted>");
                rest = &rest[value_end..];
                led = false;
                continue;
            }
        }
        out.push_str(word);
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

    fn finding(kind: FindingKind, text: &str) -> Finding {
        Finding {
            kind,
            text: text.into(),
        }
    }

    #[test]
    fn only_bugs_and_risks_are_worth_an_issue() {
        let nits = [
            finding(FindingKind::Nit, "Rename x"),
            finding(FindingKind::Nit, "Reword y"),
        ];
        assert!(worth_filing(&nits).0.is_empty());
        let mixed = [
            finding(FindingKind::Nit, "Rename x"),
            finding(FindingKind::Bug, "Off by one in z"),
            finding(FindingKind::Nit, "Reword y"),
        ];
        let (filed, dropped) = worth_filing(&mixed);
        assert_eq!(filed, [&mixed[1]]);
        assert_eq!(dropped.len(), 2);
    }

    #[test]
    fn a_finding_reads_marked_or_plain() {
        let marked: Finding =
            serde_json::from_value(json!({"kind": "nit", "finding": "Rename x"})).unwrap();
        assert_eq!(marked, finding(FindingKind::Nit, "Rename x"));
        // A reviewer that still writes plain strings has them filed, not lost.
        let plain: Finding = serde_json::from_value(json!("Off by one")).unwrap();
        assert_eq!(plain, finding(FindingKind::Risk, "Off by one"));
        assert!(
            serde_json::from_value::<Finding>(json!({"kind": "idea", "finding": "x"})).is_err()
        );
    }

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
    fn quoted_dotted_and_base64_values_go_whole() {
        // Built at run time, so secret scanners don't take the test's fake tokens for real ones.
        let part = "d4".repeat(10);
        let jwt = format!("eyJ{part}.{part}.{part}");
        let base64 = format!("{part}+{part}/{part}==");
        let slashed = format!("/{part}+{part}");
        let text = format!(
            "\"token\": \"{part}\", password=\"{part}\", token, {part}, Bearer {jwt}. \
             secret {base64} and {jwt} alone; a token. Then the end. password: \"{slashed}\", \
             token `{part}`, token +ab and token /x."
        );
        assert_eq!(
            redact_tokens(&text),
            "\"token\": \"<redacted>\", password=\"<redacted>\", token, <redacted>, Bearer \
             <redacted>. secret <redacted> and <redacted> alone; a token. Then the end. \
             password: \"<redacted>\", token `<redacted>`, token +ab and token /x."
        );
    }

    #[test]
    fn windows_paths_match_in_any_case() {
        let home = Path::new(r"C:\Users\Me");
        let places = [(home, "<home>"), (Path::new("/home/Me"), "<home>")];
        let shown = redact(
            r"c:\users\me\.paja, C:/USERS/ME/x, C:\Users\Meg and /home/me/y",
            &places,
        );
        // Unix paths keep their case: `/home/me` is another directory than `/home/Me`.
        assert_eq!(
            shown,
            r"<home>\.paja, <home>/x, C:\Users\Meg and /home/me/y"
        );
    }

    #[test]
    fn redacting_a_long_finding_stays_fast() {
        let text = "word ".repeat(200_000);
        let started = std::time::Instant::now();
        assert_eq!(redact_tokens(&text), text);
        // Dotted runs after a lead word stay linear too.
        let dotted = format!("token {}", "a.".repeat(200_000));
        assert_eq!(redact_tokens(&dotted), "token <redacted>.");
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
