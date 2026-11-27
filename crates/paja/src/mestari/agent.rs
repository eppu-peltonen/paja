use super::*;

/// `claude`, or the program named by `PAJA_CLAUDE` (tests point it at a fake).
pub(super) fn claude_program() -> String {
    std::env::var("PAJA_CLAUDE").unwrap_or_else(|_| "claude".to_string())
}

/// The sandbox agents and checks run in, when `PAJA_SBX` turns it on. Refuses when it is on but
/// can't start; says so when it is off.
/// `base` is the default branch's commit, which `.paja/setup` is read from.
pub(super) fn start_sandbox(
    home: &Home,
    repo: &Path,
    base: &str,
) -> Result<Option<crate::sandbox::Sbx>> {
    if crate::sandbox::configured().is_none() {
        progress(&format!(
            "no sandbox ({} is not set): agents and checks run as you, behind the deny-list hook only",
            crate::sandbox::PROGRAM_VAR
        ));
        return Ok(None);
    }
    progress("starting the sandbox");
    let envs = home.envs_dir();
    std::fs::create_dir_all(&envs).with_context(|| format!("creating {}", envs.display()))?;
    // The worktrees keep their git data in the repository's own `.git`, which commits need.
    let common = git(
        repo,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let setup = read_from_base(repo, base, crate::sandbox::SETUP_FILE)?;
    crate::sandbox::start(
        &sandbox_mounts(envs, PathBuf::from(common)),
        setup.as_deref(),
    )
}

/// The folders the sandbox mounts: the home's environments and the repository's git data. Git
/// data already under the environments (an `init` agent's scratch repository) is reachable
/// through them, and a mount of its own would name a folder that is gone once the agent is
/// done, which a sandbox made then would keep asking for.
fn sandbox_mounts(envs: PathBuf, common: PathBuf) -> Vec<PathBuf> {
    let canonical = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    if canonical(&common).starts_with(canonical(&envs)) {
        vec![envs]
    } else {
        vec![envs, common]
    }
}

/// `argv` run in `sbx`, in `dir`, with the throwaway variables [`without_credentials`] gives
/// code on the host, and nothing else.
pub(super) fn in_sandbox(
    sbx: &crate::sandbox::Sbx,
    dir: &Path,
    identity: Vec<(&str, String)>,
    script_or_argv: Result<&str, &[String]>,
) -> Result<Command> {
    let mut vars = Command::new("env");
    without_credentials(&mut vars, Path::new(crate::sandbox::SCRATCH), identity);
    let env = crate::sandbox::env_of(&vars);
    match script_or_argv {
        Ok(script) => sbx.script(dir, &env, script),
        Err(argv) => sbx.command(dir, &env, argv),
    }
}

/// The built-in tools an agent gets: reviewers read and run commands, builders also edit.
/// `--allowedTools` and the deny-list hook still decide what each call may do.
pub(super) fn agent_tools(read_only: bool) -> &'static str {
    if read_only {
        "Read,Glob,Grep,Bash"
    } else {
        "Read,Edit,Write,Glob,Grep,Bash"
    }
}

/// A session stopped at its `--max-turns` limit. Carries Claude Code's result, which still
/// reports what the session spent and its session id.
#[derive(Debug)]
pub(super) struct TurnLimit(pub(super) Value);

impl std::fmt::Display for TurnLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Claude Code stopped at its turn limit")
    }
}

impl std::error::Error for TurnLimit {}

pub(super) fn turn_limit(err: &anyhow::Error) -> Option<&TurnLimit> {
    err.chain().find_map(|e| e.downcast_ref::<TurnLimit>())
}

/// The deny-list hook never ran during a session, so its work was not trusted and thrown away.
/// Its session is never resumed.
#[derive(Debug)]
pub(super) struct HookNeverRan(String);

impl std::fmt::Display for HookNeverRan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for HookNeverRan {}

/// Claude Code ran and exited with an error or reported one: nothing Paja itself refused, so a
/// failed resume may be tried again as a fresh session. Holds the JSON result when Claude Code
/// printed one.
#[derive(Debug)]
pub(super) struct ClaudeFailed(pub(super) String, pub(super) Option<Value>);

impl std::fmt::Display for ClaudeFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ClaudeFailed {}

/// A step stopped at the usage or rate limit (`api_error_status` 429). Its work is kept and the
/// item resumes after the window resets.
#[derive(Debug, Clone)]
pub(super) struct UsageLimit {
    /// Claude Code's failed result, which still says what the step spent.
    pub(super) result: Option<Value>,
    /// When the window resets, in milliseconds since the epoch, if Claude Code said so.
    pub(super) resets_at: Option<u64>,
    /// Not a stop Claude Code made: the recorded window was nearly used up, so the step was
    /// never started.
    pub(super) nearly: bool,
}

/// The usage limit behind `err`, if that is what stopped the step.
pub(super) fn usage_limit(err: &anyhow::Error) -> Option<UsageLimit> {
    let ClaudeFailed(text, result) = err.chain().find_map(|e| e.downcast_ref::<ClaudeFailed>())?;
    let limited = match result {
        Some(out) => out["api_error_status"].as_u64() == Some(429),
        None => false,
    };
    limited.then(|| UsageLimit {
        resets_at: reset_time(result.as_ref(), text),
        result: result.clone(),
        nearly: false,
    })
}

/// When the window resets, from Claude Code's result: a `resets_at` field (seconds or
/// milliseconds), else the epoch seconds after the `|` in its "usage limit reached|<seconds>"
/// text, in the result or the error text.
pub(super) fn reset_time(result: Option<&Value>, text: &str) -> Option<u64> {
    let as_ms = |n: u64| if n < 100_000_000_000 { n * 1000 } else { n };
    if let Some(n) = result.and_then(|r| r["resets_at"].as_u64()) {
        return Some(as_ms(n));
    }
    let from_text = |text: &str| {
        let (_, after) = text.rsplit_once('|')?;
        let digits: String = after
            .trim()
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        // Ten digits or more: an epoch, not a stray number.
        (digits.len() >= 10)
            .then(|| digits.parse::<u64>().ok())
            .flatten()
    };
    result
        .and_then(|r| r["result"].as_str())
        .and_then(from_text)
        .or_else(|| from_text(text))
        .map(as_ms)
}

/// Why a failed `claude -p` result must not be retried as a fresh session, and what the person
/// does about it: a sign-in that no longer works fails a new session the same way. (A usage
/// limit does too, but it is not for the person: see [`usage_limit`].)
pub(super) fn not_retried(out: &Value) -> Option<&'static str> {
    match out["api_error_status"].as_u64() {
        Some(401 | 403) => Some(
            "Claude Code's sign-in was refused; run `claude` and sign in again (or check \
             ANTHROPIC_API_KEY), then answer the item to run it again",
        ),
        _ => None,
    }
}

