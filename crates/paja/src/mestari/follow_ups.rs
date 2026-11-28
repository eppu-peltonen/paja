use super::*;

/// What kind of finding a reviewer reports beside its verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    /// A wrong result in the change that is not a blocker.
    Bug,
    /// A security or data-loss risk, or a missing test for behaviour that could break.
    Risk,
    /// Style, naming, comment wording or a small refactor.
    Nit,
    /// A real problem in code the change doesn't touch: not fixed here, since one concern goes
    /// in each pull request. Recorded in the log and the pull request's description.
    Outside,
}

/// One non-blocking review finding. A plain string, as reviewers wrote them before findings
/// were marked, reads as a `risk`, so it is still fixed rather than lost.
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
            FindingKind::Outside => "outside",
        };
        format!("[{kind}] {}", self.text)
    }
}

/// The findings the builder is sent back to fix: everything but what lies outside the change.
pub(super) fn to_fix(findings: &[Finding]) -> Vec<&Finding> {
    findings
        .iter()
        .filter(|f| f.kind != FindingKind::Outside)
        .collect()
}

/// Why a finding in the change is left: it was still open at the round limit.
pub(super) const ROUND_LIMIT: &str = "not filed: round limit";

/// The findings a change lands without, each as text fit for a pull request's description and
/// the log, with why it is left: outside the change, or still open at the round limit. No issue
/// is filed: a finding is fixed in the change that drew it, and what is left is for a person to
/// read in the pull request.
pub(super) fn unsettled(
    home: &Home,
    repo: &Path,
    findings: &[Finding],
) -> Vec<(String, &'static str)> {
    // Review text came from an agent that read untrusted code: keep each finding short, never
    // let it mention anyone, and keep tokens and local paths out of a public pull request and
    // the log.
    let user_home = user_homes();
    let places = public_places(home.root(), repo, &user_home);
    findings
        .iter()
        .map(|finding| {
            // Mentions are made inert before the cut, so a cut can't leave a bare `@` behind.
            let text = redact(&finding.shown(), &places).replace('@', "@\u{200b}");
            let mut cut: String = text.chars().take(4000).collect();
            if cut.len() < text.len() {
                cut.push_str("\n\n(cut short)");
            }
            let why = if finding.kind == FindingKind::Outside {
                "not filed: outside the change"
            } else {
                ROUND_LIMIT
            };
            (cut, why)
        })
        .collect()
}

/// The user's home folders (`HOME`, and `USERPROFILE` on Windows), which a public text names
/// as `<home>`.
fn user_homes() -> Vec<PathBuf> {
    ["HOME", "USERPROFILE"]
        .iter()
        .filter_map(std::env::var_os)
        .map(PathBuf::from)
        .collect()
}

/// The places a text Paja posts on a public issue names instead of showing: the factory home,
/// the repository and the user's home folders.
fn public_places<'a>(
    factory_home: &'a Path,
    repo: &'a Path,
    user_home: &'a [PathBuf],
) -> Vec<(&'a Path, &'a str)> {
    let mut places = vec![(factory_home, "<factory home>"), (repo, "<repository>")];
    places.extend(user_home.iter().map(|dir| (dir.as_path(), "<home>")));
    places
}

/// `text` made fit for a comment on a public issue: tokens redacted, the factory home, the
/// repository and the user's home named, not shown, mentions made inert so a quoted log or
/// review can't ping anyone, code fences made plain text, and cut to `max` characters. All
/// that is done first, so the cut counts the characters it adds. For the parts of a comment
/// that quote agent text or logs, so the frame around them (how to answer, how to undo) is
/// never what gets cut, and never renders as code.
pub(crate) fn redact_for_comment(
    text: &str,
    repo: &Path,
    factory_home: &Path,
    max: usize,
) -> String {
    let user_home = user_homes();
    // `<!--` would hide the rest of the comment, the frame's instructions too.
    let text = redact(text, &public_places(factory_home, repo, &user_home))
        .replace('@', "@\u{200b}")
        .replace("<!--", "<\u{200b}!--");
    let text = without_fences(&text);
    let count = text.chars().count();
    if count <= max {
        return text;
    }
    // The middle goes: a question's start says what it is about, and its end often holds the
    // root cause and what to do.
    let head: String = text.chars().take(max / 2).collect();
    let tail: String = text.chars().skip(count - max / 2).collect();
    // The tail can start mid-line, just before backticks that were not at a line's start.
    format!("{head}\n\n(cut short)\n\n{}", without_fences(&tail))
}

