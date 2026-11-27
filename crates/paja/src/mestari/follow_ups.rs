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

/// `text` made fit for a public issue comment: tokens redacted, and the repository's and the
/// user's home paths (the factory home is under the latter by default) named, not shown.
pub(super) fn redact_for_issue(text: &str, repo: &Path) -> String {
    let user_home = ["HOME", "USERPROFILE"]
        .iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    let mut places = vec![(repo, "<repository>")];
    places.extend(user_home.iter().map(|dir| (dir.as_path(), "<home>")));
    redact(text, &places)
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
    let mut spellings: Vec<(String, &str, bool)> = Vec::new();
    for (path, label) in places {
        let shown = spelling(path);
        // A `~/...` form under a Windows home is as case-insensitive as the home itself.
        let fold = is_windows_path(&shown);
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
                    spellings.push((each, *label, fold));
                }
            }
        }
    }
    spellings.sort_by_key(|(spelling, _, _)| std::cmp::Reverse(spelling.len()));
    spellings.dedup();
    let mut text = text.to_string();
    for (spelling, label, fold) in &spellings {
        text = replace_path(&text, spelling, label, *fold);
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

/// The end of `needle` if `text` starts with it; with `fold`, letters match in any case (Unicode
/// included, char by char, so byte offsets stay those of `text`).
fn match_at(text: &str, needle: &str, fold: bool) -> Option<usize> {
    let mut chars = text.char_indices();
    for n in needle.chars() {
        let (_, t) = chars.next()?;
        if t != n && !(fold && t.to_lowercase().eq(n.to_lowercase())) {
            return None;
        }
    }
    Some(chars.next().map_or(text.len(), |(i, _)| i))
}

/// `text` with each `path` that ends at a boundary (anything but a letter or digit) replaced by
/// `label`. With `fold`, the path matches in any letter case, as Windows itself does.
fn replace_path(text: &str, path: &str, label: &str, fold: bool) -> String {
    let mut out = String::with_capacity(text.len());
    let mut from = 0;
    let mut at = 0;
    while at < text.len() {
        let Some(len) = match_at(&text[at..], path, fold) else {
            at += text[at..].chars().next().map_or(1, char::len_utf8);
            continue;
        };
        let end = at + len;
        // Only a letter or digit continues a name: `_` and `-` may be Markdown or punctuation
        // around the path, so a path before one is still redacted.
        let ends = !text[end..].starts_with(char::is_alphanumeric);
        out.push_str(&text[from..at]);
        out.push_str(if ends { label } else { &text[at..end] });
        from = end;
        at = end;
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

/// Leads that name a secret whatever follows: after one, even a short value is redacted. After
/// the others (`token`, `bearer`) a short value needs a `:`, `=` or a quote before it, since
/// "a token of ours" is prose.
const STRONG_LEADS: &[&str] = &["password", "secret", "apikey", "api_key"];

/// Whether `word` leads a credential, and if so whether it is a strong lead. A compound name
/// leads too when it ends in a lead after `_` or `-`: `access_token`, `GITHUB_TOKEN`.
fn lead_strength(word: &str) -> Option<bool> {
    if word.len() > 64 {
        return None;
    }
    let lower = word.to_ascii_lowercase();
    TOKEN_LEADS.iter().find_map(|lead| {
        let named = lower == *lead
            || lower
                .strip_suffix(lead)
                .is_some_and(|head| head.ends_with(['_', '-']));
        named.then(|| STRONG_LEADS.contains(lead))
    })
}

/// A JSON Web Token however short: `eyJ`, then three non-empty dotted parts.
fn is_jwt(value: &str) -> bool {
    value.starts_with("eyJ")
        && value.split('.').count() == 3
        && value.split('.').all(|p| !p.is_empty())
}

/// Whether `gap`, the text between a lead and the next word, ends the lead for a short value:
/// an empty quoted value (`"password": "", "user"`) or a blank line (a new paragraph). A long
/// value after the lead is redacted all the same. A pair of quotes counts only as a whole value:
/// not part of a longer run (`"""`), and followed in the gap by a space, `,` or `;`. Backticks
/// never count, since ``` `` x `` ``` opens a Markdown code span.
fn ends_lead(gap: &str) -> bool {
    let chars: Vec<char> = gap.chars().collect();
    let empty_value = (0..chars.len().saturating_sub(2)).any(|i| {
        let q = chars[i];
        matches!(q, '"' | '\'')
            && chars[i + 1] == q
            && (i == 0 || chars[i - 1] != q)
            && (chars[i + 2].is_whitespace() || matches!(chars[i + 2], ',' | ';'))
    });
    // A blank line: a line between two breaks with nothing but spaces on it. A line holding a
    // Markdown fence or a `:` is not blank, so a fenced value after `password:` still goes.
    let lines: Vec<&str> = gap.split('\n').collect();
    let blank = lines.len() > 2
        && lines[1..lines.len() - 1]
            .iter()
            .any(|l| l.trim().is_empty());
    empty_value || blank
}

/// The scan visits each word once, so its cost is linear in the text. A redacted value runs on
/// through `.`, `+`, `/` and `=`, so a dotted token (a JWT) or a base64 secret goes whole; after
/// a lead it runs to the next space, quote, backtick, comma or backslash, so symbols stay inside.
fn redact_tokens(text: &str) -> String {
    let token_char = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    let value_char = |c: char| token_char(c) || matches!(c, '.' | '+' | '/' | '=');
    let gap_char =
        |c: char| c.is_whitespace() || matches!(c, ':' | '=' | ',' | '"' | '\'' | '`' | '\\');
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    // Whether the previous word leads a credential (and whether strongly) and only spaces,
    // quotes (escaped or not), backticks, `:`, `=` or `,` came since.
    let mut led: Option<bool> = None;
    // Whether a `:`, `=` or quote came between the lead and now.
    let mut delimited = false;
    // Whether an empty value or a blank line came since the lead: then only a long value counts.
    let mut ended = false;
    while !rest.is_empty() {
        let start = if led.is_some() {
            rest.find(|c: char| !gap_char(c))
        } else {
            rest.find(token_char)
        }
        .unwrap_or(rest.len());
        let gap = &rest[..start];
        out.push_str(gap);
        if led.is_some() && ends_lead(gap) {
            ended = true;
        }
        delimited |= gap
            .chars()
            .any(|c| matches!(c, ':' | '=' | '"' | '\'' | '`'));
        rest = &rest[start..];
        if rest.is_empty() {
            break;
        }
        if let Some(strong) = led {
            // Only a lead starts this scan, and a redacted value is skipped, so each character
            // is still read a bounded number of times.
            let run_end = rest
                .find(|c: char| c.is_whitespace() || matches!(c, ',' | '"' | '\'' | '`' | '\\'))
                .unwrap_or(rest.len());
            let value = rest[..run_end].trim_end_matches(['.', ';', ')', ']', '}']);
            let starts_plain = value.starts_with(char::is_alphanumeric);
            // With nothing between, a strong lead's short value counts only when it isn't a
            // plain lowercase word: "password hunter2" is a secret, "password field" is prose.
            let wordlike = value.chars().all(|c| c.is_ascii_lowercase());
            let short_counts = !ended && starts_plain && (delimited || (strong && !wordlike));
            if value.len() >= 16 || short_counts {
                out.push_str("<redacted>");
                rest = &rest[value.len()..];
                led = None;
                continue;
            }
        }
        // A short value that starts with a symbol is left as it is, one character at a time.
        let end = match rest.find(|c: char| !token_char(c)).unwrap_or(rest.len()) {
            0 => rest.chars().next().map_or(0, char::len_utf8),
            end => end,
        };
        let word = &rest[..end];
        let prefixed = word.len() >= 16 && TOKEN_PREFIXES.iter().any(|p| word.starts_with(p));
        let value_end = if prefixed || word.starts_with("eyJ") {
            // The whole value, up to the first character no token holds; a sentence's full
            // stop stays outside it.
            rest[..rest.find(|c: char| !value_char(c)).unwrap_or(rest.len())]
                .trim_end_matches('.')
                .len()
        } else {
            0
        };
        if prefixed || (value_end > 0 && is_jwt(&rest[..value_end])) {
            out.push_str("<redacted>");
            rest = &rest[value_end..];
            led = None;
            continue;
        }
        out.push_str(word);
        led = lead_strength(word);
        delimited = false;
        ended = false;
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
    fn more_shapes_of_secrets_go() {
        let long = "e5".repeat(10);
        let text = format!(
            r#"token=\"{long}\" "access_token": "{long}" GITHUB_TOKEN={long} client_secret={long} password: Abc$defghijklmnopqrstuvw password: a$b password hunter2 token=abc eyJhIjoxfQ.{long}.{long} a token of ours"#
        );
        assert_eq!(
            redact_tokens(&text),
            r#"token=\"<redacted>\" "access_token": "<redacted>" GITHUB_TOKEN=<redacted> client_secret=<redacted> password: <redacted> password: <redacted> password <redacted> token=<redacted> <redacted> a token of ours"#
        );
    }

    #[test]
    fn a_lead_before_prose_or_an_empty_value_redacts_nothing() {
        // Built at run time, so secret scanners don't take the test's fake values for real ones.
        let long = "f6".repeat(10);
        let (short, mixed) = (["Abc", "12"].concat(), ["My", "Pass"].concat());
        let text = format!(
            "the password field is empty; secret scanners flag it; \"password\": \"\", \"user\": \
             \"bob\"; a secret\nmanager; the password.\n\nNext paragraph; password: {short}; \
             secret {mixed}; api_key:\n  {long}; password:\n  {short}; secret\n\nFinally"
        );
        assert_eq!(
            redact_tokens(&text),
            "the password field is empty; secret scanners flag it; \"password\": \"\", \"user\": \
             \"bob\"; a secret\nmanager; the password.\n\nNext paragraph; password: <redacted>; \
             secret <redacted>; api_key:\n  <redacted>; password:\n  <redacted>; secret\n\nFinally"
        );
        // A value that opens with a run of quotes or backticks is still a value, not an empty
        // one, padded Markdown code spans included.
        let text = format!(
            "password = \"\"\"{short}\"\"\" and password: ``{short}`` and password: `` {short} `` x"
        );
        assert_eq!(
            redact_tokens(&text),
            "password = \"\"\"<redacted>\"\"\" and password: ``<redacted>`` and password: `` \
             <redacted> `` x"
        );
        // A fenced value after a lead still goes: a fence line is not a blank line.
        let text = format!("Set the password:\n```\n{short}\n```");
        assert_eq!(
            redact_tokens(&text),
            "Set the password:\n```\n<redacted>\n```"
        );
        // A line break alone doesn't end a lead: a shell line continuation, or a value on the
        // next line, still goes. After an empty value or a blank line a long value still goes.
        let text = format!(
            "login --password \\\n  {short} then token\n{long} then password: \"\", {long} and \
             secret\n\n{long}"
        );
        assert_eq!(
            redact_tokens(&text),
            "login --password \\\n  <redacted> then token\n<redacted> then password: \"\", \
             <redacted> and secret\n\n<redacted>"
        );
        // Known gap, taken on purpose: a short all-lowercase word right after a lead, with
        // nothing between, reads as prose ("the password field"), so it stays.
        assert_eq!(redact_tokens("password hunter"), "password hunter");
    }

    #[test]
    fn windows_paths_fold_unicode_and_tilde_forms() {
        let home = Path::new(r"C:\Users\Jürgen");
        let proj = Path::new(r"C:\Users\Jürgen\src\proj");
        let places = [(proj, "<repository>"), (home, "<home>")];
        let shown = redact(r"C:\USERS\JÜRGEN\x and ~/SRC/PROJ/y", &places);
        assert_eq!(shown, r"<home>\x and <repository>/y");
    }

    #[test]
    fn an_issue_comment_names_the_repository_and_hides_tokens() {
        let repo = Path::new("/work/acme-app");
        let token = format!("ghp_{}", "A1".repeat(12));
        let text = format!("test failed in /work/acme-app/src/lib.rs with {token}");
        let shown = redact_for_issue(&text, repo);
        assert_eq!(
            shown,
            "test failed in <repository>/src/lib.rs with <redacted>"
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
