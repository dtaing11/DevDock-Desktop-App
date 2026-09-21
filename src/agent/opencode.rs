//! OpenCode as an engine: an open-source, provider-agnostic coding agent
//! run headless (`opencode run --format json`), the way Claude Code is.
//! It brings its own loop and tools and any model it knows — Anthropic's,
//! OpenAI's, Google's, its own free ones, a local Ollama model — and DevDock
//! wraps it the same way: permissions from a config of DevDock's own, the
//! repository's MCP servers handed over, images as attached files, a
//! question as a line at the end of a reply answered by resuming the
//! session, edits found by snapshot, and the whole thing inside the
//! sandbox when there is one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::claude_code::{edits_since, first_line, snapshot};
use super::{Event, Run, Usage};
use crate::git::Repo;

/// The provider id a model selection carries for this engine.
pub const PROVIDER: &str = "opencode";

/// A model that needs no sign-in: OpenCode's own hosted free tier, for a
/// first run before any provider is configured.
pub const DEFAULT_MODEL: &str = "opencode/big-pickle";

#[derive(Debug, Clone)]
pub struct Config {
    /// `provider/model`, as OpenCode names them; empty means its default.
    pub model: String,
    pub timeout: Duration,
    /// Run inside this sandbox instead of on this machine.
    pub sandbox: Option<std::sync::Arc<crate::sandbox::Sandbox>>,
}

impl Default for Config {
    fn default() -> Self {
        Self { model: String::new(), timeout: Duration::from_secs(3 * 60 * 60), sandbox: None }
    }
}

/// Where the `opencode` command is: on the PATH, or where its installer
/// puts it.
pub fn program() -> Option<PathBuf> {
    let path = crate::local_ci::runner::login_path();
    for dir in path.split(':') {
        let candidate = Path::new(dir).join("opencode");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    dirs::home_dir().map(|h| h.join(".opencode/bin/opencode")).filter(|p| p.is_file())
}

pub fn available() -> bool {
    program().is_some()
}

/// The models OpenCode knows, `provider/model`, from `opencode models`.
/// Empty when it is not installed or lists nothing.
pub fn models() -> Vec<String> {
    let Some(program) = program() else { return Vec::new() };
    let Ok(out) = Command::new(program).arg("models").stdin(Stdio::null()).stderr(Stdio::null()).output() else { return Vec::new() };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| l.contains('/') && !l.contains(' '))
        .map(str::to_string)
        .collect()
}

/// What a run may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permissions {
    /// Edit, and any shell command but git history, the web, and (on the
    /// host) sudo.
    Full,
    /// No edits; a shell for the repository's checks and reading only.
    ReadOnly,
}

