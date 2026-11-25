//! What differs between Windows and Unix: the home directory, private files and directories,
//! the shell that runs Vasara's checks, and how a hook command line is quoted. Everything
//! else in Paja is the same on both.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

/// The owner's home directory, as git finds it: `HOME`, and on Windows, when that is unset or
/// Unix-style (`/c/Users/me` isn't absolute there), `HOMEDRIVE` plus `HOMEPATH`, then
/// `USERPROFILE`. On Unix any non-empty `HOME` counts, as it does for git.
pub fn home_dir() -> Option<PathBuf> {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
    if !cfg!(windows) {
        return var("HOME").map(PathBuf::from);
    }
    let drive_path = var("HOMEDRIVE").zip(var("HOMEPATH")).map(|(drive, path)| {
        let mut joined = drive;
        joined.push(path);
        joined
    });
    [var("HOME"), drive_path, var("USERPROFILE")]
        .into_iter()
        .flatten()
        .map(PathBuf::from)
        .find(|p| p.is_absolute())
}

/// Where Claude Code reads managed (administrator) settings on this platform.
pub fn managed_settings_dirs() -> Vec<PathBuf> {
    if cfg!(windows) {
        // Claude Code names these two literally; the environment's spelling is checked too, in
        // case it differs.
        let mut dirs = vec![
            PathBuf::from(r"C:\Program Files\ClaudeCode"),
            PathBuf::from(r"C:\ProgramData\ClaudeCode"),
        ];
        for var in ["ProgramFiles", "ProgramData"] {
            if let Some(dir) = std::env::var_os(var) {
                let dir = PathBuf::from(dir).join("ClaudeCode");
                if !dirs
                    .iter()
                    .any(|d| d.as_os_str().eq_ignore_ascii_case(dir.as_os_str()))
                {
                    dirs.push(dir);
                }
            }
        }
        dirs
    } else {
        vec![
            PathBuf::from("/etc/claude-code"),
            PathBuf::from("/Library/Application Support/ClaudeCode"),
        ]
    }
}

/// The running paja binary. On Linux `/proc/self/exe` still reads it after a rebuild replaced
/// the file on disk.
pub fn running_exe() -> std::io::Result<PathBuf> {
    let proc = Path::new("/proc/self/exe");
    if cfg!(target_os = "linux") && proc.exists() {
        return Ok(proc.to_path_buf());
    }
    std::env::current_exe()
}

/// Set `path`'s modification time to now, ignoring failure. The file may be a running binary,
/// which can't be opened for writing: Unix opens it read-only, Windows asks only for the right
/// to change its attributes.
pub fn touch(path: &Path) {
    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
        std::fs::OpenOptions::new()
            .access_mode(FILE_WRITE_ATTRIBUTES)
            .open(path)
    };
    #[cfg(not(windows))]
    let file = std::fs::File::open(path);
    if let Ok(file) = file {
        let _ = file.set_modified(std::time::SystemTime::now());
    }
}

/// Create `dir`, which must not exist yet, so that only this user can enter it: mode 0700 on
/// Unix, an owner-only ACL on Windows.
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(dir)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir(dir)?;
        // Until the ACL is set, the parent's applies: refuse anything planted in that gap.
        let restricted =
            restrict_to_owner(dir).and_then(|()| match std::fs::read_dir(dir)?.next() {
                None => Ok(()),
                Some(_) => Err(std::io::Error::other(format!(
                    "{} was not empty right after it was created; something else is writing \
                     there, so it can't be trusted",
                    dir.display()
                ))),
            });
        restricted.inspect_err(|_| {
            let _ = std::fs::remove_dir(dir);
        })
    }
}

