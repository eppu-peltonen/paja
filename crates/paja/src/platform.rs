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

/// Where Claude Code reads managed (administrator) settings on this platform. On Windows it
/// reads files only, not the registry (checked against Claude Code's own lookup).
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
/// Unix; on Windows a protected, owner-only ACL set as it is created, so nobody gets in first.
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(dir)
    }
    #[cfg(not(unix))]
    win::create_owner_only(dir)
}

/// Make sure `dir` exists and only this user can change it. On Unix create it 0700 if needed
/// (an existing one is left as it is). On Windows create it owner-only, or, if it exists, check that this user
/// owns it and that its ACL is protected and lets no other account in; checked on every call,
/// so a later change is caught.
pub fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match create_private_dir(dir) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                win::check_owner_only(dir)
            }
            Err(err) => Err(err),
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(not(any(unix, windows)))]
    std::fs::create_dir_all(dir)
}

/// Create `path`, which must not exist yet, as a file only this user can read, write or run
/// (on Windows: inside a directory `create_private_dir` or `ensure_private_dir` made).
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
/// or space (`.git.` is `.git`), alternate data streams (`a:stream`), reserved device names (`NUL`, `COM1`) and characters
/// Windows forbids in names. All are refused rather than interpreted. `revisions` lifts only
/// the stream and trailing-dot rules, for git's revision forms (`HEAD:path`, `main..`). (An 8.3 short name such
/// as `GIT~1` only names something that exists, and the hook resolves existing paths to their
/// long names, so it needs no rule.)
/// Nothing is refused on Unix.
pub fn path_problem(text: &str, revisions: bool) -> Option<String> {
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
        let range = revisions && name.contains("..");
        if name.ends_with(['.', ' ']) && !range {
            return refuse("has a name ending in a dot or space, which Windows drops");
        }
        if name.contains(':') && !revisions {
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

/// `path` for a message: as the user would type it. On Windows that drops the `\\?\` prefix
/// `canonicalize` adds (`\\?\UNC\` becomes `\\`), which the hook itself refuses in a path.
pub fn shown(path: &Path) -> String {
    let text = path.display().to_string();
    if cfg!(windows) {
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            return format!(r"\\{rest}");
        }
        if let Some(rest) = text.strip_prefix(r"\\?\") {
            return rest.to_string();
        }
    }
    text
}

/// Quote `path` as one word of a hook command line. Claude Code runs hooks with the system
/// shell: `sh` on Unix, `cmd.exe` on Windows (or Git Bash, in some versions). On Windows the
/// word is double-quoted with forward slashes, which both of those read the same way, and
/// paths holding characters either would expand are refused.
pub fn quote_hook_word(path: &Path) -> Result<String> {
    let Some(text) = path.to_str() else {
        bail!(
            "the path {} is not valid UTF-8, so Claude Code's hook command can't name it; move \
             it (or PAJA_HOME) to a path that is",
            path.display()
        );
    };
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

/// How many times the person pressed Ctrl-C since [`catch_interrupts`].
static INTERRUPTS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// How many times the person pressed Ctrl-C (Ctrl-Break on Windows too) since
/// [`catch_interrupts`]: 0 while nobody did, 1 to stop after the current step, 2 or more to
/// stop now.
pub fn interrupts() -> u32 {
    let now = INTERRUPTS.load(std::sync::atomic::Ordering::SeqCst);
    announce(now);
    now
}

/// Say on stderr, once each, what the Ctrl-Cs so far will do. Called by the watcher and by
/// every reader of [`interrupts`], so the message is out before Paja acts on it.
fn announce(now: u32) {
    static ANNOUNCED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let now = now.min(2);
    let before = ANNOUNCED.fetch_max(now, std::sync::atomic::Ordering::SeqCst);
    // Not `eprintln!`: after a hangup stderr may be gone, and a failed write must not panic
    // before the children are ended.
    let say = |text: &str| {
        let _ = std::io::Write::write_all(&mut std::io::stderr(), text.as_bytes());
    };
    if before < 1 && now >= 1 {
        say("paja: stopping after this step; press Ctrl-C again to stop now\n");
    }
    if before < 2 && now >= 2 {
        say("paja: stopping now\n");
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_secs(10));
            std::process::exit(130);
        });
    }
}