/// The arguments of one headless Claude Code session (the prompt goes on stdin). Every agent
/// runs in auto mode, with or without a sandbox; `settings` (the deny-list hook) is given only
/// outside one.
fn claude_args(
    settings: Option<String>,
    harness: String,
    read_only: bool,
    model_args: &[String],
    tools: &[&str],
    schema: Option<&Value>,
) -> Vec<String> {
    // stream-json (which needs `--verbose`) carries the `rate_limit_event` messages the usage
    // window is read from; [`fold_stream`] turns the stream back into the one final result.
    let mut args: Vec<String> = [
        "-p",
        "--output-format",
        "stream-json",
        "--verbose",
        "--permission-mode",
    ]
    .into_iter()
    .map(String::from)
    // Auto mode, Anthropic's own safety checks, as an extra layer beside the hook. Never
    // bypassPermissions (the sandbox image's default) and never a weaker mode.
    .chain(["auto".to_string()])
    // No settings files at all, only the deny-list hook: neither the owner's settings nor
    // a settings file on the branch can loosen or switch it off.
    .chain(["--setting-sources".to_string(), String::new()])
    .collect();
    if let Some(settings) = settings {
        args.extend(["--settings".to_string(), settings]);
    }
    // With no setting sources Claude Code loads no CLAUDE.md, so pass the harness from
    // main: the branch under review can't rewrite the rules its agents follow.
    args.extend(["--append-system-prompt".to_string(), harness]);
    // No MCP servers from anyone's configuration: agents get only Claude Code's own tools.
    args.push("--strict-mcp-config".to_string());
    // Only the built-in tools the role uses are defined at all, so no other tool (subagents,
    // web tools) reaches the agent or spends prompt tokens on its definition.
    args.extend(["--tools".to_string(), agent_tools(read_only).to_string()]);
    // Per-machine details (the worktree's path, its git status) go in the first message
    // instead of the system prompt, so sessions in different environments share a cached
    // prompt prefix.
    args.push("--exclude-dynamic-system-prompt-sections".to_string());
    // The model and effort come from `.paja/models` on main, so a change can't pick the models
    // that judge it.
    args.extend(model_args.iter().cloned());
    args.push("--allowedTools".to_string());
    args.extend(tools.iter().map(|t| t.to_string()));
    if let Some(schema) = schema {
        args.extend(["--json-schema".to_string(), schema.to_string()]);
    }
    args
}

/// The single result JSON of a `claude -p --output-format stream-json` run: the last message of
/// type `result`, with the latest `rate_limit_event`'s `rate_limit_info` added when the result
/// has none of its own. Output that is already one JSON value (an older Claude Code, a fake in
/// tests) is returned as it is, and so is output with no `result` message (a crash), so the
/// callers' errors still show what Claude Code printed.
pub(super) fn fold_stream(stdout: &[u8]) -> Vec<u8> {
    if serde_json::from_slice::<Value>(stdout).is_ok() {
        return stdout.to_vec();
    }
    let mut result: Option<Value> = None;
    let mut window: Option<Value> = None;
    for line in String::from_utf8_lossy(stdout).lines() {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match message["type"].as_str() {
            Some("result") => result = Some(message),
            Some("rate_limit_event") if message["rate_limit_info"].is_object() => {
                window = Some(message["rate_limit_info"].clone());
            }
            _ => {}
        }
    }
    let Some(mut result) = result else {
        return stdout.to_vec();
    };
    if let (Some(info), Some(map)) = (window, result.as_object_mut()) {
        map.entry("rate_limit_info").or_insert(info);
    }
    serde_json::to_vec(&result).unwrap_or_else(|_| stdout.to_vec())
}