/// On Windows, replace `dir`'s ACL with one that lets only the current user in, for it and
/// everything created in it, and check that no other account is left with access (`icacls`
/// keeps other accounts' explicit entries). A new directory otherwise inherits its parent's
/// ACL, which is not private under, say, `C:\paja`. Each directory is done once per process.
/// Unix needs nothing: Paja creates such directories 0700.
pub fn restrict_to_owner(dir: &Path) -> std::io::Result<()> {
    if !cfg!(windows) {
        return Ok(());
    }
    use std::sync::{Mutex, OnceLock};
    static DONE: OnceLock<Mutex<Vec<PathBuf>>> = OnceLock::new();
    let done = DONE.get_or_init(|| Mutex::new(Vec::new()));
    if done.lock().unwrap().iter().any(|d| d == dir) {
        return Ok(());
    }
    let failed = |what: String| std::io::Error::other(format!("{what} for {}", dir.display()));
    let (account, sid) = current_user()?;
    let icacls = || Command::new(system32().join("icacls.exe"));
    let status = icacls()
        .arg(dir)
        .args([
            "/inheritance:r",
            "/grant:r",
            &format!("*{sid}:(OI)(CI)F"),
            "/q",
        ])
        .stdout(std::process::Stdio::null())
        .status()?;
    // New directories also get explicit entries for SYSTEM and Administrators (who can take
    // any file anyway); drop them by SID, which every Windows language spells the same.
    let removed = icacls()
        .arg(dir)
        .args(["/remove:g", "*S-1-5-18", "*S-1-5-32-544", "/q"])
        .stdout(std::process::Stdio::null())
        .status()?;
    if !status.success() || !removed.success() {
        return Err(failed("`icacls` could not make an owner-only ACL".into()));
    }
    // `icacls <dir>` prints `<dir> ACCOUNT:(rights)` and then one `ACCOUNT:(rights)` per line.
    let listing = icacls().arg(dir).output()?;
    let text = String::from_utf8_lossy(&listing.stdout).replace(&*dir.to_string_lossy(), "");
    let others: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| line.contains(":("))
        .filter_map(|line| line.split(":(").next())
        .filter(|name| !name.eq_ignore_ascii_case(&account) && !name.eq_ignore_ascii_case(&sid))
        .collect();
    if !listing.status.success() || !others.is_empty() {
        return Err(failed(format!(
            "other accounts ({}) still have access; remove their entries with `icacls` or use \
             another PAJA_HOME",
            others.join(", ")
        )));
    }
    done.lock().unwrap().push(dir.to_path_buf());
    Ok(())
}

fn system32() -> PathBuf {
    let system = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    PathBuf::from(system).join("System32")
}

/// The current user's account name and SID, from `whoami /user /fo csv /nh`, which prints
/// `"DOMAIN\user","S-1-5-..."`.
fn current_user() -> std::io::Result<(String, String)> {
    use std::sync::OnceLock;
    static USER: OnceLock<(String, String)> = OnceLock::new();
    if let Some(user) = USER.get() {
        return Ok(user.clone());
    }
    let who = Command::new(system32().join("whoami.exe"))
        .args(["/user", "/fo", "csv", "/nh"])
        .output()?;
    let text = String::from_utf8_lossy(&who.stdout);
    let fields: Vec<String> = text
        .trim()
        .split("\",\"")
        .map(|f| f.trim_matches('"').to_string())
        .collect();
    match fields.as_slice() {
        [account, sid] if sid.starts_with("S-1-") => {
            Ok(USER.get_or_init(|| (account.clone(), sid.clone())).clone())
        }
        _ => Err(std::io::Error::other(format!(
            "`whoami /user` gave no account and SID (got {text:?})"
        ))),
    }
}

/// Create `path`, which must not exist yet, as a file only this user can read, write or run
/// (on Windows: inside a directory `create_private_dir` or `restrict_to_owner` made).
pub fn create_private_file(path: &Path) -> std::io::Result<std::fs::File> {
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o700);
    }
    options.open(path)
}

/// A command that runs `script` with a POSIX shell. On Windows that is Git for Windows' `sh`,
/// which Claude Code on Windows needs anyway, so checks and agents' commands share one shell.
pub fn shell(script: &str) -> Result<Command> {
    let mut cmd = Command::new(posix_shell()?);
    cmd.args(["-c", script]);
    Ok(cmd)
}