/// The config DevDock hands OpenCode for one run: permissions, the
/// repository's MCP servers, no sharing, no updates.
pub fn run_config(root: &Path, permissions: Permissions, sandboxed: bool) -> serde_json::Value {
    let mut bash = serde_json::Map::new();
    match permissions {
        Permissions::Full => {
            bash.insert("*".into(), "allow".into());
        }
        Permissions::ReadOnly => {
            bash.insert("*".into(), "deny".into());
            for program in super::claude_code::toolchain(root, &[]) {
                bash.insert(format!("{program} *"), "allow".into());
                bash.insert(program, "allow".into());
            }
            for cmd in ["ls", "cat", "head", "tail", "wc", "grep", "rg", "find", "pwd", "echo", "diff", "sort", "uniq", "tree", "stat", "file", "git status", "git diff", "git log", "git show", "git ls-files", "git grep", "git blame", "git branch", "git rev-parse"] {
                bash.insert(format!("{cmd}*"), "allow".into());
            }
        }
    }
    for denied in ["git commit", "git push", "git reset", "git checkout", "git switch", "git rebase", "git merge", "git stash", "git cherry-pick", "git revert", "git tag", "git clean", "git worktree", "git remote", "git branch -d", "git branch -D", "git branch -m"] {
        bash.insert(format!("{denied}*"), "deny".into());
    }
    if !sandboxed {
        bash.insert("sudo*".into(), "deny".into());
    }
    let mut permission = serde_json::Map::new();
    permission.insert("edit".into(), if permissions == Permissions::Full { "allow" } else { "deny" }.into());
    permission.insert("bash".into(), serde_json::Value::Object(bash));
    permission.insert("webfetch".into(), "deny".into());
    permission.insert("websearch".into(), "deny".into());
    // Its own interactive question tool would wait on a person who is not
    // there; a question is a line at the end of the reply instead.
    permission.insert("question".into(), "deny".into());

    let mut mcp = serde_json::Map::new();
    for spec in super::mcp::declared(root) {
        let mut command = vec![spec.command.clone()];
        command.extend(spec.args.iter().cloned());
        mcp.insert(
            spec.name.clone(),
            serde_json::json!({"type": "local", "command": command, "environment": spec.env, "enabled": true}),
        );
    }
    let mut config = serde_json::json!({
        "$schema": "https://opencode.ai/config.json",
        "permission": permission,
        "share": "disabled",
        "autoupdate": false,
    });
    if !mcp.is_empty() {
        config["mcp"] = serde_json::Value::Object(mcp);
    }
    config
}

/// Where the run's config file goes, relative to the worktree.
pub const CONFIG_FILE: &str = ".devdock/opencode.json";

/// How one `opencode run` is launched.
pub struct Launch<'a> {
    pub task: &'a str,
    /// Guidance put before the task in the message: OpenCode takes no
    /// system prompt of the caller's in headless mode.
    pub instructions: Option<&'a str>,
    pub permissions: Permissions,
    /// Files to attach (images), as paths OpenCode can read where it runs.
    pub files: &'a [String],
    pub resume: Option<&'a str>,
    pub collect_edits: bool,
}