/// A host scratch directory (if any) that is removed when this is dropped.
struct Scratch(Option<PathBuf>);

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Some(dir) = &self.0 {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// The most characters a `claude` command line may hold: well under Windows' 32,767, since
/// the harness (`CLAUDE.md`), the hook settings and the schema all travel on it.
pub(super) const COMMAND_LINE_BUDGET: usize = 30_000;

/// Run headless Claude Code in the environment and return its JSON result. The programs it may
/// run besides local git are the ones `.paja/commands` on `base` lists.
#[allow(clippy::too_many_arguments)]
pub(super) fn claude(
    home: &Home,
    env: &Env,
    base: &str,
    prompt: &str,
    tools: &[&str],
    schema: Option<&Value>,
    read_only: bool,
    model_args: &[String],
) -> Result<Value> {
    claude_with(
        home, env, base, prompt, tools, schema, read_only, model_args, None,
    )
}

/// [`claude`], with `programs` (when given) as the programs the hook lets it run instead of
/// `.paja/commands`: for a repository that has no such file yet.
#[allow(clippy::too_many_arguments)]
pub(super) fn claude_with(
    home: &Home,
    env: &Env,
    base: &str,
    prompt: &str,
    tools: &[&str],
    schema: Option<&Value>,
    read_only: bool,
    model_args: &[String],
    programs: Option<&[String]>,
) -> Result<Value> {
    let program = claude_program();
    let harness = match read_from_base(&env.path, base, "CLAUDE.md")? {
        Some(text) => format!("The project's CLAUDE.md, as it is on the default branch:\n\n{text}"),
        None => String::new(),
    };
    let git_identity = git_identity(&env.path);
    let guard = GitGuard::new(&env.path)?;
    // Agents get a throwaway PAJA_HOME, so trying the CLI never touches the factory's state.
    // The sandbox keeps its own (`sandbox::SCRATCH`), so none is made on the host then. The guard
    // removes it on every way out, an early error included.
    let sbx = crate::sandbox::active();
    let scratch = Scratch(if sbx.is_none() {
        Some(tempfile_dir(&env.path)?)
    } else {
        None
    });
    // In the sandbox the deny-list hook (a host program) doesn't run; the sandbox is the guard.
    let (settings, stamp, exe) = if sbx.is_some() {
        (String::new(), PathBuf::new(), PathBuf::new())
    } else {
        // The programs the project allows, as main has them; a file that can't be read or
        // parsed stops the agent before it starts.
        let programs = match programs {
            Some(given) => Ok(given.to_vec()),
            None => read_from_base(&env.path, base, crate::hook::COMMANDS_FILE)
                .and_then(|text| crate::hook::Commands::parse(text.as_deref()))
                .map(|c| if read_only { c.reviewer } else { c.builder }),
        };
        programs.and_then(|programs| hook_settings(&env.path, home, read_only, &programs))?
    };
    // The prompt goes on stdin, not the command line: a review prompt holds the diff, and
    // Windows refuses a command line over 32,767 characters (os error 206).
    let args = claude_args(
        (sbx.is_none()).then_some(settings),
        harness,
        read_only,
        model_args,
        tools,
        schema,
    );
    let line: usize = args.iter().map(|arg| arg.len() + 3).sum();
    if line > COMMAND_LINE_BUDGET {
        let longest = args
            .iter()
            .max_by_key(|arg| arg.len())
            .cloned()
            .unwrap_or_default();
        let _ = std::fs::remove_file(&stamp);
        if let Some(dir) = stamp.parent().filter(|_| sbx.is_none()) {
            let _ = std::fs::remove_dir(dir);
        }
        bail!(
            "the `claude` command line would be {line} characters, over Paja's budget of \
             {COMMAND_LINE_BUDGET} (Windows allows 32,767); its longest argument starts `{}`",
            longest.chars().take(60).collect::<String>()
        );
    }
    let mut cmd = match &sbx {
        Some(sbx) => {
            let mut argv = vec![program.clone()];
            argv.extend(args);
            let mut cmd = in_sandbox(sbx, &env.path, git_identity, Err(&argv))?;
            // `sbx exec` passes stdin through only when asked to.
            crate::sandbox::keep_stdin(&mut cmd);
            cmd
        }
        None => {
            let mut cmd = Command::new(&program);
            cmd.current_dir(&env.path);
            without_credentials(
                &mut cmd,
                scratch.0.as_deref().unwrap_or(&env.path),
                git_identity,
            );
            cmd.args(&args);
            cmd
        }
    };
    let before = git_head(&env.path);
    let status_before = git_status(&env.path);
    let output = crate::platform::output_until_stopped_with_input(
        &mut cmd,
        Some(prompt.as_bytes().to_vec()),
    )
    .map(|mut out| {
        out.stdout = fold_stream(&out.stdout);
        out
    });
    drop(scratch);
    let hooked = stamp.exists();
    let _ = std::fs::remove_file(&stamp);
    if let Some(dir) = stamp.parent() {
        let _ = std::fs::remove_dir(dir);
    }
    guard.check("the agent")?;
    // Code the agent ran could also have swapped the hook's copy during the session.
    let hooked = sbx.is_some() || hooked && hook_copy_intact(&exe);
    if let (Some(sbx), Ok(out)) = (&sbx, &output) {
        // Not a failed check or session: the person has to sign in, once.
        if crate::sandbox::signed_out(
            &String::from_utf8_lossy(&out.stdout),
            &String::from_utf8_lossy(&out.stderr),
        ) {
            bail!("{}", sbx.sign_in_hint());
        }
    }
    // A session that took more than one turn, or changed the worktree, used tools, so the hook
    // must have run. If it didn't, something (such as an administrator's policy) switched it
    // off and nothing the agent did can be trusted: throw it away, so a later run can't resume
    // and land it.
    let turns = output
        .as_ref()
        .ok()
        .and_then(|o| serde_json::from_slice::<Value>(&o.stdout).ok())
        .and_then(|value| value["num_turns"].as_u64())
        .unwrap_or(0);
    if !hooked
        && output.is_ok()
        && (turns > 1 || changed_since(&env.path, before.as_deref(), status_before.as_deref()))
    {
        let fate = match before.as_deref() {
            Some(before) => match discard(env, before) {
                Ok(()) => "was discarded".to_string(),
                Err(err) => format!(
                    "could not be discarded ({err:#}); delete the branch {} before running it again",
                    env.branch
                ),
            },
            None => format!(
                "is still on the branch {} (its starting commit was unknown); check or delete it \
                 before running it again",
                env.branch
            ),
        };
        return Err(HookNeverRan(format!(
            "Claude Code never called Paja's deny-list hook during the agent's run in {}, or the \
             hook's copy {} changed during it, so its work is not trusted and {fate}; check for \
             settings that disable hooks (such as `disableAllHooks` in managed settings) and run \
             again",
            env.path.display(),
            exe.display()
        ))
        .into());
    }
    let output = output.with_context(|| {
        format!(
            "running `{program}` (install Claude Code and sign in{})",
            crate::platform::CLAUDE_HINT
        )
    })?;
    // Checked before the exit status: a session at its turn limit exits with an error.
    if let Ok(value) = serde_json::from_slice::<Value>(&output.stdout) {
        if value["subtype"] == "error_max_turns" {
            return Err(TurnLimit(value).into());
        }
    }
    if !output.status.success() {
        // A Claude Code without headless auto mode rejects the flag: stop, never fall back to
        // another permission mode.
        let text = tail(&output);
        if text.contains("--permission-mode") {
            bail!(
                "`{program} -p` does not accept `--permission-mode auto`; Paja always runs agents \
                 in auto mode and never falls back to another mode, so update Claude Code to a \
                 version that supports auto mode headless. Claude Code said: {text}"
            );
        }
        return Err(ClaudeFailed(
            format!(
                "`{program} -p` failed in {}: {}",
                env.path.display(),
                tail(&output)
            ),
            serde_json::from_slice(&output.stdout).ok(),
        )
        .into());
    }
    let value: Value = serde_json::from_slice(&output.stdout)
        .with_context(|| format!("`{program} -p` printed something that is not JSON"))?;
    if value.get("is_error") == Some(&Value::Bool(true)) {
        return Err(ClaudeFailed(
            format!("`{program} -p` reported an error: {}", value["result"]),
            Some(value),
        )
        .into());
    }
    Ok(value)
}

/// The owner's commit name and email, and their global ignore file, which agents' git uses
/// while git's global configuration is hidden from them.
pub(super) fn git_identity(repo: &Path) -> Vec<(&'static str, String)> {
    let get = |key: &str| {
        let out = git_command()
            .arg("-C")
            .arg(repo)
            .args(["config"])
            .args((key == "core.excludesFile").then_some("--path"))
            .arg(key)
            .output()
            .ok()?;
        let value = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (out.status.success() && !value.is_empty()).then_some(value)
    };
    let mut vars = Vec::new();
    if let Some(name) = get("user.name") {
        vars.push(("GIT_AUTHOR_NAME", name.clone()));
        vars.push(("GIT_COMMITTER_NAME", name));
    }
    if let Some(email) = get("user.email") {
        vars.push(("GIT_AUTHOR_EMAIL", email.clone()));
        vars.push(("GIT_COMMITTER_EMAIL", email));
    }
    // Without it, editor and system files the owner ignores everywhere would be committed.
    // Mestari sets two config entries; when there is no ignore file, the second repeats the
    // first, so git's own default ignore file still applies.
    let (key, value) = match get("core.excludesFile") {
        Some(file) => ("core.excludesFile", file),
        None => ("credential.helper", String::new()),
    };
    vars.push(("GIT_CONFIG_KEY_1", key.into()));
    vars.push(("GIT_CONFIG_VALUE_1", value));
    vars
}

pub(super) fn git_head(repo: &Path) -> Option<String> {
    let out = git_command()
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Whether the worktree's HEAD or its uncommitted changes differ from before the run.
pub(super) fn changed_since(repo: &Path, head: Option<&str>, status: Option<&str>) -> bool {
    git_head(repo).as_deref() != head || git_status(repo).as_deref() != status
}

/// `git status --porcelain`, to compare the worktree before and after an agent's run.
pub(super) fn git_status(repo: &Path) -> Option<String> {
    let out = git_command()
        .arg("-C")
        .arg(repo)
        .args(["status", "--porcelain"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Whether a `git ls-files -v` listing marks any file assume-unchanged (a lowercase tag) or
/// skip-worktree (`S`).
pub(super) fn has_index_flags(listing: &str) -> bool {
    listing
        .lines()
        .any(|line| line.starts_with(|c: char| c.is_ascii_lowercase() || c == 'S'))
}

/// Put the environment's branch, index and files back at `to` after code ran there. The
/// index is rebuilt from scratch, so no file stays hidden behind an assume-unchanged flag,
/// and only sparse checkout's own skip-worktree flags come back. Ignored files such as
/// `target/` stay.
pub(super) fn put_back(env: &Env, to: &str) -> Result<()> {
    let index = git(&env.path, &["rev-parse", "--git-path", "index"])?;
    let index = env.path.join(index);
    match std::fs::remove_file(&index) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(err).with_context(|| format!("removing git's index {}", index.display()))
        }
    }
    git(
        &env.path,
        &["checkout", "-q", "-f", "-B", env.branch.as_str(), to],
    )?;
    git(&env.path, &["clean", "-q", "-f", "-d"])?;
    Ok(())
}

/// Put the environment's own branch back at `to` and drop every file made since, ignored
/// ones included, even if the agent switched branches.
pub(super) fn discard(env: &Env, to: &str) -> Result<()> {
    for args in [
        &["checkout", "-q", "-f", "-B", &env.branch, to][..],
        &["clean", "-q", "-f", "-d", "-x"],
    ] {
        let status = git_command()
            .arg("-C")
            .arg(&env.path)
            .args(args)
            .status()
            .context("running git")?;
        if !status.success() {
            bail!("`git {}` failed in {}", args.join(" "), env.path.display());
        }
    }
    Ok(())
}

/// Keeps stamp directory names unique within one process.
pub(super) static STAMPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Claude Code settings that run every tool call through `paja hook`, bounded to `root`.
/// Before handing them out, check that the hook really blocks a push and allows `git status`; an
/// agent never starts under a hook that doesn't.
pub(super) fn hook_settings(
    root: &Path,
    home: &Home,
    read_only: bool,
    programs: &[String],
) -> Result<(String, PathBuf, PathBuf)> {
    refuse_hook_policies()?;
    let exe = hook_binary(home)?;
    // The stamp sits in a fresh directory only this user can enter, so nobody can plant it
    // beforehand; creating the directory fails if the name is already taken.
    let private = std::env::temp_dir().join(format!(
        "paja-stamp-{}-{}-{}",
        std::process::id(),
        crate::events::now_ms(),
        STAMPS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    crate::platform::create_private_dir(&private)
        .with_context(|| format!("creating the hook's stamp directory {}", private.display()))?;
    let stamp = private.join("stamp");
    // Test the very command Claude Code will run, with a throwaway stamp of its own, so a
    // path the shell reads differently fails here and not on every tool call.
    let trial = private.join("self-test");
    // The shipped command is built once; the trial differs from it in the stamp word only.
    let command = hook_command(&exe, root, home.root(), &stamp, read_only, programs)?;
    let tested = crate::platform::quote_hook_word(&stamp)
        .and_then(|real| Ok((real, crate::platform::quote_hook_word(&trial)?)))
        .and_then(|(real, fake)| {
            let swapped =
                command.replacen(&format!("--stamp {real}"), &format!("--stamp {fake}"), 1);
            if swapped == command {
                bail!("the hook command `{command}` has no stamp word to test with");
            }
            self_test_hook(&swapped, root, read_only)
        });
    let _ = std::fs::remove_file(&trial);
    if let Err(err) = tested {
        let _ = std::fs::remove_dir(&private);
        return Err(err);
    }
    // Claude Code's timeout also counts as "allow", so leave the hook's own deadline room.
    let timeout = crate::hook::HOOK_DEADLINE_SECS * 3;
    let settings = json!({
        "hooks": {
            "PreToolUse": [{
                "matcher": "*",
                "hooks": [{"type": "command", "command": command, "timeout": timeout}]
            }]
        }
    })
    .to_string();
    Ok((settings, stamp, exe))
}

/// The shell command Claude Code runs for every tool call. It only blocks on exit 2, so a
/// hook that can't start (127), is killed or fails in any other way must block too.
pub(super) fn hook_command(
    exe: &Path,
    root: &Path,
    home: &Path,
    stamp: &Path,
    read_only: bool,
    programs: &[String],
) -> Result<String> {
    Ok(format!(
        "{} hook --root {} --home {} --stamp {}{} --programs {} || exit 2",
        crate::platform::quote_hook_word(exe)?,
        crate::platform::quote_hook_word(root)?,
        crate::platform::quote_hook_word(home)?,
        crate::platform::quote_hook_word(stamp)?,
        if read_only { " --read-only" } else { "" },
        crate::platform::quote_hook_word(Path::new(&crate::hook::encode_programs(programs)))?
    ))
}

/// Run the hook `command` through the shell, as Claude Code does, and check that it blocks a
/// push (a commit when `read_only`) and allows a build.
pub(super) fn self_test_hook(command: &str, root: &Path, read_only: bool) -> Result<()> {
    let (blocked, build) = crate::hook::self_test_inputs(root, read_only);
    let (code, stderr) = run_hook_once(command, &blocked)?;
    let blocks = code == Some(2) && stderr.contains("Blocked by Paja's deny-list");
    let (code, _) = run_hook_once(command, &build)?;
    if !blocks || code != Some(0) {
        bail!(
            "the deny-list hook command `{command}` failed its self-test (it must block `{}` \
             and allow `git status`), so no agent will start; rebuild paja from main, and \
             check that every path in the command exists as written",
            if read_only { "git commit" } else { "git push" }
        );
    }
    Ok(())
}

/// Managed (administrator) settings load whatever `--setting-sources` says, and two of their
/// keys would silently drop Mestari's hook. Refuse to start agents on such a machine.
pub(super) fn refuse_hook_policies() -> Result<()> {
    let mut paths = Vec::new();
    for dir in crate::platform::managed_settings_dirs() {
        let dir = dir.as_path();
        paths.push(dir.join("managed-settings.json"));
        // Drop-in files next to it carry the same keys.
        if let Ok(entries) = std::fs::read_dir(dir.join("managed-settings.d")) {
            let mut drop_ins: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "json"))
                .collect();
            drop_ins.sort();
            paths.extend(drop_ins);
        }
    }
    for path in &paths {
        let path = path.display();
        let text = match std::fs::read_to_string(path.to_string()) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "reading Claude Code's managed settings {path}, which could switch off \
                         Paja's deny-list hook; make it readable or run Paja elsewhere"
                    )
                })
            }
        };
        let settings: Value = serde_json::from_str(&text).with_context(|| {
            format!(
                "reading Claude Code's managed settings {path}, which is not valid JSON; ask \
                     the machine's administrator to fix it, or run Paja on another machine"
            )
        })?;
        for key in ["disableAllHooks", "allowManagedHooksOnly"] {
            if settings[key] == Value::Bool(true) {
                bail!(
                    "Claude Code's managed settings ({path}) set `{key}`, which would switch off \
                     Paja's deny-list hook, so no agent will start; ask the machine's \
                     administrator to drop it, or run Paja on another machine"
                );
            }
        }
    }
    Ok(())
}