/// The POSIX shell: `sh` on Unix, Git for Windows' `sh.exe` on Windows.
pub fn posix_shell() -> Result<PathBuf> {
    if !cfg!(windows) {
        return Ok(PathBuf::from("sh"));
    }
    // Claude Code's own setting first: it names Git Bash's bash.exe, next to sh.exe.
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(bash) = std::env::var_os("CLAUDE_CODE_GIT_BASH_PATH") {
        let bash = PathBuf::from(bash);
        candidates.extend(bash.parent().map(|bin| bin.join("sh.exe")));
        candidates.push(bash);
    }
    // Then the Git for Windows that runs as `git`: <root>\cmd\git.exe or <root>\bin\git.exe.
    if let Some(git) = find_on_path("git.exe") {
        // Or <root>\mingw64\bin\git.exe, one level deeper.
        for root in git.ancestors().skip(2).take(2) {
            candidates.push(root.join("bin").join("sh.exe"));
            candidates.push(root.join("usr").join("bin").join("sh.exe"));
        }
    }
    candidates.extend(find_on_path("sh.exe"));
    // Only absolute paths: checks run in the worktree, where a relative one could name a shell
    // the branch under review committed.
    candidates
        .into_iter()
        .find(|path| path.is_absolute() && path.is_file())
        .context(
            "Vasara's checks need a POSIX shell, and none was found: install Git for Windows \
             (https://git-scm.com/downloads/win), which Claude Code on Windows needs too, or set \
             CLAUDE_CODE_GIT_BASH_PATH to its bash.exe",
        )
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

/// Where cargo and rustup look for their homes when `CARGO_HOME` and `RUSTUP_HOME` are unset:
/// `HOME` on Unix, the user profile on Windows (they don't read `HOME` there).
pub fn tool_home() -> Option<PathBuf> {
    if cfg!(windows) {
        std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .or_else(home_dir)
    } else {
        home_dir()
    }
}

/// A path written in a shell command, as the program it is passed to will see it. Git Bash
/// turns `/c/Users/me` into `C:\Users\me` for Windows programs; elsewhere a path is itself.
pub fn shell_path(word: &str) -> PathBuf {
    if cfg!(windows) {
        let mut chars = word.chars();
        if let (Some('/'), Some(drive), rest) = (chars.next(), chars.next(), chars.as_str()) {
            if drive.is_ascii_alphabetic() && (rest.is_empty() || rest.starts_with('/')) {
                return PathBuf::from(
                    format!(
                        "{}:\\{}",
                        drive.to_ascii_uppercase(),
                        rest.trim_start_matches('/')
                    )
                    .replace('/', "\\"),
                );
            }
        }
    }
    PathBuf::from(word)
}

/// Why the hook can't judge `text` as a path here, if it can't. On Windows the file system
/// reads some spellings differently from how they look: device and UNC paths (`\\?\`,
/// `\\server\share`), drive-relative paths (`C:foo`), paths rooted without a drive (`\x`,
/// which a tool may map to another drive or Git's own directory), names with a trailing dot
/// or space (`.git.` is `.git`), alternate data streams (`a:stream`, unless `colons` allows
/// colons, as in git's `HEAD:path`), reserved device names (`NUL`, `COM1`) and characters
/// Windows forbids in names. All are refused rather than interpreted. (An 8.3 short name such
/// as `GIT~1` only names something that exists, and the hook resolves existing paths to their
/// long names, so it needs no rule.)
/// Nothing is refused on Unix.
pub fn path_problem(text: &str, colons: bool) -> Option<String> {
    if !cfg!(windows) || text.is_empty() {
        return None;
    }
    let refuse = |why: &str| {
        Some(format!(
            "`{text}` {why}; give a plain path such as C:\\dir\\file or dir\\file"
        ))
    };
    let bytes = text.as_bytes();
    let sep = |b: u8| b == b'/' || b == b'\\';
    if sep(bytes[0]) && bytes.get(1).copied().is_some_and(sep) {
        return refuse("is a device or network path");
    }
    let drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    if drive && !bytes.get(2).copied().is_some_and(sep) {
        return refuse("is relative to a drive's current directory");
    }
    if sep(bytes[0]) {
        return refuse("is rooted without a drive letter");
    }
    let rest = if drive { &text[2..] } else { text };
    for name in rest.split(['/', '\\']).filter(|n| !n.is_empty()) {
        if name == "." || name == ".." {
            continue;
        }
        // In git's words `main..` is a range, not a name.
        let range = colons && name.contains("..");
        if name.ends_with(['.', ' ']) && !range {
            return refuse("has a name ending in a dot or space, which Windows drops");
        }
        if name.contains(':') && !colons {
            return refuse("names an alternate data stream");
        }
        if name.contains(['<', '>', '"', '|', '?', '*']) || name.chars().any(|c| c.is_control()) {
            return refuse("holds a character Windows doesn't allow in names");
        }
        let stem = name
            .split('.')
            .next()
            .unwrap_or(name)
            .trim_end()
            .to_ascii_uppercase();
        let chars: Vec<char> = stem.chars().collect();
        let reserved = matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        ) || (chars.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && (chars[3].is_ascii_digit() || "¹²³".contains(chars[3])));
        if reserved {
            return refuse("names a device");
        }
    }
    None
}