/// Runs OpenCode once. Returns the run and the session id for resuming.
pub fn run(config: &Config, root: &Path, launch: Launch<'_>, on_event: &mut dyn FnMut(Event)) -> Result<Run, String> {
    let sandboxed = config.sandbox.is_some();
    let repo = Repo::open(root).map_err(|e| e.to_string())?;
    let before = if launch.collect_edits { snapshot(&repo)? } else { Default::default() };

    // The config file, in the worktree so the sandbox sees it too; never
    // staged (.devdock is an artifact directory).
    let config_path = root.join(CONFIG_FILE);
    std::fs::create_dir_all(config_path.parent().unwrap()).map_err(|e| e.to_string())?;
    std::fs::write(&config_path, serde_json::to_string_pretty(&run_config(root, launch.permissions, sandboxed)).unwrap()).map_err(|e| e.to_string())?;
    let config_inner = match &config.sandbox {
        Some(sandbox) => format!("{}/{CONFIG_FILE}", sandbox.inner_root()),
        None => config_path.display().to_string(),
    };

    let message = match launch.instructions.map(str::trim).filter(|s| !s.is_empty()) {
        Some(extra) => format!("Instructions for this run:\n{extra}\n\n---\n\n{}", launch.task),
        None => launch.task.to_string(),
    };

    let dir = match &config.sandbox {
        Some(sandbox) => sandbox.inner_root().to_string(),
        None => root.display().to_string(),
    };
    let args = argv(config, launch.resume, &message, launch.files, Some(dir));
    let mut cmd = match &config.sandbox {
        Some(sandbox) => {
            let mut env = BTreeMap::new();
            env.insert("OPENCODE_CONFIG".to_string(), config_inner.clone());
            env.insert("NO_COLOR".to_string(), "1".to_string());
            sandbox.command("opencode", &args, &env, "")
        }
        None => {
            let program = program().ok_or("OpenCode is not installed on this machine (the `opencode` command was not found).")?;
            let mut c = Command::new(program);
            // `--dir` and PWD both: OpenCode reads the shell's PWD, which a
            // child's working directory does not update, and without them
            // it worked in the parent process's directory.
            c.args(&args)
                .current_dir(root)
                .env("PWD", root)
                .env("OPENCODE_CONFIG", &config_inner)
                .env("PATH", crate::local_ci::runner::login_path());
            c
        }
    };
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).env("NO_COLOR", "1");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|e| format!("could not start opencode: {e}"))?;
    let pid = child.id();
    let (tx, rx) = mpsc::channel::<String>();
    let stdout = child.stdout.take().ok_or("no stdout from opencode")?;
    std::thread::spawn(move || {
        use std::io::BufRead as _;
        for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let stderr = child.stderr.take().map(crate::local_ci::runner::PipeReader::start);

    let mut log = Vec::new();
    let mut outcome = Outcome::default();
    let deadline = Instant::now() + config.timeout;
    let mut timed_out = false;
    let stop = crate::cancel::token(root);
    let mut exited: Option<Instant> = None;
    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => {
                for event in parse_line(&line, root, &mut outcome) {
                    log.push(event.line());
                    on_event(event);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            // The process is over and has been quiet since: whatever still
            // holds its stdout — a daemon a command of its started — is not
            // going to say anything this run needs.
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if matches!(child.try_wait(), Ok(Some(_))) && exited.get_or_insert_with(Instant::now).elapsed() >= crate::local_ci::runner::PIPE_GRACE {
                    break;
                }
            }
        }
        if stop.is_stopped() {
            #[cfg(unix)]
            crate::local_ci::runner::kill_group(pid);
            let _ = child.kill();
            let _ = child.wait();
            return Err(crate::cancel::STOPPED.to_string());
        }
        if Instant::now() >= deadline {
            timed_out = true;
            #[cfg(unix)]
            crate::local_ci::runner::kill_group(pid);
            let _ = child.kill();
            break;
        }
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    let stderr = stderr.map(|r| r.finish(Instant::now() + crate::local_ci::runner::PIPE_GRACE)).unwrap_or_default();
    let _ = std::fs::remove_file(&config_path);
    if timed_out {
        return Err(format!("OpenCode ran for more than {} minutes and was stopped.", config.timeout.as_secs() / 60));
    }
    if let Some(error) = outcome.error.take() {
        return Err(format!("OpenCode: {error}"));
    }
    if !status.success() && outcome.text.is_none() {
        let tail: String = stderr.lines().rev().filter(|l| !l.trim().is_empty()).take(6).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
        return Err(format!("OpenCode exited with {status}: {tail}"));
    }
    let edits = if launch.collect_edits { edits_since(&repo, &before)? } else { Vec::new() };
    Ok(Run {
        text: outcome.text.unwrap_or_default(),
        edits,
        log,
        truncated: false,
        turns: outcome.steps,
        session: outcome.session_id,
        usage: Usage::default(),
    })
}

/// The arguments of one `opencode run`, on the host or inside a sandbox
/// (where `dir` is the worktree's inner path). The message goes before
/// the files: `--file` takes a list and would eat it.
fn argv(config: &Config, resume: Option<&str>, message: &str, files: &[String], dir: Option<String>) -> Vec<String> {
    let mut args: Vec<String> = vec!["run".into(), "--format".into(), "json".into(), "--auto".into()];
    if let Some(dir) = dir {
        args.push("--dir".into());
        args.push(dir);
    }
    let model = config.model.trim();
    if !model.is_empty() {
        args.push("--model".into());
        args.push(model.to_string());
    }
    if let Some(session) = resume {
        args.push("--session".into());
        args.push(session.to_string());
    }
    args.push(message.to_string());
    for file in files {
        args.push("--file".into());
        args.push(file.clone());
    }
    args
}

#[derive(Default)]
struct Outcome {
    text: Option<String>,
    session_id: Option<String>,
    steps: usize,
    error: Option<String>,
}

/// One line of `--format json` as events. The stream is one JSON object
/// per line: `text`, `tool_use` (a part with its tool and state), `step_start`,
/// `step_finish`, `reasoning`, `error`.
fn parse_line(line: &str, root: &Path, outcome: &mut Outcome) -> Vec<Event> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line.trim()) else { return Vec::new() };
    if let Some(id) = value.get("sessionID").and_then(|s| s.as_str()) {
        outcome.session_id = Some(id.to_string());
    }
    let part = value.get("part").cloned().unwrap_or_default();
    let mut events = Vec::new();
    match value.get("type").and_then(|t| t.as_str()).unwrap_or("") {
        "text" => {
            if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                if !text.trim().is_empty() {
                    outcome.text = Some(text.to_string());
                    events.push(Event::Thought(text.to_string()));
                }
            }
        }
        "step_finish" => outcome.steps += 1,
        "tool_use" | "tool" => {
            let tool = part.get("tool").and_then(|t| t.as_str()).unwrap_or("tool");
            let state = part.get("state").cloned().unwrap_or_default();
            let input = state.get("input").cloned().unwrap_or_default();
            let status = state.get("status").and_then(|s| s.as_str()).unwrap_or("");
            if status == "error" {
                let error = state.get("error").and_then(|e| e.as_str()).unwrap_or("error");
                let short = if error.contains("rule which prevents") { "denied by DevDock's rules".to_string() } else { first_line(error) };
                events.push(Event::Tool { summary: format!("tool error: {}: {short}", summarize(tool, &input, root)), is_error: true });
            } else if status == "completed" || status == "running" {
                events.push(Event::Tool { summary: summarize(tool, &input, root), is_error: false });
            }
        }
        "error" => {
            let message = value.pointer("/error/data/message").or_else(|| value.get("error")).map(|e| e.as_str().map(str::to_string).unwrap_or_else(|| e.to_string())).unwrap_or_else(|| "error".into());
            outcome.error = Some(first_line(&message));
        }
        _ => {}
    }
    events
}