/// This paja's own bytes, read once per process: what the hook copy must match, whatever later
/// happens to the file.
pub(super) static RUNNING: std::sync::OnceLock<Vec<u8>> = std::sync::OnceLock::new();

/// Whether the hook's copy still matches this paja byte for byte.
pub(super) fn hook_copy_intact(exe: &Path) -> bool {
    RUNNING
        .get()
        .is_some_and(|bytes| read_regular(exe).as_deref() == Some(bytes.as_slice()))
}

/// A git configuration file's entries as git itself reads them, leaving out `branch.*`:
/// ordinary git work (`push -u`, `checkout -b`) writes those, and they run no program. `None`
/// when git can't parse the file, which is then compared byte for byte. Includes are watched
/// as files of their own, so they aren't followed here. Only called on plain files.
pub(super) fn config_entries(path: &Path) -> Option<Vec<u8>> {
    let out = Command::new("git")
        .arg("config")
        .arg("--file")
        .arg(path)
        .args(["--no-includes", "--list", "--null"])
        .output()
        .ok()
        .filter(|out| out.status.success())?;
    let mut kept = Vec::new();
    for entry in out.stdout.split(|b| *b == 0).filter(|e| !e.is_empty()) {
        if !entry.to_ascii_lowercase().starts_with(b"branch.") {
            kept.extend_from_slice(entry);
            kept.push(0);
        }
    }
    Some(kept)
}

/// What [`GitGuard`] keeps, in front of the raw bytes, for a configuration file git can't parse.
pub(super) const UNPARSED: &[u8] = b"\0unparsed\0";