/// Why a search pattern (Glob's `pattern`, Grep's `glob`) can't be judged here, if it can't:
/// on Windows `\\` and `C:` would start a path the hook's `/` split doesn't see.
pub fn pattern_problem(pattern: &str) -> Option<String> {
    (cfg!(windows) && pattern.contains(['\\', ':'])).then(|| {
        format!(
            "the pattern `{pattern}` uses `\\` or a drive; write it with `/`, relative to the \
             worktree"
        )
    })
}

/// What agents may read outside the worktree besides dependency sources, for messages.
pub const SYSTEM_READS: &str = if cfg!(windows) { "" } else { "/usr and " };

/// What to add to "install Claude Code" on this platform.
pub const CLAUDE_HINT: &str = if cfg!(windows) {
    "; on Windows Paja needs its native claude.exe, not the npm claude.cmd"
} else {
    ""
};

/// Quote `path` as one word of a hook command line. Claude Code runs hooks with the system
/// shell: `sh` on Unix, `cmd.exe` on Windows (or Git Bash, in some versions). On Windows the
/// word is double-quoted with forward slashes, which both of those read the same way, and
/// paths holding characters either would expand are refused.
pub fn quote_hook_word(path: &Path) -> Result<String> {
    let text = path.to_string_lossy();
    if cfg!(windows) {
        if let Some(c) = text
            .chars()
            .find(|c| matches!(c, '"' | '%' | '!' | '$' | '`' | '^'))
        {
            bail!(
                "the path {text} holds `{c}`, which the shell running Claude Code's hooks would \
                 expand; move it (or PAJA_HOME) to a path without it"
            );
        }
        Ok(format!("\"{}\"", text.replace('\\', "/")))
    } else {
        Ok(format!("'{}'", text.replace('\'', r"'\''")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hook_words_are_quoted_for_the_platform_shell() {
        if cfg!(windows) {
            assert_eq!(
                quote_hook_word(Path::new(r"C:\Users\me\.paja")).unwrap(),
                "\"C:/Users/me/.paja\""
            );
            assert!(quote_hook_word(Path::new(r"C:\100%\x")).is_err());
        } else {
            assert_eq!(
                quote_hook_word(Path::new("/a/it's")).unwrap(),
                r"'/a/it'\''s'"
            );
        }
    }

    #[test]
    fn private_dirs_refuse_a_name_that_is_taken() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("private");
        create_private_dir(&dir).unwrap();
        assert!(create_private_dir(&dir).is_err());
    }
}