/// `text` with every line that would open or close a code fence (`` ``` `` or `~~~`, of any
/// length, after any indent) broken by a zero-width space, so quoted text can't leave a fence
/// open over what follows it, however it is cut.
fn without_fences(text: &str) -> String {
    text.split('\n')
        .map(|line| {
            let indent = line.len() - line.trim_start().len();
            let rest = &line[indent..];
            if rest.starts_with("```") || rest.starts_with("~~~") {
                format!("{}{}\u{200b}{}", &line[..indent], &rest[..1], &rest[1..])
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The most of one quoted part (a question, an answer, a reason) a comment shows: two or three
/// fit GitHub's limit with the frame around them.
pub(crate) const QUOTED_CHARS: usize = 20_000;

/// Write `body` to a new file of its own in the factory home's `tmp`, out of the way of its
/// state, for a `gh ... --body-file`: a long body would not fit a Windows command line. The name
/// is unique to this call (the process id and a counter), and the file is created new, so two
/// posts at once, in one process or two, never share one. The caller removes it.
pub(crate) fn body_file(home: &Home, stem: &str, body: &str) -> Result<PathBuf> {
    use std::io::Write as _;
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = home.root().join("tmp");
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = dir.join(format!("{stem}-{}-{n}.md", std::process::id()));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("creating {}", path.display()))?;
    if let Err(err) = file.write_all(body.as_bytes()) {
        drop(file);
        let _ = std::fs::remove_file(&path);
        return Err(err).with_context(|| format!("writing {}", path.display()));
    }
    Ok(path)
}

/// GitHub's limit for a comment, in characters.
const GITHUB_COMMENT_CHARS: usize = 65_536;

// Three cut parts and a frame fit a GitHub comment.
const _: () = assert!(3 * (QUOTED_CHARS + 30) + 1_000 < GITHUB_COMMENT_CHARS);

/// Post `body`, whose quoted parts went through [`redact_for_comment`], as a comment on issue
/// `number`, from a [`body_file`] removed once `gh` is done. A body past GitHub's limit is cut
/// to it, so the comment still posts.
pub(super) fn comment_on_issue(home: &Home, repo: &Path, number: u64, body: &str) -> Result<()> {
    // Never reached while each comment quotes at most three parts of `QUOTED_CHARS`; a check
    // at build time holds that sum under the limit.
    let body: String = body.chars().take(GITHUB_COMMENT_CHARS).collect();
    let file = body_file(home, &format!("comment-{number}"), &body)?;
    let posted = run_gh(
        repo,
        &[
            "issue",
            "comment",
            &number.to_string(),
            "--body-file",
            &file.to_string_lossy(),
        ],
    );
    let _ = std::fs::remove_file(&file);
    posted.map(|_| ())
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
/// `label`. With `fold`, the path matches in any letter case, as Windows itself does: each
/// character of the text against the path's, by its lowercase form (so `İ` or the Kelvin sign
/// fold as Unicode says, not only ASCII letters).
///
/// Its cost is linear in the text: matches are found with `str::find`, in a lowercased copy
/// when folding, whose offsets are mapped back to the text's, and each is confirmed character by
/// character.
fn replace_path(text: &str, path: &str, label: &str, fold: bool) -> String {
    let matches = if fold {
        folded_matches(text, path)
    } else {
        text.match_indices(path)
            .map(|(at, found)| (at, found.len()))
            .collect()
    };
    let mut out = String::with_capacity(text.len());
    let mut from = 0;
    for (start, len) in matches {
        let end = start + len;
        // Only a letter or digit continues a name: `_` and `-` may be Markdown or punctuation
        // around the path, so a path before one is still redacted.
        let ends = !text[end..].starts_with(char::is_alphanumeric);
        out.push_str(&text[from..start]);
        out.push_str(if ends { label } else { &text[start..end] });
        from = end;
    }
    out.push_str(&text[from..]);
    out
}

/// Where `path` is in `text`, letters in any case, left to right and not overlapping: each
/// match's start and length in `text`.
fn folded_matches(text: &str, path: &str) -> Vec<(usize, usize)> {
    let needle: String = path.chars().flat_map(char::to_lowercase).collect();
    if needle.is_empty() {
        return Vec::new();
    }
    // The lowercased text, and for each of its offsets that starts a character of the text,
    // that character's offset in `text` (`NONE` elsewhere); and back from `text`'s offsets.
    const NONE: usize = usize::MAX;
    let mut folded = String::with_capacity(text.len());
    let mut to_text = Vec::with_capacity(text.len() + 1);
    let mut to_folded = vec![NONE; text.len() + 1];
    for (at, c) in text.char_indices() {
        to_folded[at] = folded.len();
        let before = folded.len();
        folded.extend(c.to_lowercase());
        to_text.push(at);
        to_text.resize(folded.len(), NONE);
        debug_assert!(folded.len() > before);
    }
    to_text.push(text.len());
    to_folded[text.len()] = folded.len();
    let mut found = Vec::new();
    let mut from = 0;
    while let Some(i) = folded[from..].find(&needle) {
        let at = from + i;
        let confirmed = match to_text[at] {
            NONE => None,
            start => match_at(&text[start..], path, true).map(|len| (start, len)),
        };
        match confirmed {
            Some((start, len)) => {
                found.push((start, len));
                from = to_folded[start + len];
            }
            None => from = at + folded[at..].chars().next().map_or(1, char::len_utf8),
        }
    }
    found
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
/// an empty quoted value (`"password": "", "user"`). A long value after the lead is redacted all
/// the same. A pair of quotes counts only as a whole value: not part of a longer run (`"""`),
/// and followed in the gap by a space, `,` or `;`. Backticks never count, since ``` `` x `` ```
/// opens a Markdown code span. Line breaks never count, blank lines included: a value often sits
/// after a blank line or in a fence, and redacting a word of prose is better than leaking one.
fn ends_lead(gap: &str) -> bool {
    let chars: Vec<char> = gap.chars().collect();
    (0..chars.len().saturating_sub(2)).any(|i| {
        let q = chars[i];
        matches!(q, '"' | '\'')
            && chars[i + 1] == q
            // Right after the lead it closes the key itself (`''password'', ''x''`).
            && i > 0
            && chars[i - 1] != q
            && (chars[i + 2].is_whitespace() || matches!(chars[i + 2], ',' | ';'))
    })
}

/// Whether `rest`, after `gap`, is the info string of a code fence opening on its own line: the
/// gap's last line is three or more backticks (after spaces) and the line's rest holds no
/// backtick (else it is a code span), or the line starts with three or more tildes.
fn opens_fence(gap: &str, rest: &str) -> bool {
    let Some(at) = gap.rfind('\n') else {
        return false;
    };
    let tail = gap[at + 1..].trim_start_matches(' ');
    let line = &rest[..rest.find('\n').unwrap_or(rest.len())];
    let backticks = tail.len() >= 3 && tail.chars().all(|c| c == '`') && !line.contains('`');
    let tildes = tail.is_empty() && line.starts_with("~~~");
    backticks || tildes
}

/// Whether `word` reads as a fence's language name (`text`, `sh`, `c++`, `objective-c`): up to
/// 15 lowercase ASCII letters and `+`, `#`, `-` or `.`, with no digit, so a value written after
/// the fence (`hunter2`, a random string) isn't taken for one. `python3` is redacted, which is
/// the side to err on.
fn language_tag(word: &str) -> bool {
    word.len() <= 15
        && word.starts_with(|c: char| c.is_ascii_lowercase())
        && word
            .chars()
            .all(|c| c.is_ascii_lowercase() || matches!(c, '+' | '#' | '-' | '.'))
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
    // Whether an empty value came since the lead: then only a long value counts.
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
        // A fence opened after a lead: its info string (```` ```text ````, `~~~ sh`) names the
        // language, and the value is on the lines after it, so the lead carries on past the
        // line. A first word that doesn't look like a language name may be the value written
        // on the fence's own line, so it goes; the rest of the line is scanned on its own.
        if led.is_some() && opens_fence(gap, rest) {
            let line = &rest[..rest.find('\n').unwrap_or(rest.len())];
            let tag_at = line.len() - line.trim_start_matches(['~', ' ', '\t']).len();
            let tag_end = line[tag_at..]
                .find(char::is_whitespace)
                .map_or(line.len(), |i| tag_at + i);
            let tag = &line[tag_at..tag_end];
            out.push_str(&line[..tag_at]);
            out.push_str(if tag.is_empty() || language_tag(tag) {
                tag
            } else {
                "<redacted>"
            });
            out.push_str(&redact_tokens(&line[tag_end..]));
            rest = &rest[line.len()..];
            delimited = true;
            continue;
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
    fn every_finding_in_the_change_is_fixed_and_only_outside_ones_are_left() {
        let mixed = [
            finding(FindingKind::Nit, "Rename x"),
            finding(FindingKind::Outside, "Off by one in z"),
            finding(FindingKind::Risk, "No test for y"),
        ];
        assert_eq!(to_fix(&mixed), [&mixed[0], &mixed[2]]);
    }

    #[test]
    fn a_finding_reads_marked_or_plain() {
        let marked: Finding =
            serde_json::from_value(json!({"kind": "nit", "finding": "Rename x"})).unwrap();
        assert_eq!(marked, finding(FindingKind::Nit, "Rename x"));
        // A reviewer that still writes plain strings has them fixed, not lost.
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
             secret {mixed}; api_key:\n  {long}; password:\n  {short}; secret\n\nfinally"
        );
        assert_eq!(
            redact_tokens(&text),
            "the password field is empty; secret scanners flag it; \"password\": \"\", \"user\": \
             \"bob\"; a secret\nmanager; the password.\n\nNext paragraph; password: <redacted>; \
             secret <redacted>; api_key:\n  <redacted>; password:\n  <redacted>; secret\n\nfinally"
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
        // A fenced value after a lead still goes, blank lines before or inside the fence too.
        let text = format!(
            "Set the password:\n```\n{short}\n```\nThe password:\n\n```\n{short}\n```\n\
             password:\n```\n\n{short}\n```"
        );
        assert_eq!(
            redact_tokens(&text),
            "Set the password:\n```\n<redacted>\n```\nThe password:\n\n```\n<redacted>\n```\n\
             password:\n```\n\n<redacted>\n```"
        );
        // A line break alone doesn't end a lead: a shell line continuation, or a value on the
        // next line, still goes, as does one after a blank line. After an empty value a long value still goes.
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
        let shown = redact_for_comment(&text, repo, Path::new("/srv/paja-home"), 1_000);
        assert_eq!(
            shown,
            "test failed in <repository>/src/lib.rs with <redacted>"
        );
    }

    #[test]
    fn an_escalation_comment_names_the_factory_home_and_mentions_no_one() {
        let repo = Path::new("/work/acme-app");
        let factory = Path::new("/srv/paja-home");
        let text = "see /srv/paja-home/envs/app-1/log and /work/acme-app/x, cc @octocat";
        assert_eq!(
            redact_for_comment(text, repo, factory, 100),
            "see <factory home>/envs/app-1/log and <repository>/x, cc @\u{200b}octocat"
        );
        // Inert mentions count toward the cut, so a log full of them still fits a comment.
        let mentions = redact_for_comment(&"@".repeat(65_536), repo, factory, 1_000);
        assert!(mentions.chars().count() <= 1_020, "too long");
        // The middle is cut: the start and the end stay.
        let long = format!("start\n{}\nend", "a".repeat(2_000));
        let shown = redact_for_comment(&long, repo, factory, 1_000);
        assert!(shown.starts_with("start\n"), "{shown}");
        assert!(shown.ends_with("\nend"), "{shown}");
        assert!(shown.contains("\n\n(cut short)\n\n"), "{shown}");
        assert!(shown.chars().count() <= 1_030, "too long");
        // No line of quoted text opens a fence, of any kind or length, so however it is cut
        // the frame after it never renders as code. Inline backticks stay.
        for fence in ["```", "````rust", "  ~~~", "~~~~"] {
            let shown = redact_for_comment(&format!("x\n{fence}\ny"), repo, factory, 100);
            assert!(
                !shown
                    .lines()
                    .any(|l| l.trim_start().starts_with("``") || l.trim_start().starts_with("~~")),
                "{shown:?}"
            );
        }
        // A cut that lands just before backticks in the middle of a line doesn't open one.
        let aimed = format!("{}x```yz", "a".repeat(1_000));
        let shown = redact_for_comment(&aimed, repo, factory, 10);
        assert!(!shown.lines().any(|l| l.starts_with("``")), "{shown:?}");
        assert_eq!(
            redact_for_comment("run `cargo test` ```now```", repo, factory, 100),
            "run `cargo test` ```now```"
        );
        // A comment marker can't hide the frame.
        assert!(!redact_for_comment("a <!-- b", repo, factory, 100).contains("<!--"));
    }

    #[test]
    fn a_fence_info_string_after_a_lead_is_not_the_value() {
        let short = "hunter2x";
        let text = format!(
            "password:\n```text\n{short}\n```\nsecret:\n  ~~~ sh\n{short}\n~~~\ntoken:\n```\n{short}\n```"
        );
        assert_eq!(
            redact_tokens(&text),
            "password:\n```text\n<redacted>\n```\nsecret:\n  ~~~ sh\n<redacted>\n~~~\ntoken:\n```\n\
             <redacted>\n```"
        );
        // A token in the info line still goes; a one-line code span is the value itself.
        let gh = format!("{}{}", "ghp_", "a".repeat(36));
        assert_eq!(
            redact_tokens(&format!("password:\n```{gh}\n{short}\n```")),
            "password:\n```<redacted>\n<redacted>\n```"
        );
        assert_eq!(
            redact_tokens(&format!("password:\n```text {gh}\n{short}\n```")),
            "password:\n```text <redacted>\n<redacted>\n```"
        );
        // A value written on the fence's own line isn't a language name, so it goes, short or
        // long, after backticks or tildes, and the lead still reaches the next line.
        let long = "AbCdEfGh12345678ZZ";
        for fence in ["```", "~~~", "~~~ "] {
            for value in [short, long, "python3"] {
                assert_eq!(
                    redact_tokens(&format!("password:\n{fence}{value}\n{short}\n")),
                    format!("password:\n{fence}<redacted>\n<redacted>\n"),
                    "{fence}{value}"
                );
            }
        }
        assert_eq!(
            redact_tokens(&format!("password:\n```{short}```")),
            "password:\n```<redacted>```"
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
    fn redacting_paths_in_a_long_text_stays_fast() {
        // A path that matches part of itself, over text that nearly matches it at every
        // place: a scan that compared the path at each character reads the text a hundred
        // times over, which takes many seconds here even in a release build.
        let path = format!("{}/b", "/a".repeat(100));
        let text = format!("{}{path} x", "/a".repeat(1_000_000));
        let started = std::time::Instant::now();
        let shown = replace_path(&text, &path, "<home>", false);
        assert!(shown.ends_with("<home> x"));
        let upper = text.to_uppercase();
        let shown = replace_path(&upper, &path, "<home>", true);
        assert!(shown.ends_with("<home> X"));
        // Other letters than ASCII ones fold the same linear way.
        let path = format!("{}/ö", "/ä".repeat(100));
        let text = format!("{}{} x", "/Ä".repeat(500_000), path.to_uppercase());
        let shown = replace_path(&text, &path, "<home>", true);
        assert!(shown.ends_with("<home> x"));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        // A folded path with other letters still matches in any case.
        assert_eq!(
            replace_path(r"C:\USERS\JÖHN\x", r"C:\Users\Jöhn", "<home>", true),
            r"<home>\x"
        );
        // Characters whose lowercase is longer (`İ`), before and after a folded path, keep
        // every offset right; the Kelvin sign folds to `k`, as before.
        assert_eq!(
            replace_path("İÄ C:\\USERS\\ME\\x İ end", r"C:\Users\me", "<home>", true),
            "İÄ <home>\\x İ end"
        );
        assert_eq!(
            replace_path("İC:\\USERS\\MİKE x", r"C:\Users\mike", "<home>", true),
            "İC:\\USERS\\MİKE x"
        );
        assert_eq!(
            replace_path(
                "at C:\\Users\\Mi\u{212A}e x",
                r"C:\Users\mike",
                "<home>",
                true
            ),
            "at <home> x"
        );
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