/// The entries that differ between two [`config_entries`] listings, as `name: old -> new`,
/// `name added (value)` or `name removed`. Values show only in sections that hold no URLs or
/// credentials (`core.`, `extensions.` and the like), never when they look like a URL with a user
/// or a header, and are cut when long; a name with a URL in it shows only its section and key.
pub(super) fn config_changes(before: &[u8], after: &[u8]) -> Vec<String> {
    if before.starts_with(UNPARSED) || after.starts_with(UNPARSED) {
        return Vec::new();
    }
    fn entries(raw: &[u8]) -> Vec<(String, String)> {
        raw.split(|b| *b == 0)
            .filter(|e| !e.is_empty())
            .map(|e| {
                let e = String::from_utf8_lossy(e);
                let (name, value) = e.split_once('\n').unwrap_or((&e, ""));
                (name.to_string(), value.to_string())
            })
            .collect()
    }
    // Only sections whose values carry no URLs or secrets show them; anything that looks like
    // a URL with a user, or a header, stays hidden even there.
    fn shown(name: &str, value: &str) -> String {
        let lower = name.to_ascii_lowercase();
        let plain = [
            "core.",
            "extensions.",
            "safe.",
            "init.",
            "gc.",
            "index.",
            "feature.",
        ]
        .iter()
        .any(|prefix| lower.starts_with(prefix))
            && !["password", "token", "secret", "key", "credential", "header"]
                .iter()
                .any(|word| lower.contains(word));
        let value_lower = value.to_ascii_lowercase();
        let risky = [
            "@",
            "://",
            "authorization",
            "bearer",
            "ghp_",
            "ghs_",
            "github_pat_",
        ]
        .iter()
        .any(|bit| value_lower.contains(bit));
        if !plain || risky {
            return "(value hidden)".into();
        }
        let mut cut: String = value
            .chars()
            .take(80)
            .collect::<String>()
            .replace('\n', "\\n");
        if value.chars().count() > 80 {
            cut.push('…');
        }
        format!("`{cut}`")
    }
    // A subsection can hold a URL with a user and password (`url.<url>.insteadof`).
    fn shown_name(name: &str) -> String {
        if name.contains('@') || name.contains("://") {
            let first = name.split('.').next().unwrap_or("");
            let last = name.rsplit('.').next().unwrap_or("");
            format!("{first}.(hidden).{last}")
        } else {
            name.to_string()
        }
    }
    let (was, now) = (entries(before), entries(after));
    let mut changes = Vec::new();
    let names: std::collections::BTreeSet<&str> = was
        .iter()
        .chain(&now)
        .map(|(name, _)| name.as_str())
        .collect();
    for name in names {
        let old: Vec<&str> = was
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .collect();
        let new: Vec<&str> = now
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .collect();
        if old == new {
            continue;
        }
        let list = |values: &[&str]| -> String {
            values
                .iter()
                .map(|v| shown(name, v))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let shown_as = shown_name(name);
        let name = shown_as.as_str();
        changes.push(match (old.is_empty(), new.is_empty()) {
            (true, _) => format!("{name} added ({})", list(&new)),
            (_, true) => format!("{name} removed"),
            _ => format!("{name}: {} -> {}", list(&old), list(&new)),
        });
    }
    changes
}

/// A file's contents, read only when it is a plain file: opening a FIFO someone planted
/// would hang. Anything else reads as a marker that differs from every real content.
pub(super) fn read_regular(path: &Path) -> Option<Vec<u8>> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => std::fs::read(path).ok(),
        Ok(_) => Some(b"\0not a regular file\0".to_vec()),
        Err(_) => None,
    }
}

/// The paja binary the hook runs from: a copy in the factory home, so an agent's own
/// `cargo build` can't replace the binary that judges it. Made once per `paja run` from the
/// binary that is running (on Linux even if the file on disk has been rebuilt since; elsewhere
/// it is the file on disk when the run first needs it); copies other runs
/// left behind are removed once they are a day old.
pub(super) fn hook_binary(home: &Home) -> Result<PathBuf> {
    let _lock = home.lock()?;
    let dir = home.root().join("hook");
    // Nobody else may swap the hook's binary: the directory is this user's alone.
    crate::platform::ensure_private_dir(&dir).with_context(|| {
        format!(
            "making {} private to this user for the deny-list hook",
            dir.display()
        )
    })?;
    if let Ok(entries) = std::fs::read_dir(&dir) {
        let day = std::time::Duration::from_secs(24 * 60 * 60);
        for entry in entries.flatten() {
            let stale = entry
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > day);
            if stale {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let running =
        crate::platform::running_exe().context("finding the paja binary for the deny-list hook")?;
    let bytes = match RUNNING.get() {
        Some(bytes) => bytes,
        None => {
            let read = std::fs::read(&running)
                .with_context(|| format!("reading {} for the deny-list hook", running.display()))?;
            RUNNING.get_or_init(|| read)
        }
    };
    // One copy per build of paja, shared by every run of it, so copies don't pile up.
    let digest = bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0100_0000_01b3)
    });
    // `.exe` on Windows, which runs nothing without it.
    let exe = dir.join(format!(
        "paja-{}-{digest:016x}{}",
        bytes.len(),
        std::env::consts::EXE_SUFFIX
    ));
    // The copy sits in the factory home, which code an agent runs could reach. Use it only
    // while it still matches this paja byte for byte; otherwise copy again.
    if !hook_copy_intact(&exe) {
        // Write under a temporary name and rename, so no run ever sees half a binary.
        let partial = dir.join(format!(
            "partial-{}-{}",
            std::process::id(),
            crate::events::now_ms()
        ));
        let written = (|| {
            let mut file = crate::platform::create_private_file(&partial)?;
            std::io::Write::write_all(&mut file, bytes)?;
            drop(file);
            std::fs::rename(&partial, &exe)
        })();
        written
            .inspect_err(|_| {
                let _ = std::fs::remove_file(&partial);
            })
            .with_context(|| {
                format!(
                    "copying {} to {} for the deny-list hook; check that {} is writable",
                    running.display(),
                    exe.display(),
                    dir.display()
                )
            })?;
    }
    // Keep the copy fresh so another run's cleanup leaves it alone.
    crate::platform::touch(&exe);
    Ok(exe)
}

pub(super) fn run_hook_once(command: &str, input: &str) -> Result<(Option<i32>, String)> {
    let mut child = crate::platform::shell(command)?
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("starting the deny-list hook command `{command}`"))?;
    // A hook that exits without reading its input is judged by its exit code, not the pipe.
    if let Err(err) = std::io::Write::write_all(
        child.stdin.as_mut().context("the hook's stdin")?,
        input.as_bytes(),
    ) {
        if err.kind() != std::io::ErrorKind::BrokenPipe {
            let _ = child.kill();
            let _ = child.wait();
            return Err(err).with_context(|| {
                format!(
                    "sending the self-test to the deny-list hook command `{command}`; rebuild \
                     paja from main"
                )
            });
        }
    }
    drop(child.stdin.take());
    // Drain stderr while waiting, so a chatty hook can't fill the pipe and look hung.
    let mut stderr = child.stderr.take().context("the hook's stderr")?;
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        let _ = std::io::Read::read_to_string(&mut stderr, &mut text);
        text
    });
    // The hook blocks itself after its deadline; anything longer is a hung binary.
    let deadline = std::time::Instant::now()
        + std::time::Duration::from_secs(crate::hook::HOOK_DEADLINE_SECS + 10);
    while child
        .try_wait()
        .inspect_err(|_| {
            let _ = child.kill();
        })
        .with_context(|| format!("waiting for the deny-list hook command `{command}`"))?
        .is_none()
    {
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "the deny-list hook command `{command}` hung in its self-test, so no agent \
                 will start; rebuild paja from main"
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let code = child
        .wait()
        .with_context(|| format!("waiting for the deny-list hook command `{command}`"))?
        .code();
    Ok((code, reader.join().unwrap_or_default()))
}

