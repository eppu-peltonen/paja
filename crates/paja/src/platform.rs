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
        let program_files =
            std::env::var_os("ProgramFiles").unwrap_or_else(|| r"C:\Program Files".into());
        vec![
            PathBuf::from(program_files).join("ClaudeCode"),
            PathBuf::from(
                std::env::var_os("ProgramData").unwrap_or_else(|| r"C:\ProgramData".into()),
            )
            .join("ClaudeCode"),
        ]
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

/// Create `dir`, which must not exist yet, so that only this user can enter it. On Windows
/// the directory only inherits its parent's ACL, which is private under the user's own
/// temporary or profile directory but not under, say, `C:\paja`; owner-only ACLs come with
/// the hook's Windows port (#23), and Paja starts no agents on Windows before that.
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(dir)
    }
    #[cfg(not(unix))]
    std::fs::create_dir(dir)
}

/// Create `path`, which must not exist yet, as a file only this user can read, write or run
/// (on Windows: with its parent's ACL, as `create_private_dir` says).
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

fn posix_shell() -> Result<PathBuf> {
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

/// Why `paja run` and `paja hook` refuse to work here, if they do. On Windows the deny-list
/// hook doesn't yet understand Windows paths (drive letters, `\\` separators, case-insensitive
/// names), so it can't guard an agent; until it does, both refuse rather than run unguarded.
pub fn unguarded() -> Option<&'static str> {
    cfg!(windows).then_some(
        "the deny-list hook doesn't understand Windows paths yet, so Paja won't start agents \
         on Windows (they would run unguarded); that port is the next step of issue #23",
    )
}

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