/// One line for a tool call, in the words the harness's log uses.
fn summarize(tool: &str, input: &serde_json::Value, root: &Path) -> String {
    let path = |key: &str| {
        input.get(key).and_then(|v| v.as_str()).map(|p| Path::new(p).strip_prefix(root).map(|r| r.display().to_string()).unwrap_or_else(|_| p.to_string())).unwrap_or_default()
    };
    match tool {
        "bash" => {
            let command = input.get("command").and_then(|c| c.as_str()).unwrap_or("");
            let short: String = command.chars().take(100).collect();
            format!("run `{short}{}`", if short.chars().count() < command.chars().count() { "…" } else { "" })
        }
        "read" => format!("read {}", path("filePath")),
        "write" => format!("write {}", path("filePath")),
        "edit" => format!("edit {}", path("filePath")),
        "glob" => format!("glob {}", input.get("pattern").and_then(|p| p.as_str()).unwrap_or("")),
        "grep" => format!("search \"{}\"", input.get("pattern").and_then(|p| p.as_str()).unwrap_or("")),
        "list" => format!("list {}", path("path")),
        "todowrite" | "todoread" => "plan".into(),
        "task" => format!("subagent: {}", input.get("description").and_then(|d| d.as_str()).unwrap_or("")),
        other => other.to_string(),
    }
}