pub(super) fn tempfile_dir(near: &Path) -> Result<PathBuf> {
    // A fresh directory only this user can enter; creating it fails if the name is taken, so
    // nobody can prepare it beforehand.
    static COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "paja-agent-{}-{}-{}",
        std::process::id(),
        crate::events::now_ms(),
        COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::create_dir_all(std::env::temp_dir());
    crate::platform::create_private_dir(&dir).with_context(|| {
        format!(
            "creating a scratch home {} for work in {}; check that the temporary \
                 directory (TMPDIR, or TEMP on Windows) exists and is writable",
            dir.display(),
            near.display()
        )
    })?;
    Ok(dir)
}

/// Give `cmd` a throwaway PAJA_HOME (`scratch`) and keep GitHub Mestari's: take away the easy
/// ways an agent, or the code it wrote, could reach it (tokens, the SSH agent, gh's stored
/// login, git's credential helpers and askpass programs, and registry tokens). Agents keep
/// the owner's `HOME`, which Claude Code needs to sign in, so code they run can still read
/// credential files there (`.netrc`, say), and a token written into the remote's URL is not
/// covered. Determined code run by `cargo test` is the sandbox's job (`PAJA_SBX`).
pub(super) fn without_credentials(
    cmd: &mut Command,
    scratch: &Path,
    git_identity: Vec<(&str, String)>,
) {
    for (name, _) in std::env::vars_os() {
        let name = name.to_string_lossy();
        if name.starts_with("GIT_CONFIG_")
            || name.starts_with("CARGO_REGISTRIES_") && name.ends_with("_TOKEN")
        {
            cmd.env_remove(name.as_ref());
        }
    }
    cmd.env("PAJA_HOME", scratch)
        // Replacement refs would let code show the reviewer a different history.
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_GRAFT_FILE", "/dev/null")
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .env_remove("GH_ENTERPRISE_TOKEN")
        .env_remove("GITHUB_ENTERPRISE_TOKEN")
        .env_remove("PAJA_GH")
        .env_remove("SSH_AUTH_SOCK")
        .env_remove("CARGO_REGISTRY_TOKEN")
        .env("GH_CONFIG_DIR", scratch)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ASKPASS", "false")
        .env("SSH_ASKPASS", "false")
        .env("GIT_SSH_COMMAND", "false")
        // Configuration from the environment comes last, and an empty helper clears the list
        // so far, so no credential helper from the repository's own config runs either.
        .env("GIT_CONFIG_COUNT", "2")
        .env("GIT_CONFIG_KEY_0", "credential.helper")
        .env("GIT_CONFIG_VALUE_0", "")
        // Without the global config, commits still need the owner's name and email.
        .envs(git_identity);
}

/// A snapshot of the git configuration, hooks and attributes a worktree's git reads, all of
/// them outside the worktree and shared with the repository. Code that an agent, Vasara's
/// checks or a reviewer's build runs could change them to make Mestari's own git run a program
/// with Mestari's credentials (a credential helper, a filter). Mestari takes the snapshot before
/// such code runs and checks it before it runs git there again.
pub(super) struct GitGuard {
    files: Vec<PathBuf>,
    /// Watched files that are not git configuration, compared byte for byte.
    raw: Vec<PathBuf>,
    hooks: Vec<PathBuf>,
    snapshot: Vec<(PathBuf, Option<Vec<u8>>)>,
    /// Every other worktree's links and configuration: code here could repoint another
    /// environment's git. Those come and go as other runs work, so only a file that still
    /// exists and now differs counts.
    others: Vec<PathBuf>,
    others_snapshot: Vec<Option<Vec<u8>>>,
}

