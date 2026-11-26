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
/// quotes, `:`, `=` or `,`, is redacted whatever it looks like.
const TOKEN_LEADS: &[&str] = &["bearer", "token", "password", "secret", "apikey", "api_key"];

/// The scan visits each word once, so its cost is linear in the text. A redacted value runs on
/// through `.`, `+`, `/` and `=`, so a dotted token (a JWT) or a base64 secret goes whole.
fn redact_tokens(text: &str) -> String {
    let token_char = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    let value_char = |c: char| token_char(c) || matches!(c, '.' | '+' | '/' | '=');
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    // Whether the previous word leads a credential and only spaces, quotes, `:`, `=` or `,`
    // came since.
    let mut led = false;
    while !rest.is_empty() {
        let start = rest.find(token_char).unwrap_or(rest.len());
        let gap = &rest[..start];
        out.push_str(gap);
        if !gap
            .chars()
            .all(|c| c.is_whitespace() || matches!(c, ':' | '=' | ',' | '"' | '\''))
        {
            led = false;
        }
        rest = &rest[start..];
        if rest.is_empty() {
            break;
        }
        let end = rest.find(|c: char| !token_char(c)).unwrap_or(rest.len());
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
        let text = format!(
            "\"token\": \"{part}\", password=\"{part}\", token, {part}, Bearer {jwt}. \
             secret {base64} and {jwt} alone; a token. Then the end."
        );
        assert_eq!(
            redact_tokens(&text),
            "\"token\": \"<redacted>\", password=\"<redacted>\", token, <redacted>, Bearer \
             <redacted>. secret <redacted> and <redacted> alone; a token. Then the end."
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
