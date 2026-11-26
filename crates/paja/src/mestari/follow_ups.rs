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

/// Tokens that look like GitHub, Anthropic, AWS or bearer credentials become `<redacted>`, and
/// each path in `places` (and anything under it) becomes its label. Longer paths go first, so
/// a worktree under the factory home is named as the factory home, not cut in half.
pub(super) fn redact(text: &str, places: &[(&Path, &str)]) -> String {
    let mut places: Vec<(String, &str)> = places
        .iter()
        .flat_map(|(path, label)| {
            let shown = path
                .to_string_lossy()
                .trim_end_matches(['/', '\\'])
                .to_string();
            // Both separators, since a finding may quote either spelling of a Windows path.
            [shown.replace('\\', "/"), shown.replace('/', "\\"), shown]
                .into_iter()
                .map(move |spelling| (spelling, *label))
        })
        // A path of one or two characters (`/`, `C:`) would redact far too much.
        .filter(|(spelling, _)| spelling.len() > 2)
        .collect();
    places.sort_by_key(|(spelling, _)| std::cmp::Reverse(spelling.len()));
    places.dedup();
    let mut text = text.to_string();
    for (spelling, label) in &places {
        text = text.replace(spelling.as_str(), label);
    }
    redact_tokens(&text)
}

/// Prefixes of credentials that are safe to recognise by their start.
const TOKEN_PREFIXES: &[&str] = &[
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "sk-ant-",
    "AKIA",
    "xoxb-",
    "xoxp-",
];

fn redact_tokens(text: &str) -> String {
    let token_char = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        // The next word: a run of token characters.
        let start = rest.find(token_char).unwrap_or(rest.len());
        out.push_str(&rest[..start]);
        rest = &rest[start..];
        let end = rest.find(|c: char| !token_char(c)).unwrap_or(rest.len());
        let word = &rest[..end];
        let before = out.trim_end().to_ascii_lowercase();
        let secret = (word.len() >= 16 && TOKEN_PREFIXES.iter().any(|p| word.starts_with(p)))
            || (word.len() >= 16 && before.ends_with("bearer"));
        out.push_str(if secret { "<redacted>" } else { word });
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
        let text = "See /home/me/.paja/envs/work-3/src/a.rs and /home/me/src/paja/b.rs, \
                    /home/me/notes, token ghp_ABCDEFGHIJKLMNOPQRSTuvwx and \
                    Authorization: Bearer abcdefghijklmnopqrstuvwxyz. Keep ghp_ and #12.";
        let shown = redact(text, &places);
        assert_eq!(
            shown,
            "See <factory home>/envs/work-3/src/a.rs and <repository>/b.rs, <home>/notes, \
             token <redacted> and Authorization: Bearer <redacted>. Keep ghp_ and #12."
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