impl GitGuard {
    pub(super) fn new(worktree: &Path) -> Result<GitGuard> {
        let dir = |arg: &str| -> Result<PathBuf> {
            let path = PathBuf::from(git(worktree, &["rev-parse", arg])?);
            Ok(if path.is_absolute() {
                path
            } else {
                worktree.join(path)
            })
        };
        let (own, common) = (dir("--absolute-git-dir")?, dir("--git-common-dir")?);
        // The repository's hooks, and wherever the owner's `core.hooksPath` points: gh runs
        // git without Paja's overrides.
        let mut hooks = vec![common.join("hooks")];
        // A relative path in the configuration is relative to where git runs: gh runs it in the
        // main checkout, next to the shared git directory, not in this worktree.
        let main_checkout = common.parent().unwrap_or(&common).to_path_buf();
        // Asked without Paja's own override, which would answer `/dev/null`. Reading the
        // configuration runs no hook.
        let owner_hooks = Command::new("git")
            .arg("-C")
            .arg(worktree)
            .args(["config", "--path", "core.hooksPath"])
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .unwrap_or_default();
        if !owner_hooks.is_empty() {
            hooks.push(main_checkout.join(owner_hooks));
        }
        // The owner's global configuration, which Mestari's own git reads.
        let home = crate::platform::home_dir();
        let xdg = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| home.as_ref().map(|h| h.join(".config")));
        let global: Vec<PathBuf> = home
            .iter()
            .map(|h| h.join(".gitconfig"))
            .chain(
                xdg.iter()
                    .flat_map(|x| [x.join("git/config"), x.join("git/attributes")]),
            )
            .chain(std::env::var_os("GIT_CONFIG_GLOBAL").map(PathBuf::from))
            .collect();
        let mut guard = GitGuard {
            // The worktree's links to the repository too: rewriting them would point git at
            // another configuration without touching these files.
            files: vec![
                common.join("config"),
                common.join("config.worktree"),
                own.join("config.worktree"),
                common.join("info/attributes"),
                common.join("info/grafts"),
                worktree.join(".git"),
                own.join("commondir"),
                own.join("gitdir"),
                common.join("info/exclude"),
                // Sparse checkout's patterns decide which files may be skip-worktree; new
                // ones could hide a test from the checks.
                own.join("info/sparse-checkout"),
            ],
            raw: Vec::new(),
            hooks,
            snapshot: Vec::new(),
            others: Vec::new(),
            others_snapshot: Vec::new(),
        };
        guard.files.extend(global);
        guard.raw = guard
            .files
            .iter()
            .filter(|path| {
                !(path.ends_with("config")
                    || path.ends_with("config.worktree")
                    || path.ends_with(".gitconfig"))
            })
            .cloned()
            .collect();
        // A global attributes file the owner chose, wherever it is.
        let attributes = Command::new("git")
            .arg("-C")
            .arg(worktree)
            .args(["config", "--path", "core.attributesFile"])
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .unwrap_or_default();
        if !attributes.is_empty() {
            // Mestari's own git reads a relative one here, gh's in the main checkout.
            for base in [worktree, main_checkout.as_path()] {
                let path = base.join(&attributes);
                // Attributes are compared byte for byte, even in a file also watched as
                // configuration: their lines aren't config entries.
                if !guard.raw.contains(&path) {
                    guard.raw.push(path.clone());
                }
                if !guard.files.contains(&path) {
                    guard.files.push(path);
                }
            }
        }
        // Every file git reads configuration from here, includes and the system file among
        // them, and every file an include names, read or not: one that doesn't exist yet
        // could be created.
        let origins = git(
            worktree,
            &["config", "--list", "--show-origin", "--name-only"],
        )?;
        for line in origins.lines() {
            if let Some(file) = line
                .strip_prefix("file:")
                .and_then(|l| l.split('\t').next())
            {
                let path = Path::new(file);
                let path = if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    worktree.join(path)
                };
                if !guard.files.contains(&path) {
                    guard.files.push(path);
                }
            }
        }
        let includes = git(
            worktree,
            &[
                "config",
                "--path",
                "--null",
                "--get-regexp",
                r"^include(if\..*)?\.path$",
            ],
        )
        .unwrap_or_default();
        for entry in includes.split('\0') {
            if let Some((_, file)) = entry.split_once('\n') {
                let path = PathBuf::from(file);
                // A relative include is relative to the file that names it: the repository's
                // config or the owner's global one, so watch it against both.
                let candidates: Vec<PathBuf> = if path.is_absolute() {
                    vec![path]
                } else {
                    [Some(common.clone()), home.clone()]
                        .into_iter()
                        .flatten()
                        .map(|base| base.join(&path))
                        .collect()
                };
                for path in candidates {
                    if !guard.files.contains(&path) {
                        guard.files.push(path);
                    }
                }
            }
        }
        if let Ok(entries) = std::fs::read_dir(common.join("worktrees")) {
            for entry in entries.flatten() {
                let dir = entry.path();
                // A worktree another run (a pool's other lane) is adding right now has no
                // `gitdir` or `commondir` with content yet, and gets them in a moment: that is
                // not a change to an existing worktree. Watching starts with its next snapshot.
                let written =
                    |name: &str| read_regular(&dir.join(name)).is_some_and(|b| !b.is_empty());
                if dir != own && !(written("gitdir") && written("commondir")) {
                    continue;
                }
                for name in ["gitdir", "commondir", "config.worktree"] {
                    guard.others.push(dir.join(name));
                }
                // `gitdir` names the worktree's `.git` file, which points back here.
                if let Some(link) = read_regular(&dir.join("gitdir")) {
                    let link = String::from_utf8_lossy(&link).trim().to_string();
                    if !link.is_empty() {
                        guard.others.push(PathBuf::from(link));
                    }
                }
            }
        }
        guard.others.retain(|path| !guard.files.contains(path));
        guard.others_snapshot = guard.others.iter().map(|p| read_regular(p)).collect();
        guard.snapshot = guard.read();
        Ok(guard)
    }

    pub(super) fn read(&self) -> Vec<(PathBuf, Option<Vec<u8>>)> {
        let mut hooks: Vec<PathBuf> = self
            .hooks
            .iter()
            .flat_map(|dir| {
                std::fs::read_dir(dir)
                    .map(|entries| entries.flatten().map(|e| e.path()).collect::<Vec<_>>())
                    .unwrap_or_default()
            })
            .collect();
        hooks.sort();
        self.files
            .iter()
            .chain(&hooks)
            .map(|path| {
                let raw = read_regular(path);
                let content = if self.raw.contains(path) || !self.files.contains(path) {
                    raw
                } else {
                    // A file git can't parse is kept whole, behind a marker no listing starts
                    // with, so the message never reads its text as entries.
                    raw.map(|bytes| {
                        config_entries(path)
                            .unwrap_or_else(|| [UNPARSED, bytes.as_slice()].concat())
                    })
                };
                (path.clone(), content)
            })
            .collect()
    }

    /// Fails, before Mestari runs git again, when anything changed since the snapshot.
    pub(super) fn check(&self, what: &str) -> Result<()> {
        let now = self.read();
        let others: Vec<String> = self
            .others
            .iter()
            .zip(&self.others_snapshot)
            .filter(|(path, before)| {
                let after = read_regular(path);
                after.is_some() && after != **before
            })
            .map(|(path, _)| path.display().to_string())
            .collect();
        if now != self.snapshot || !others.is_empty() {
            let before = |path: &PathBuf| self.snapshot.iter().find(|(p, _)| p == path);
            let mut changed: Vec<String> = now
                .iter()
                .filter(|entry| !self.snapshot.contains(entry))
                .chain(self.snapshot.iter().filter(|entry| !now.contains(entry)))
                .map(|(path, _)| path)
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .map(|path| {
                    let shown = path.display().to_string();
                    // A configuration file says which entries changed, so a person can tell
                    // what wrote it.
                    let after = now
                        .iter()
                        .find(|(p, _)| p == path)
                        .and_then(|(_, c)| c.as_ref());
                    let was = before(path).and_then(|(_, c)| c.as_ref());
                    match (was, after) {
                        (Some(was), Some(after))
                            if self.files.contains(path) && !self.raw.contains(path) =>
                        {
                            let entries = config_changes(was, after);
                            if entries.is_empty() {
                                shown
                            } else {
                                format!("{shown}: {}", entries.join("; "))
                            }
                        }
                        (None, Some(_)) => format!("{shown}: added"),
                        (Some(_), None) => format!("{shown}: removed"),
                        _ => shown,
                    }
                })
                .chain(others)
                .collect();
            changed.sort();
            changed.dedup();
            return Err(Tampered(format!(
                "git's configuration, hooks or attributes changed while {what} ran ({}), so \
                 Paja stopped before running git there again; a person must check and restore \
                 those files before running Paja again. The agent may have changed them, or \
                 another program writing to the same repository meanwhile (an editor, another \
                 session, a `git worktree add`), or your own edits",
                changed.join(", ")
            ))
            .into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_data_under_the_environments_gets_no_mount_of_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let (envs, repo) = (dir.path().join("envs"), dir.path().join("repo"));
        let scratch = envs.join("recommend-1-0").join(".git");
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        assert_eq!(sandbox_mounts(envs.clone(), scratch), vec![envs.clone()]);
        assert_eq!(
            sandbox_mounts(envs.clone(), repo.join(".git")),
            [envs, repo.join(".git")]
        );
    }

    #[test]
    fn a_stream_folds_into_its_result_with_the_latest_window() {
        let stream = [
            r#"{"type":"system","subtype":"init"}"#,
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","utilization":0.5,"resetsAt":1}}"#,
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","utilization":0.9,"resetsAt":2}}"#,
            r#"{"type":"result","is_error":false,"result":"done","num_turns":3}"#,
        ]
        .join("\n");
        let folded: Value = serde_json::from_slice(&fold_stream(stream.as_bytes())).unwrap();
        assert_eq!(folded["result"], "done");
        assert_eq!(folded["rate_limit_info"]["utilization"], 0.9);
        // A result's own object wins; a single JSON value and a stream with no result stay as is.
        let own = r#"{"type":"result","rate_limit_info":{"status":"allowed"}}"#;
        let both = format!(
            "{}\n{own}",
            r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected"}}"#
        );
        let folded: Value = serde_json::from_slice(&fold_stream(both.as_bytes())).unwrap();
        assert_eq!(folded["rate_limit_info"]["status"], "allowed");
        assert_eq!(fold_stream(own.as_bytes()), own.as_bytes());
        assert_eq!(fold_stream(b"a\nb"), b"a\nb");
    }

    #[test]
    fn agents_always_run_in_auto_mode_and_never_bypass_permissions() {
        for settings in [None, Some("{}".to_string())] {
            for read_only in [false, true] {
                let args = claude_args(
                    settings.clone(),
                    String::new(),
                    read_only,
                    &[],
                    &["Read"],
                    None,
                );
                let at = args.iter().position(|a| a == "--permission-mode").unwrap();
                assert_eq!(args[at + 1], "auto", "{args:?}");
                assert_eq!(args.iter().filter(|a| *a == "--permission-mode").count(), 1);
                assert!(args.iter().any(|a| a == "--tools"), "{args:?}");
                for arg in &args {
                    assert!(!arg.contains("bypassPermissions"), "{args:?}");
                    assert!(!arg.contains("dangerously-skip-permissions"), "{args:?}");
                    assert!(!arg.contains("acceptEdits"), "{args:?}");
                }
            }
        }
    }

    #[test]
    fn a_broken_hook_command_fails_the_self_test_and_is_named() {
        let tmp = tempfile::tempdir().unwrap();
        // The command Claude Code would run, pointing at a hook binary that isn't there.
        let command = hook_command(
            &tmp.path().join("no-such-paja"),
            tmp.path(),
            tmp.path(),
            &tmp.path().join("stamp"),
            false,
            &["cargo".to_string()],
        )
        .unwrap();
        let why = self_test_hook(&command, tmp.path(), false)
            .unwrap_err()
            .to_string();
        assert!(why.contains("failed its self-test"), "{why}");
        assert!(why.contains(&command), "{why}");
    }

    #[test]
    fn config_changes_name_each_entry_and_hide_credentials() {
        let listing = |entries: &[(&str, &str)]| -> Vec<u8> {
            entries
                .iter()
                .flat_map(|(n, v)| format!("{n}\n{v}\0").into_bytes())
                .collect()
        };
        let before = listing(&[
            ("core.repositoryformatversion", "0"),
            ("remote.origin.url", "https://example.invalid/r"),
            ("http.extraheader", "Authorization: old"),
        ]);
        let after = listing(&[
            ("core.repositoryformatversion", "1"),
            ("extensions.relativeworktrees", "true"),
            ("http.extraheader", "Authorization: new"),
        ]);
        let changes = config_changes(&before, &after);
        assert_eq!(
            changes,
            [
                "core.repositoryformatversion: `0` -> `1`",
                "extensions.relativeworktrees added (`true`)",
                "http.extraheader: (value hidden) -> (value hidden)",
                "remote.origin.url removed",
            ]
        );
        // No credential reaches the message, in a value or in a name.
        let secret = "ghs_s3cr3t";
        let with_url = listing(&[
            (
                "remote.origin.url",
                &format!("https://x-access-token:{secret}@example.invalid/r"),
            ),
            (
                "url.https://u:pw@example.invalid/.insteadof",
                "https://example.invalid/",
            ),
            ("core.sshcommand", &format!("ssh -o User=me@{secret}")),
        ]);
        let changes = config_changes(&before, &with_url).join("; ");
        assert!(
            !changes.contains(secret) && !changes.contains("pw@"),
            "{changes}"
        );
        assert!(
            changes.contains("url.(hidden).insteadof added"),
            "{changes}"
        );
        // A file git can't parse is named, not read as entries.
        let unparsed = [UNPARSED, b"[core]\n\teditor = x\n[broken\n".as_slice()].concat();
        assert!(config_changes(&before, &unparsed).is_empty());
        assert!(config_changes(&unparsed, &before).is_empty());
        assert!(config_changes(&before, &before).is_empty());
        let long = listing(&[("core.editor", &"x".repeat(200))]);
        let cut = config_changes(&[], &long);
        assert!(cut[0].ends_with("…`)"), "{cut:?}");
    }

    #[test]
    fn a_tamper_stop_names_the_configuration_entries_that_changed() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("main");
        std::fs::create_dir_all(&main).unwrap();
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(&main)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {}", tail(&out));
        };
        run(&["init", "-q"]);
        run(&[
            "-c",
            "user.name=paja",
            "-c",
            "user.email=paja@example.invalid",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "seed",
        ]);
        let worktree = tmp.path().join("work");
        run(&["worktree", "add", "-q", worktree.to_str().unwrap()]);
        let guard = GitGuard::new(&worktree).unwrap();
        guard.check("nothing").unwrap();
        // A branch entry is the agent's own business and still doesn't count.
        run(&["config", "branch.x.remote", "origin"]);
        guard.check("nothing").unwrap();
        run(&["config", "extensions.example", "true"]);
        let err = format!("{:#}", guard.check("the agent").unwrap_err());
        assert!(err.contains("extensions.example added (`true`)"), "{err}");
        assert!(!err.contains("branch.x"), "{err}");
        // A configuration git can't parse is named, and none of its text is shown.
        let guard = GitGuard::new(&worktree).unwrap();
        let config = git(&worktree, &["rev-parse", "--git-common-dir"]).unwrap();
        let config = worktree.join(config).join("config");
        let mut text = std::fs::read_to_string(&config).unwrap();
        text.push_str("\n[broken\n");
        std::fs::write(&config, text).unwrap();
        let err = format!("{:#}", guard.check("the agent").unwrap_err());
        assert!(err.contains("config"), "{err}");
        assert!(
            !err.contains("repositoryformatversion") && !err.contains("[core]"),
            "{err}"
        );
    }

    #[test]
    fn guard_ignores_a_worktree_being_added_but_not_a_changed_one() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let main = dir.join("main");
        std::fs::create_dir_all(&main).unwrap();
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(&main)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {}", tail(&out));
        };
        run(&["init", "-q"]);
        run(&[
            "-c",
            "user.name=paja",
            "-c",
            "user.email=paja@example.invalid",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "seed",
        ]);
        let worktree = dir.join("work");
        run(&["worktree", "add", "-q", worktree.to_str().unwrap()]);
        run(&["worktree", "add", "-q", dir.join("other").to_str().unwrap()]);
        let admin = main.join(".git/worktrees");
        // Another lane's `git worktree add` caught halfway: `gitdir` written, `commondir` not.
        let adding = admin.join("adding");
        std::fs::create_dir_all(&adding).unwrap();
        std::fs::write(
            adding.join("gitdir"),
            dir.join("adding/.git").to_str().unwrap(),
        )
        .unwrap();
        let guard = GitGuard::new(&worktree).unwrap();
        std::fs::write(adding.join("commondir"), "../..\n").unwrap();
        guard.check("another run").unwrap();
        // A worktree that was complete is still watched.
        std::fs::write(admin.join("other/commondir"), "/elsewhere\n").unwrap();
        let err = format!("{:#}", guard.check("the agent").unwrap_err());
        assert!(err.contains("other"), "{err}");
    }

    #[test]
    fn guard_watches_a_relative_attributes_file_in_the_main_checkout() {
        // Removed when the test ends, passing or not.
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let main = dir.join("main");
        std::fs::create_dir_all(&main).unwrap();
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(&main)
                .args(args)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {}", tail(&out));
        };
        run(&["init", "-q"]);
        run(&[
            "-c",
            "user.name=paja",
            "-c",
            "user.email=paja@example.invalid",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "seed",
        ]);
        let worktree = dir.join("work");
        run(&["worktree", "add", "-q", worktree.to_str().unwrap()]);
        run(&["config", "core.attributesFile", "attrs"]);
        std::fs::write(main.join("attrs"), "").unwrap();
        std::fs::write(worktree.join("attrs"), "").unwrap();
        let guard = GitGuard::new(&worktree).unwrap();
        let canonical = |p: &Path| std::fs::canonicalize(p).ok();
        let watched: Vec<_> = guard.raw.iter().filter_map(|p| canonical(p)).collect();
        for expected in [main.join("attrs"), worktree.join("attrs")] {
            assert!(
                watched.contains(&canonical(&expected).unwrap()),
                "{} not in {watched:?}",
                expected.display()
            );
        }
        // An attributes file that is also watched as configuration is still compared byte
        // for byte.
        // Spelled as git spells the common directory, so it is the very path the guard
        // already watches as configuration.
        let common = git(
            &worktree,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )
        .unwrap();
        let config = format!("{common}/config");
        run(&["config", "core.attributesFile", &config]);
        let guard = GitGuard::new(&worktree).unwrap();
        assert!(guard.files.contains(&PathBuf::from(&common).join("config")));
        let watched: Vec<_> = guard.raw.iter().filter_map(|p| canonical(p)).collect();
        assert!(
            watched.contains(&canonical(Path::new(&config)).unwrap()),
            "{watched:?}"
        );
    }
}