fn interrupted() {
    INTERRUPTS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

/// The terminal closed or the process was asked to end: stop now, as a second Ctrl-C does,
/// so the agents and checks in their own process groups end with Paja.
#[cfg(unix)]
fn hung_up() {
    INTERRUPTS.fetch_max(2, std::sync::atomic::Ordering::SeqCst);
}

/// Count Ctrl-C instead of dying on it, for `paja run`. The first says on stderr that the run
/// stops after this step; the second that it stops now, and if the process is still alive
/// ten seconds later (stuck on something that isn't a child Paja can end) it exits. On Unix
/// a hangup (the terminal closed) or SIGTERM stops now too, ending the agents and checks that
/// run in their own process groups; on Windows closing the console reaches them itself.
pub fn catch_interrupts() -> Result<()> {
    #[cfg(unix)]
    {
        extern "C" fn on_sigint(_: std::ffi::c_int) {
            interrupted();
        }
        extern "C" fn on_sighup(_: std::ffi::c_int) {
            hung_up();
        }
        extern "C" {
            fn signal(signum: std::ffi::c_int, handler: usize) -> usize;
        }
        const SIGHUP: std::ffi::c_int = 1;
        const SIGINT: std::ffi::c_int = 2;
        const SIGTERM: std::ffi::c_int = 15;
        const SIG_ERR: usize = usize::MAX;
        let handlers = [
            (SIGINT, on_sigint as extern "C" fn(std::ffi::c_int)),
            (SIGHUP, on_sighup as extern "C" fn(std::ffi::c_int)),
            (SIGTERM, on_sighup as extern "C" fn(std::ffi::c_int)),
        ];
        for (signum, handler) in handlers {
            // SAFETY: the handlers only update an atomic, which is async-signal-safe.
            if unsafe { signal(signum, handler as usize) } == SIG_ERR {
                bail!("can't catch Ctrl-C: {}", std::io::Error::last_os_error());
            }
        }
    }
    #[cfg(windows)]
    win::catch_ctrl_c()?;
    std::thread::spawn(|| loop {
        std::thread::sleep(std::time::Duration::from_millis(100));
        interrupts();
    });
    Ok(())
}

/// Run `cmd` in a process group of its own, so the terminal's Ctrl-C reaches only `paja`,
/// which decides when its children stop. Unix: a new process group; Windows:
/// `CREATE_NEW_PROCESS_GROUP`, which also makes the child ignore Ctrl-C.
pub fn own_process_group(cmd: &mut Command) -> &mut Command {
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(cmd, 0);
    #[cfg(windows)]
    {
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        std::os::windows::process::CommandExt::creation_flags(cmd, CREATE_NEW_PROCESS_GROUP);
    }
    cmd
}

/// [`Command::output`] for a long step (an agent or a check), in its own process group, ended
/// with everything it started when the person presses Ctrl-C a second time.
pub fn output_until_stopped(cmd: &mut Command) -> std::io::Result<std::process::Output> {
    use std::io::Read;
    use std::process::Stdio;
    let mut child = own_process_group(cmd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // Drain both pipes while waiting, so a chatty child can't fill one and stall.
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let stdout = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let stderr = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if interrupts() >= 2 {
            kill_tree(&mut child);
            let _ = child.wait();
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "stopped by a second Ctrl-C",
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    Ok(std::process::Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

/// End `child` and every process it started, so a second Ctrl-C leaves nothing running.
fn kill_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        extern "C" {
            fn kill(pid: i32, sig: std::ffi::c_int) -> std::ffi::c_int;
        }
        const SIGKILL: std::ffi::c_int = 9;
        if let Ok(group) = i32::try_from(child.id()) {
            // SAFETY: a plain system call; the child leads its own process group.
            unsafe {
                kill(-group, SIGKILL);
            }
        }
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    let _ = child.kill();
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

    #[cfg(unix)]
    #[test]
    fn a_hook_word_that_is_not_utf8_is_refused() {
        use std::os::unix::ffi::OsStrExt;
        let path = Path::new(std::ffi::OsStr::from_bytes(b"/a/\xff"));
        let why = quote_hook_word(path).unwrap_err().to_string();
        assert!(why.contains("not valid UTF-8"), "{why}");
    }

    #[cfg(windows)]
    #[test]
    fn a_private_dir_is_owner_only_and_an_inheriting_one_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let private = tmp.path().join("private");
        ensure_private_dir(&private).unwrap();
        // Checked again when it exists.
        ensure_private_dir(&private).unwrap();
        let plain = tmp.path().join("plain");
        std::fs::create_dir(&plain).unwrap();
        let why = ensure_private_dir(&plain).unwrap_err().to_string();
        // Refused for inheriting, or for its owner when the tests run elevated.
        assert!(why.contains("Paja recreates it"), "{why}");
    }

    #[test]
    fn private_dirs_refuse_a_name_that_is_taken() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("private");
        create_private_dir(&dir).unwrap();
        assert!(create_private_dir(&dir).is_err());
    }
}

/// Windows security calls: owner-only directories made in one step, and checked later.
#[cfg(windows)]
mod win {
    use std::ffi::c_void;
    use std::io::{Error, Result};
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        GetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
    };
    use windows_sys::Win32::Security::{
        AclSizeInformation, EqualSid, GetAce, GetAclInformation, GetSecurityDescriptorControl,
        GetTokenInformation, TokenUser, ACCESS_ALLOWED_ACE, ACL, ACL_SIZE_INFORMATION,
        DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
        SECURITY_ATTRIBUTES, SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::Storage::FileSystem::CreateDirectoryW;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
    const ACCESS_DENIED_ACE_TYPE: u8 = 1;

    /// Count Ctrl-C and Ctrl-Break instead of ending the process; closing the console still
    /// ends it.
    pub fn catch_ctrl_c() -> anyhow::Result<()> {
        use windows_sys::Win32::System::Console::{
            SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_C_EVENT,
        };
        unsafe extern "system" fn on_ctrl(kind: u32) -> windows_sys::core::BOOL {
            if kind == CTRL_C_EVENT || kind == CTRL_BREAK_EVENT {
                super::interrupted();
                1
            } else {
                0
            }
        }
        // SAFETY: registers a handler that only increments an atomic.
        if unsafe { SetConsoleCtrlHandler(Some(on_ctrl), 1) } == 0 {
            anyhow::bail!("can't catch Ctrl-C: {}", Error::last_os_error());
        }
        Ok(())
    }

    fn wide(text: &std::ffi::OsStr) -> Vec<u16> {
        text.encode_wide().chain(Some(0)).collect()
    }

    /// The current user, as a `TOKEN_USER` buffer kept in u64s so it is aligned; its SID
    /// points into it, so it must outlive any use of that SID.
    fn token_user() -> Result<Vec<u64>> {
        // SAFETY: Win32 calls with a buffer of the size the API reports; the token handle is
        // closed before returning.
        unsafe {
            let mut token: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err(Error::last_os_error());
            }
            let mut len = 0u32;
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len);
            let mut buf = vec![0u64; (len as usize).div_ceil(8)];
            let ok = GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len);
            let err = Error::last_os_error();
            CloseHandle(token);
            if ok == 0 {
                return Err(err);
            }
            Ok(buf)
        }
    }

    /// The current user's SID as text (`S-1-5-...`).
    fn user_sid_string() -> Result<String> {
        let user = token_user()?;
        // SAFETY: `user` holds the TOKEN_USER filled above; ConvertSidToStringSidW allocates
        // the string, which is freed with LocalFree.
        unsafe {
            let sid = (*(user.as_ptr() as *const TOKEN_USER)).User.Sid;
            let mut text: *mut u16 = std::ptr::null_mut();
            if ConvertSidToStringSidW(sid, &mut text) == 0 {
                return Err(Error::last_os_error());
            }
            let len = (0..).take_while(|&i| *text.add(i) != 0).count();
            let out = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
            LocalFree(text.cast());
            Ok(out)
        }
    }

    /// Create `dir` with a protected ACL that gives this user full control, of it and of what
    /// is created in it, and nobody else anything.
    pub fn create_owner_only(dir: &Path) -> Result<()> {
        // Owned by this user too: an elevated process would otherwise give it to Administrators.
        let sid = user_sid_string()?;
        let sddl = format!("O:{sid}D:P(A;OICI;FA;;;{sid})");
        let sddl = wide(std::ffi::OsStr::new(&sddl));
        let path = wide(dir.as_os_str());
        // SAFETY: the conversion allocates the descriptor, which CreateDirectoryW copies and
        // LocalFree then frees.
        unsafe {
            let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                std::ptr::null_mut(),
            ) == 0
            {
                return Err(Error::last_os_error());
            }
            let attributes = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: sd,
                bInheritHandle: 0,
            };
            let ok = CreateDirectoryW(path.as_ptr(), &attributes);
            let err = Error::last_os_error();
            LocalFree(sd);
            if ok == 0 {
                return Err(err);
            }
        }
        Ok(())
    }

    /// Fail unless this user owns `dir` and its ACL is protected and allows only this user.
    pub fn check_owner_only(dir: &Path) -> Result<()> {
        let user = token_user()?;
        let path = wide(dir.as_os_str());
        let refuse = |why: &str| {
            Error::other(format!(
                "{} {why}, so another account could change what's in it; delete it and Paja \
                 recreates it for this user only (on a drive without permissions, such as FAT32 \
                 or exFAT, set PAJA_HOME to a folder on an NTFS drive instead)",
                dir.display()
            ))
        };
        // SAFETY: GetNamedSecurityInfoW allocates the descriptor that `owner` and `dacl` point
        // into; it is freed with LocalFree after the last use of either. `user` outlives `me`.
        unsafe {
            let me = (*(user.as_ptr() as *const TOKEN_USER)).User.Sid;
            let mut owner: PSID = std::ptr::null_mut();
            let mut dacl: *mut ACL = std::ptr::null_mut();
            let mut sd: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
            let code = GetNamedSecurityInfoW(
                path.as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut sd,
            );
            if code != 0 {
                return Err(Error::from_raw_os_error(code as i32));
            }
            let verdict = (|| {
                if EqualSid(owner, me) == 0 {
                    return Err(refuse("is owned by another account"));
                }
                let (mut control, mut revision) = (0u16, 0u32);
                if GetSecurityDescriptorControl(sd, &mut control, &mut revision) == 0 {
                    return Err(Error::last_os_error());
                }
                if control & SE_DACL_PROTECTED == 0 || dacl.is_null() {
                    return Err(refuse("inherits its parent's permissions"));
                }
                let mut size = ACL_SIZE_INFORMATION {
                    AceCount: 0,
                    AclBytesInUse: 0,
                    AclBytesFree: 0,
                };
                if GetAclInformation(
                    dacl,
                    (&mut size as *mut ACL_SIZE_INFORMATION).cast(),
                    std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                    AclSizeInformation,
                ) == 0
                {
                    return Err(Error::last_os_error());
                }
                for index in 0..size.AceCount {
                    let mut ace: *mut c_void = std::ptr::null_mut();
                    if GetAce(dacl, index, &mut ace) == 0 {
                        return Err(Error::last_os_error());
                    }
                    let ace = ace as *const ACCESS_ALLOWED_ACE;
                    // Deny entries only take access away; every other kind must be this user's.
                    let kind = (*ace).Header.AceType;
                    if kind == ACCESS_DENIED_ACE_TYPE {
                        continue;
                    }
                    let sid = std::ptr::addr_of!((*ace).SidStart) as PSID;
                    if kind != ACCESS_ALLOWED_ACE_TYPE || EqualSid(sid, me) == 0 {
                        return Err(refuse("lets another account in"));
                    }
                }
                Ok(())
            })();
            LocalFree(sd);
            verdict
        }
    }
}