/// Gives OpenCode the developer's Anthropic sign-in, from DevDock's own
/// store: the current access token, no refresh token (that stays
/// DevDock's — a refresh token used by two clients breaks one of them),
/// written into OpenCode's credentials file where it runs. `Ok(false)`
/// when DevDock has no Anthropic sign-in.
pub fn seed_anthropic_auth(sandbox: Option<&crate::sandbox::Sandbox>) -> Result<bool, String> {
    let Some(client) = crate::claude::Client::from_store("claude-sonnet-4-5") else { return Ok(false) };
    let Some((access, expires_at)) = client.oauth_access() else { return Ok(false) };
    let entry = serde_json::json!({"type": "oauth", "refresh": "", "access": access, "expires": expires_at * 1000});
    match sandbox {
        None => {
            let dir = dirs::home_dir().ok_or("no home directory")?.join(".local/share/opencode");
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            let path = dir.join("auth.json");
            let mut existing: serde_json::Value = std::fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_else(|| serde_json::json!({}));
            if existing.get("anthropic").and_then(|a| a.get("refresh")).and_then(|r| r.as_str()).is_some_and(|r| !r.is_empty()) {
                // OpenCode has its own sign-in here; leave it alone.
                return Ok(false);
            }
            existing["anthropic"] = entry;
            std::fs::write(&path, serde_json::to_string(&existing).unwrap()).map_err(|e| e.to_string())?;
            Ok(true)
        }
        Some(sandbox) => {
            let script = "mkdir -p \"$HOME/.local/share/opencode\" && f=\"$HOME/.local/share/opencode/auth.json\" && umask 077 && cat > \"$f.devdock\" && if [ -s \"$f\" ] && grep -q '\"refresh\":\"[^\"]' \"$f\" 2>/dev/null; then rm -f \"$f.devdock\"; else python3 - \"$f\" \"$f.devdock\" <<'PY'\nimport json,sys\np,n=sys.argv[1],sys.argv[2]\ntry: d=json.load(open(p))\nexcept Exception: d={}\nd['anthropic']=json.load(open(n))\njson.dump(d,open(p,'w'))\nPY\nrm -f \"$f.devdock\"; fi";
            let mut cmd = sandbox.command("sh", &["-c".to_string(), script.to_string()], &Default::default(), "");
            cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped());
            let mut child = cmd.spawn().map_err(|e| e.to_string())?;
            {
                use std::io::Write as _;
                let mut stdin = child.stdin.take().ok_or("no stdin")?;
                stdin.write_all(entry.to_string().as_bytes()).map_err(|e| e.to_string())?;
            }
            let out = child.wait_with_output().map_err(|e| e.to_string())?;
            if !out.status.success() {
                return Err(format!("could not seed OpenCode's sign-in: {}", String::from_utf8_lossy(&out.stderr).trim()));
            }
            Ok(true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_run_config_allows_a_shell_and_denies_history_and_the_web() {
        let tmp = tempfile::tempdir().unwrap();
        let full = run_config(tmp.path(), Permissions::Full, false);
        assert_eq!(full["permission"]["edit"], "allow");
        assert_eq!(full["permission"]["bash"]["*"], "allow");
        assert_eq!(full["permission"]["bash"]["git push*"], "deny");
        assert_eq!(full["permission"]["bash"]["sudo*"], "deny");
        assert_eq!(full["permission"]["webfetch"], "deny");
        assert_eq!(full["permission"]["question"], "deny");
        assert!(full.get("mcp").is_none());
        let inside = run_config(tmp.path(), Permissions::Full, true);
        assert!(inside["permission"]["bash"].get("sudo*").is_none(), "root is the point of a sandbox");

        std::fs::write(tmp.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        std::fs::write(tmp.path().join(".mcp.json"), r#"{"mcpServers": {"adder": {"command": "python3", "args": ["adder.py"], "env": {"A": "1"}}}}"#).unwrap();
        let readonly = run_config(tmp.path(), Permissions::ReadOnly, false);
        assert_eq!(readonly["permission"]["edit"], "deny");
        assert_eq!(readonly["permission"]["bash"]["*"], "deny");
        assert_eq!(readonly["permission"]["bash"]["cargo *"], "allow");
        assert_eq!(readonly["permission"]["bash"]["git diff*"], "allow");
        assert_eq!(readonly["mcp"]["adder"]["command"], serde_json::json!(["python3", "adder.py"]));
        assert_eq!(readonly["mcp"]["adder"]["environment"]["A"], "1");
    }

    #[test]
    fn the_stream_becomes_events_and_a_result() {
        let root = Path::new("/w");
        let mut outcome = Outcome::default();
        let lines = [
            r#"{"type":"step_start","sessionID":"ses_1","part":{"type":"step-start"}}"#,
            r#"{"type":"tool_use","sessionID":"ses_1","part":{"type":"tool","tool":"bash","state":{"status":"completed","input":{"command":"echo permitted"},"output":"permitted\n"}}}"#,
            r#"{"type":"tool_use","sessionID":"ses_1","part":{"type":"tool","tool":"bash","state":{"status":"error","input":{"command":"git push origin main"},"error":"The user has specified a rule which prevents you from using this specific tool call."}}}"#,
            r#"{"type":"tool_use","sessionID":"ses_1","part":{"type":"tool","tool":"read","state":{"status":"completed","input":{"filePath":"/w/src/lib.rs"}}}}"#,
            r#"{"type":"step_finish","sessionID":"ses_1","part":{"reason":"tool-calls"}}"#,
            r#"{"type":"text","sessionID":"ses_1","part":{"type":"text","text":"Done. Verified: cargo test"}}"#,
            r#"{"type":"step_finish","sessionID":"ses_1","part":{"reason":"stop"}}"#,
            "not json",
        ];
        let mut events = Vec::new();
        for line in lines {
            events.extend(parse_line(line, root, &mut outcome));
        }
        let lines: Vec<String> = events.iter().map(|e| e.line()).collect();
        assert_eq!(lines[0], "· run `echo permitted`");
        assert!(lines[1].starts_with("! tool error: run `git push origin main`: denied by DevDock's rules"), "{}", lines[1]);
        assert_eq!(lines[2], "· read src/lib.rs");
        assert_eq!(outcome.text.as_deref(), Some("Done. Verified: cargo test"));
        assert_eq!(outcome.session_id.as_deref(), Some("ses_1"));
        assert_eq!(outcome.steps, 2);
        let mut failed = Outcome::default();
        parse_line(r#"{"type":"error","error":{"name":"ProviderAuthError","data":{"message":"not signed in"}}}"#, root, &mut failed);
        assert_eq!(failed.error.as_deref(), Some("not signed in"));
    }

    #[test]
    fn the_arguments_carry_the_run_wherever_it_starts() {
        let config = Config { model: "opencode/big-pickle".into(), ..Default::default() };
        let args = argv(&config, Some("ses_1"), "do it", &["/work/.devdock/prompt-images/1-a.png".into()], Some("/work".into()));
        assert_eq!(args[..7], ["run", "--format", "json", "--auto", "--dir", "/work", "--model"]);
        assert!(args.contains(&"ses_1".to_string()));
        let at = args.iter().position(|a| a == "do it").unwrap();
        assert_eq!(args[at + 1], "--file", "files come after the message");
        // Through a sandbox, every argument reaches opencode, not the shell.
        let quoted = crate::sandbox::shell_words(&args);
        assert!(quoted.contains("'run' '--format' 'json'"), "{quoted}");
    }

    #[test]
    fn a_missing_opencode_is_a_clear_error() {
        let tmp = tempfile::tempdir().unwrap();
        let out = Command::new("git").args(["init", "-q"]).current_dir(tmp.path()).output().unwrap();
        assert!(out.status.success());
        if available() {
            eprintln!("opencode is installed here; the missing case is not testable");
            return;
        }
        let err = run(&Config::default(), tmp.path(), Launch { task: "x", instructions: None, permissions: Permissions::Full, files: &[], resume: None, collect_edits: false }, &mut |_| {}).unwrap_err();
        assert!(err.contains("not installed"), "{err}");
    }
}
