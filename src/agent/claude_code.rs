//! Claude Code as the engine: the `claude` command line, run headless in the
//! working tree, instead of this crate's own tool-use loop.
//!
//! Anthropic's agent already knows how to read, edit, search, and run
//! commands, and someone who uses it every day may want exactly that agent
//! behind the Agent tab and the backlog fixer. This module drives it the
//! way a script would — `claude -p <task> --output-format stream-json` —
//! and turns what it streams into the same [`Event`]s the built-in harness
//! emits, so the panel, the log, and the review at the end are the same
//! whichever engine did the work.
//!
//! Two things are different, and both are structural:
//!
//! - **It writes to disk.** There is no overlay; the run has to be live.
//!   Changes are found afterwards by comparing the tree with a snapshot
//!   taken before the run, which is what makes Keep and Revert work.
//! - **Commands are limited to the repository's checks.** Claude Code's
//!   `Bash` tool is allowed only the commands `.git-manage-ci.toml`
//!   declares (`Bash(cargo test:*)`, say), and its web tools are off. That
//!   is the same rule the built-in harness lives by: no arbitrary commands.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::{Event, PendingEdit, PlanStep, Run, Usage};
use crate::git::Repo;

/// Model aliases the CLI accepts. "default" sends no `--model` at all; the
/// CLI also takes any full model id (`claude-opus-5`, `claude-fable-5-1`),
/// which the picker lists from the account's own model list, so a family
/// the aliases do not cover — and the exact version — can be chosen.
pub const MODELS: &[&str] = &["default", "sonnet", "opus", "haiku"];

/// The provider id the model picker writes for this engine.
pub const PROVIDER: &str = "claude-code";

/// How a run is bounded.
#[derive(Debug, Clone)]
pub struct Config {
    /// Run `claude` inside this sandbox instead of on this machine: its
    /// shell, its file edits and its MCP servers are then contained too.
    pub sandbox: Option<std::sync::Arc<crate::sandbox::Sandbox>>,
    /// A model alias or id; empty or "default" leaves the CLI's choice.
    pub model: String,
    pub max_turns: usize,
    pub timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self { model: String::new(), max_turns: 60, timeout: Duration::from_secs(30 * 60), sandbox: None }
    }
}

/// Where the `claude` command is: `PATH`, then where its installers put it.
/// A GUI started from the Dock does not get the shell's `PATH`.
pub fn program() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("claude");
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut candidates: Vec<PathBuf> =
        vec!["/opt/homebrew/bin/claude".into(), "/usr/local/bin/claude".into()];
    if let Some(home) = home {
        candidates.push(home.join(".claude/local/claude"));
        candidates.push(home.join(".local/bin/claude"));
    }
    candidates.into_iter().find(|p| p.is_file())
}

pub fn available() -> bool {
    program().is_some()
}

/// What an unattended run is never allowed, whatever else it may do: web
/// tools, and git commands that commit, push, or rewrite history — the
/// harness or the developer does those. Deny rules beat allow rules in
/// Claude Code, so `Bash` can be open and these still hold.
pub const DENIED_TOOLS: &[&str] = &[
    "WebFetch",
    "WebSearch",
    "Bash(git commit:*)",
    "Bash(git push:*)",
    "Bash(git reset:*)",
    "Bash(git checkout:*)",
    "Bash(git switch:*)",
    "Bash(git rebase:*)",
    "Bash(git merge:*)",
    "Bash(git stash:*)",
    "Bash(git cherry-pick:*)",
    "Bash(git revert:*)",
    "Bash(git tag:*)",
    "Bash(git clean:*)",
    "Bash(git worktree:*)",
    "Bash(git remote:*)",
    "Bash(git branch:*)",
    "Bash(sudo:*)",
];

/// What a run tells Claude Code about MCP: which config to load, and the
/// allow rules for the servers in it.
pub struct McpLaunch {
    /// The `--mcp-config` value: the repository's `.mcp.json`, when it has one.
    pub config: Option<String>,
    /// `mcp__<server>` for each declared server: all of its tools.
    pub allow_rules: Vec<String>,
}

/// The repository's `.mcp.json` servers, when the file is there and parses.
pub fn mcp_servers(root: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(root.join(".mcp.json")) else { return Vec::new() };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else { return Vec::new() };
    value
        .get("mcpServers")
        .and_then(|s| s.as_object())
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

pub fn mcp_launch(root: &Path) -> McpLaunch {
    let servers = mcp_servers(root);
    if servers.is_empty() {
        return McpLaunch { config: None, allow_rules: Vec::new() };
    }
    McpLaunch {
        config: Some(root.join(".mcp.json").display().to_string()),
        allow_rules: servers.iter().map(|s| format!("mcp__{s}")).collect(),
    }
}

/// The MCP server a permission refusal was about, from Claude Code's
/// wording: "Claude requested permissions to use mcp__plugin_x_y__tool,
/// but you haven't granted it yet."
fn denied_mcp_server(text: &str) -> Option<String> {
    let at = text.find("mcp__")?;
    let name: String = text[at..].chars().take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')).collect();
    // `mcp__<server>__<tool>`: the server is up to the second `__`.
    let rest = name.strip_prefix("mcp__")?;
    let server = rest.split("__").next().filter(|s| !s.is_empty())?;
    Some(format!("mcp__{server}"))
}

/// The `--disallowedTools` value.
pub fn disallowed_tools() -> String {
    DENIED_TOOLS.join(",")
}

/// The `--allowedTools` value: the editing and reading tools, and `Bash` —
/// the whole of it, less [`DENIED_TOOLS`] — so the agent can build, test,
/// format, and install with whatever the repository uses.
pub fn allowed_tools() -> String {
    ["Read", "Edit", "Write", "MultiEdit", "Grep", "Glob", "LS", "TodoWrite", "NotebookEdit", "Bash"].join(",")
}

/// The programs the repository's checks and toolchains use, for the note
/// in the system prompt: what is worth reaching for first.
pub fn toolchain(root: &Path, check_commands: &[String]) -> Vec<String> {
    let mut seen = std::collections::BTreeSet::new();
    for command in check_commands {
        for segment in command.split("&&") {
            if let Some(word) = segment.split_whitespace().next().filter(|w| *w != "cd") {
                seen.insert(word.to_string());
            }
        }
    }
    for tool in crate::local_ci::toolchain_commands(root) {
        seen.insert(tool.to_string());
    }
    seen.into_iter().collect()
}

/// A sentence for the system prompt saying what may be run and what may
/// not, so the model does not spend turns on what will be denied.
pub fn allowed_commands_note(root: &Path, check_commands: &[String]) -> String {
    let tools = toolchain(root, check_commands);
    let mut note = String::from("Commands: you may run shell commands");
    if !tools.is_empty() {
        note.push_str(&format!(" — this repository is driven with {}", tools.join(", ")));
    }
    note.push_str(
        ". Git commands that commit, push, or rewrite history, and the web, are denied, and \
         nobody is here to approve a denied command, so do not retry one — say what you could \
         not do in your summary. Do not commit or push: that is done for you when you finish. \
         Run every command to completion in the foreground and read its output: never start \
         one in the background, never `sleep`, never poll a log in a loop — `sleep` is \
         refused here, and a refusal is final. A build or a test suite that takes minutes \
         is fine to wait on.",
    );
    let servers = mcp_servers(root);
    if servers.is_empty() {
        note.push_str(" MCP tools, when you have any, may be used; one that is refused the first time is allowed and you are resumed — do not retry it yourself, carry on with the shell and it will be offered again.");
    } else {
        note.push_str(&format!(
            " The repository's MCP servers are loaded and their tools allowed: {}. Any other MCP tool that is refused once is allowed on resume; do not retry it yourself.",
            servers.join(", ")
        ));
    }
    note
}

/// Runs Claude Code on `task` in `root`, live, and reports what changed.
///
/// `system_extra` goes in as `--append-system-prompt`: project guidance and
/// the rules of an unattended run. It may run any command but the ones in
/// [`DENIED_TOOLS`]; `check_commands` are what the caller expects it to use.
pub fn run(
    config: &Config,
    root: &Path,
    task: &str,
    system_extra: Option<&str>,
    check_commands: &[String],
    on_event: &mut dyn FnMut(Event),
) -> Result<Run, String> {
    let _ = check_commands;
    let mut allowed = allowed_tools();
    let (mut run, mut denied) = run_with_tools(config, root, Launch { task, system_extra, allowed: &allowed, collect_edits: true, resume: None }, on_event)?;
    // A server DevDock could not list in advance — a plugin's, a user-level
    // one — shows up as a refusal the first time the model reaches for it.
    // Allow it and resume the same session, a few servers at most.
    for _ in 0..3 {
        if denied.is_empty() {
            break;
        }
        let Some(session) = run.session.clone() else { break };
        for server in &denied {
            if !allowed.split(',').any(|a| a == server) {
                allowed.push(',');
                allowed.push_str(server);
            }
        }
        on_event(Event::Tool { summary: format!("allowed MCP: {}; resuming", denied.join(", ")), is_error: false });
        let prompt = format!(
            "The MCP tools you were refused ({}) are allowed now. Continue the task, using them where they help.",
            denied.join(", ")
        );
        let (more, denied_again) = run_with_tools(config, root, Launch { task: &prompt, system_extra, allowed: &allowed, collect_edits: true, resume: Some(&session) }, on_event)?;
        run.text = more.text;
        run.session = more.session;
        run.turns += more.turns;
        run.truncated = more.truncated;
        run.usage = run.usage.plus(more.usage);
        run.log.extend(more.log);
        run.edits = more.edits;
        denied = denied_again;
    }
    Ok(run)
}

/// Runs Claude Code to read and run, not edit — no editing tools, and told
/// that any change it makes is put back — and returns what it said. For a
/// review, or advice. The caller keeps the tree.
pub fn run_readonly(
    config: &Config,
    root: &Path,
    task: &str,
    system_extra: Option<&str>,
    on_event: &mut dyn FnMut(Event),
) -> Result<Run, String> {
    let allowed = reviewer_tools();
    let mut system = String::from(
        "Read the repository and run what you need — the checks, the tools, anything that \
         answers a question — but do not edit: you have no editing tools, and any change \
         a command of yours makes to the tree is put back after you answer.",
    );
    if let Some(extra) = system_extra.map(str::trim).filter(|s| !s.is_empty()) {
        system.push_str("\n\n");
        system.push_str(extra);
    }
    run_with_tools(config, root, Launch { task, system_extra: Some(&system), allowed: &allowed, collect_edits: false, resume: None }, on_event).map(|(run, _)| run)
}

/// What a reviewer may use: the reading tools and the whole shell, less
/// [`DENIED_TOOLS`]. A list of permitted commands was tried first, and
/// every review found one it needed that was not on it — a pipeline, a VM's
/// status — and spent its turns being refused. Edits are the caller's to
/// undo, and are.
pub fn reviewer_tools() -> String {
    ["Read", "Grep", "Glob", "LS", "Bash"].join(",")
}

/// Directories Claude Code may read besides the worktree: the caches
/// where a dependency's source lives — cargo's registry, pub's cache, the
/// Flutter SDK, Go's module cache — so "how does this crate's type work"
/// is a read, not a refusal. Only the ones that exist.
pub fn extra_read_dirs(sandbox: Option<&crate::sandbox::Sandbox>) -> Vec<String> {
    const RELATIVE: &[&str] = &[".cargo/registry/src", ".cargo/git/checkouts", ".rustup/toolchains", ".pub-cache", "flutter", "go/pkg/mod", ".npm/_npx", ".gem"];
    match sandbox {
        Some(sandbox) => {
            let probe = RELATIVE.iter().map(|d| format!("test -d \"$HOME/{d}\" && echo \"$HOME/{d}\"")).collect::<Vec<_>>().join("; ");
            let probe = format!("{probe}; test -n \"$FLUTTER_ROOT\" && test -d \"$FLUTTER_ROOT\" && echo \"$FLUTTER_ROOT\"; true");
            sandbox
                .exec(&probe, "", &[], Some(std::time::Duration::from_secs(20)))
                .map(|o| o.stdout.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect())
                .unwrap_or_default()
        }
        None => {
            let Some(home) = dirs::home_dir() else { return Vec::new() };
            let mut dirs: Vec<String> = RELATIVE.iter().map(|d| home.join(d)).filter(|p| p.is_dir()).map(|p| p.display().to_string()).collect();
            if let Ok(root) = std::env::var("FLUTTER_ROOT") {
                if Path::new(&root).is_dir() && !dirs.contains(&root) {
                    dirs.push(root);
                }
            }
            dirs
        }
    }
}

/// What the file tools reach, for the system prompt: the worktree and
/// the toolchain caches, nothing else on the machine.
fn reach_note(extra_dirs: &[String]) -> String {
    let caches = if extra_dirs.is_empty() {
        String::new()
    } else {
        format!(" and, read-only in practice, the toolchain caches at {}", extra_dirs.join(", "))
    };
    format!(
        "Your file and shell tools reach the working directory (this repository's worktree){caches}. \
         Nothing else on this machine is in reach — other repositories, the home directory's \
         configuration, VM or container state such as ~/.lima — and an attempt is refused, so do not \
         make one. If the task needs something from outside, ask the developer for it."
    )
}

/// Continues a session — after a question was answered — with `prompt`
/// as the next message; the same tools as [`run`].
pub fn resume(
    config: &Config,
    root: &Path,
    session_id: &str,
    prompt: &str,
    system_extra: Option<&str>,
    on_event: &mut dyn FnMut(Event),
) -> Result<Run, String> {
    run_with_tools(config, root, Launch { task: prompt, system_extra, allowed: &allowed_tools(), collect_edits: true, resume: Some(session_id) }, on_event).map(|(run, _)| run)
}

/// How one `claude -p` is launched.
struct Launch<'a> {
    task: &'a str,
    system_extra: Option<&'a str>,
    allowed: &'a str,
    collect_edits: bool,
    /// A session to continue instead of starting one.
    resume: Option<&'a str>,
}

/// One `claude -p`: the run, and the MCP servers it was refused.
fn run_with_tools(
    config: &Config,
    root: &Path,
    launch: Launch<'_>,
    on_event: &mut dyn FnMut(Event),
) -> Result<(Run, Vec<String>), String> {
    let Launch { task, system_extra, allowed, collect_edits, resume } = launch;
    let allowed = {
        let extra = mcp_launch(root).allow_rules;
        if extra.is_empty() { allowed.to_string() } else { format!("{allowed},{}", extra.join(",")) }
    };
    let allowed = allowed.as_str();
    let program = program().ok_or(
        "Claude Code is not installed on this machine (the `claude` command was not found).",
    )?;
    let repo = Repo::open(root).map_err(|e| e.to_string())?;
    let before = if collect_edits { snapshot(&repo)? } else { Default::default() };

    // Every argument first, then the command: through a sandbox the
    // arguments must go into the inner script, not onto the shell that
    // starts it.
    let mut args: Vec<String> = vec![
        "-p".into(),
        task.to_string(),
        "--output-format".into(),
        "stream-json".into(),
        "--verbose".into(),
        "--permission-mode".into(),
        "acceptEdits".into(),
        "--max-turns".into(),
        config.max_turns.to_string(),
        "--allowedTools".into(),
        allowed.to_string(),
        "--disallowedTools".into(),
        disallowed_tools(),
    ];
    let mcp = mcp_launch(root);
    if mcp.config.is_some() {
        let config_path = match &config.sandbox {
            Some(sandbox) => format!("{}/.mcp.json", sandbox.inner_root()),
            None => root.join(".mcp.json").display().to_string(),
        };
        args.push("--mcp-config".into());
        args.push(config_path);
    }
    let model = config.model.trim();
    if !model.is_empty() && model != "default" {
        args.push("--model".into());
        args.push(model.to_string());
    }
    if let Some(id) = resume {
        args.push("--resume".into());
        args.push(id.to_string());
    }
    let extra_dirs = extra_read_dirs(config.sandbox.as_deref());
    for dir in &extra_dirs {
        args.push("--add-dir".into());
        args.push(dir.clone());
    }
    // Said up front, so a run does not spend turns finding out by being
    // refused: what its file tools can reach, and what they cannot.
    let mut system = reach_note(&extra_dirs);
    if let Some(extra) = system_extra.map(str::trim).filter(|s| !s.is_empty()) {
        system.push_str("\n\n");
        system.push_str(extra);
    }
    args.push("--append-system-prompt".into());
    args.push(system);
    // Inside the sandbox, `claude` is the one provisioned there and the
    // worktree is at its inner path; on the host, the installed one.
    let mut cmd = match &config.sandbox {
        Some(sandbox) => {
            let mut env = std::collections::BTreeMap::new();
            env.insert("CLAUDE_CODE_ENTRYPOINT".to_string(), "devdock".to_string());
            env.insert("NO_COLOR".to_string(), "1".to_string());
            sandbox.command("claude", &args, &env, "")
        }
        None => {
            let mut c = Command::new(&program);
            c.args(&args);
            c
        }
    };
    if config.sandbox.is_none() {
        cmd.current_dir(root);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Not our terminal, and not an interactive session's settings.
        .env("CLAUDE_CODE_ENTRYPOINT", "devdock")
        .env("NO_COLOR", "1");
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|e| format!("could not start {}: {e}", program.display()))?;
    let pid = child.id();

    // stdout streamed line by line; stderr collected for the error message.
    let (tx, rx) = mpsc::channel::<String>();
    let stdout = child.stdout.take().ok_or("no stdout from claude")?;
    std::thread::spawn(move || {
        use std::io::BufRead as _;
        for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let stderr = child.stderr.take().map(|mut pipe| {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = std::io::Read::read_to_end(&mut pipe, &mut buf);
            String::from_utf8_lossy(&buf).into_owned()
        })
    });

    let mut log: Vec<String> = Vec::new();
    let mut outcome = Outcome::default();
    let deadline = Instant::now() + config.timeout;
    let mut timed_out = false;
    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => {
                for event in parse_line(&line, root, &mut outcome) {
                    log.push(event.line());
                    on_event(event);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if Instant::now() >= deadline {
            timed_out = true;
            #[cfg(unix)]
            // SAFETY: a plain signal to a process group this process created.
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
            let _ = child.kill();
            break;
        }
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    let stderr = stderr.and_then(|h| h.join().ok()).unwrap_or_default();
    if timed_out {
        return Err(format!(
            "Claude Code ran longer than {}s and was stopped.",
            config.timeout.as_secs()
        ));
    }
    if outcome.result.is_none() {
        let tail: String = stderr.lines().rev().take(8).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
        return Err(format!(
            "Claude Code exited ({}) without a result.{}",
            status.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into()),
            if tail.trim().is_empty() { String::new() } else { format!("\n{tail}") }
        ));
    }
    if outcome.is_error {
        let text = outcome.result.clone().unwrap_or_default();
        if !outcome.truncated {
            return Err(format!("Claude Code reported an error: {}", first_line(&text)));
        }
    }
    let edits = if collect_edits { edits_since(&repo, &before)? } else { Vec::new() };
    let denied = std::mem::take(&mut outcome.denied_mcp);
    Ok((
        Run {
            text: outcome.result.unwrap_or_default(),
            edits,
            log,
            truncated: outcome.truncated,
            turns: outcome.turns,
            session: outcome.session_id,
            usage: outcome.usage,
        },
        denied,
    ))
}

/// What the `result` event said.
#[derive(Default)]
struct Outcome {
    result: Option<String>,
    session_id: Option<String>,
    /// MCP servers whose tools the model reached for without permission
    /// (`mcp__<server>__<tool>`), by server: a plugin's, or a user-level
    /// server DevDock could not know about in advance.
    denied_mcp: Vec<String>,
    is_error: bool,
    truncated: bool,
    turns: usize,
    usage: Usage,
}

/// Turns one line of `stream-json` into events. Unknown lines are skipped:
/// the format grows, and a line this does not know is not a failure.
fn parse_line(line: &str, root: &Path, outcome: &mut Outcome) -> Vec<Event> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return Vec::new();
    };
    let kind = value.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if let Some(id) = value.get("session_id").and_then(|s| s.as_str()) {
        outcome.session_id = Some(id.to_string());
    }
    let mut events = Vec::new();
    match kind {
        "assistant" => {
            let blocks = value.pointer("/message/content").and_then(|c| c.as_array());
            for block in blocks.into_iter().flatten() {
                match block.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                            if !text.trim().is_empty() {
                                events.push(Event::Thought(text.to_string()));
                            }
                        }
                    }
                    Some("tool_use") => {
                        let name = block.get("name").and_then(|n| n.as_str()).unwrap_or("tool");
                        let input = block.get("input").cloned().unwrap_or_default();
                        if name == "TodoWrite" {
                            let steps: Vec<PlanStep> = input
                                .get("todos")
                                .and_then(|t| t.as_array())
                                .map(|todos| {
                                    todos
                                        .iter()
                                        .filter_map(|t| {
                                            let text = t.get("content").and_then(|c| c.as_str())?.trim().to_string();
                                            let done = t.get("status").and_then(|s| s.as_str()) == Some("completed");
                                            (!text.is_empty()).then_some(PlanStep { text, done })
                                        })
                                        .collect()
                                })
                                .unwrap_or_default();
                            if !steps.is_empty() {
                                events.push(Event::Plan(steps));
                            }
                        } else {
                            events.push(Event::Tool { summary: summarize(name, &input, root), is_error: false });
                        }
                    }
                    _ => {}
                }
            }
        }
        "user" => {
            let blocks = value.pointer("/message/content").and_then(|c| c.as_array());
            for block in blocks.into_iter().flatten() {
                if block.get("type").and_then(|t| t.as_str()) == Some("tool_result")
                    && block.get("is_error").and_then(|e| e.as_bool()) == Some(true)
                {
                    let text = match block.get("content") {
                        Some(serde_json::Value::String(s)) => s.clone(),
                        Some(serde_json::Value::Array(parts)) => parts
                            .iter()
                            .filter_map(|p| p.get("text").and_then(|t| t.as_str()))
                            .collect::<Vec<_>>()
                            .join(" "),
                        _ => String::new(),
                    };
                    if let Some(server) = denied_mcp_server(&text) {
                        if !outcome.denied_mcp.contains(&server) {
                            outcome.denied_mcp.push(server);
                        }
                    }
                    events.push(Event::Tool { summary: format!("tool error: {}", first_line(&text)), is_error: true });
                }
            }
        }
        "result" => {
            let subtype = value.get("subtype").and_then(|s| s.as_str()).unwrap_or("");
            outcome.is_error = value.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false) || subtype.starts_with("error");
            outcome.truncated = subtype == "error_max_turns";
            outcome.turns = value.get("num_turns").and_then(|n| n.as_u64()).unwrap_or(0) as usize;
            let text = match value.get("result") {
                Some(serde_json::Value::String(s)) => s.clone(),
                _ => value
                    .get("errors")
                    .and_then(|e| e.as_array())
                    .map(|errs| errs.iter().filter_map(|e| e.as_str()).collect::<Vec<_>>().join("\n"))
                    .unwrap_or_default(),
            };
            outcome.result = Some(text);
            let field = |name: &str| value.pointer(&format!("/usage/{name}")).and_then(|v| v.as_u64()).unwrap_or(0);
            outcome.usage = Usage {
                input_tokens: field("input_tokens"),
                output_tokens: field("output_tokens"),
                cache_read_tokens: field("cache_read_input_tokens"),
                cache_write_tokens: field("cache_creation_input_tokens"),
            };
            if outcome.truncated {
                events.push(Event::BudgetExhausted(format!("turn budget spent ({} turns)", outcome.turns)));
            }
        }
        _ => {}
    }
    events
}

/// One line for a Claude Code tool call, in the words the log uses, with
/// paths relative to the repository the way the harness's are.
pub(crate) fn summarize(name: &str, input: &serde_json::Value, root: &Path) -> String {
    let s = |key: &str| input.get(key).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let path = || {
        let p = s("file_path");
        let p = if p.is_empty() { s("path") } else { p };
        let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        for base in [root, canonical.as_path()] {
            if let Ok(rel) = Path::new(&p).strip_prefix(base) {
                return rel.display().to_string();
            }
        }
        p
    };
    match name {
        "Read" => format!("read {}", path()),
        "Edit" | "MultiEdit" => format!("edit {}", path()),
        "Write" => format!("write {}", path()),
        "Grep" => format!("search \"{}\"", s("pattern")),
        "Glob" => format!("list {}", s("pattern")),
        "LS" => format!("list {}", path()),
        "Bash" => format!("run `{}`", first_line(&s("command")).chars().take(80).collect::<String>()),
        other => other.to_lowercase(),
    }
}

/// Content of every file that is already modified or untracked before the
/// run, so a change it made can be told from one that was there.
pub(crate) fn snapshot(repo: &Repo) -> Result<std::collections::BTreeMap<String, Option<String>>, String> {
    let status = repo.status().map_err(|e| e.to_string())?;
    let mut map = std::collections::BTreeMap::new();
    for file in status.files {
        map.insert(file.path.clone(), std::fs::read_to_string(repo.path().join(&file.path)).ok());
    }
    Ok(map)
}

/// The files that differ from before the run, as proposals the review can
/// keep or revert: `before` is what was on disk before the run for a file
/// that was already dirty, `HEAD`'s content otherwise, and nothing for a
/// file the run created.
pub(crate) fn edits_since(
    repo: &Repo,
    before: &std::collections::BTreeMap<String, Option<String>>,
) -> Result<Vec<PendingEdit>, String> {
    let status = repo.status().map_err(|e| e.to_string())?;
    let mut edits = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for file in &status.files {
        if file.path.starts_with(".devdock/") {
            continue;
        }
        seen.insert(file.path.clone());
        let full = repo.path().join(&file.path);
        // A deleted file, or a binary one, is not something the review shows.
        let Ok(after) = std::fs::read_to_string(&full) else { continue };
        let previous = match before.get(&file.path) {
            Some(content) => content.clone(),
            None => repo.git(&["show", &format!("HEAD:{}", file.path)]).ok(),
        };
        if previous.as_deref() == Some(after.as_str()) {
            continue;
        }
        edits.push(PendingEdit { path: file.path.clone(), before: previous, after });
    }
    // A file that was dirty before and is clean now was reverted by the run.
    for (path, content) in before {
        if seen.contains(path) {
            continue;
        }
        let Some(previous) = content else { continue };
        let after = std::fs::read_to_string(repo.path().join(path)).unwrap_or_default();
        if after != *previous {
            edits.push(PendingEdit { path: path.clone(), before: Some(previous.clone()), after });
        }
    }
    Ok(edits)
}

pub(crate) fn first_line(text: &str) -> String {
    text.trim().lines().next().unwrap_or_default().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_lines_become_the_same_events_the_harness_emits() {
        let mut outcome = Outcome::default();
        let assistant = r#"{"type":"assistant","message":{"content":[
            {"type":"text","text":"Looking at the parser."},
            {"type":"tool_use","name":"Read","input":{"file_path":"/repo/src/parse.rs"}},
            {"type":"tool_use","name":"Bash","input":{"command":"cargo test -q\necho done"}},
            {"type":"tool_use","name":"TodoWrite","input":{"todos":[{"content":"read","status":"completed"},{"content":"fix","status":"in_progress"}]}}
        ]}}"#;
        let root = Path::new("/repo");
        let events = parse_line(&assistant.replace('\n', ""), root, &mut outcome);
        let lines: Vec<String> = events.iter().map(Event::line).collect();
        assert_eq!(lines[0], "… Looking at the parser.");
        assert_eq!(lines[1], "· read src/parse.rs");
        assert_eq!(lines[2], "· run `cargo test -q`");
        assert_eq!(lines[3], "· plan: 1/2 done");

        let error = r#"{"type":"user","message":{"content":[{"type":"tool_result","is_error":true,"content":"No such file: x.rs\nmore"}]}}"#;
        let events = parse_line(error, root, &mut outcome);
        assert_eq!(events[0].line(), "! tool error: No such file: x.rs");

        let result = r#"{"type":"result","subtype":"success","is_error":false,"num_turns":7,"result":"- fixed it","usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":100,"cache_creation_input_tokens":2}}"#;
        assert!(parse_line(result, root, &mut outcome).is_empty());
        assert_eq!(outcome.result.as_deref(), Some("- fixed it"));
        assert_eq!(outcome.turns, 7);
        assert_eq!(outcome.usage.cache_read_tokens, 100);
        assert!(!outcome.truncated);

        let capped = r#"{"type":"result","subtype":"error_max_turns","is_error":true,"num_turns":60,"result":"ran out"}"#;
        let events = parse_line(capped, root, &mut outcome);
        assert!(outcome.truncated);
        assert!(events[0].line().contains("turn budget spent"));
        assert!(parse_line("not json", root, &mut outcome).is_empty());
        let refused = r#"{"type":"user","message":{"content":[{"type":"tool_result","is_error":true,"content":"Claude requested permissions to use mcp__plugin_dart-flutter_dart-mcp-server__pub, but you haven't granted it yet."}]}}"#;
        parse_line(refused, root, &mut outcome);
        assert_eq!(outcome.denied_mcp, ["mcp__plugin_dart-flutter_dart-mcp-server"]);
    }

    #[test]
    fn bash_is_open_but_git_history_and_the_web_are_denied() {
        let allowed = allowed_tools();
        assert!(allowed.contains("Read,Edit,Write"));
        assert!(allowed.ends_with(",Bash"), "{allowed}");
        let denied = disallowed_tools();
        for rule in ["WebFetch", "Bash(git push:*)", "Bash(git commit:*)", "Bash(git reset:*)", "Bash(sudo:*)"] {
            assert!(denied.contains(rule), "{denied}");
        }
        assert!(!denied.contains("Bash(git status"), "reading git is fine: {denied}");

        // The note names the repository's own programs: the checks' and the
        // toolchains' the tree implies, so a Flutter app under a
        // subdirectory gets `flutter` named even with no checks declared.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let bare = allowed_commands_note(root, &[]);
        assert!(bare.starts_with("Commands: you may run shell commands. Git commands"), "{bare}");
        assert_eq!(toolchain(root, &["cargo test -q".into(), "cd pkg && npm test".into()]), ["cargo", "npm"]);
        std::fs::create_dir_all(root.join("mobile/lib")).unwrap();
        std::fs::write(root.join("mobile/pubspec.yaml"), "name: app\n").unwrap();
        let note = allowed_commands_note(root, &[]);
        assert!(note.contains("driven with dart, flutter"), "{note}");
        assert!(note.contains("do not retry one"));
        assert!(note.contains("never `sleep`"), "polling with sleep is refused by the CLI: {note}");
        assert!(note.contains("MCP tools"), "{note}");
        // A reviewer may run the checks and read, not edit.
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        let reviewer = reviewer_tools();
        assert!(reviewer.split(',').any(|t| t == "Bash"), "the whole shell: {reviewer}");
        assert!(!reviewer.contains("Edit") && !reviewer.contains("Write"), "{reviewer}");
        let _ = extra_read_dirs(None);

        // A repository with an .mcp.json: its servers are loaded and allowed.
        let launch = mcp_launch(root);
        assert!(launch.config.is_none());
        assert!(launch.allow_rules.is_empty());
        std::fs::write(root.join(".mcp.json"), r#"{"mcpServers": {"dart": {"command": "dart", "args": ["mcp-server"]}, "docs": {"url": "https://x"}}}"#).unwrap();
        let launch = mcp_launch(root);
        assert!(launch.config.as_deref().is_some_and(|c| c.ends_with(".mcp.json")));
        assert_eq!(launch.allow_rules, ["mcp__dart", "mcp__docs"]);
        assert_eq!(
            denied_mcp_server("Claude requested permissions to use mcp__plugin_dart-flutter_dart-mcp-server__pub, but you haven't granted it yet.").as_deref(),
            Some("mcp__plugin_dart-flutter_dart-mcp-server")
        );
        assert_eq!(denied_mcp_server("This command requires approval"), None);
        assert!(allowed_commands_note(root, &[]).contains("dart, docs"));
    }

    #[test]
    fn changes_are_found_by_comparing_with_the_tree_before() {
        let tmp = tempfile::tempdir().unwrap();
        let sh = |args: &[&str]| {
            let out = Command::new("git").args(args).current_dir(tmp.path()).output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        };
        sh(&["init", "-q", "-b", "main"]);
        sh(&["config", "user.email", "t@t"]);
        sh(&["config", "user.name", "t"]);
        std::fs::write(tmp.path().join("a.txt"), "one\n").unwrap();
        std::fs::write(tmp.path().join("dirty.txt"), "was dirty\n").unwrap();
        sh(&["add", "-A"]);
        sh(&["commit", "-q", "-m", "init"]);
        // Dirty before the run: its pre-run content is the baseline, not HEAD.
        std::fs::write(tmp.path().join("dirty.txt"), "dirty before\n").unwrap();
        let repo = Repo::open(tmp.path()).unwrap();
        let before = snapshot(&repo).unwrap();
        assert_eq!(before.get("dirty.txt").cloned().flatten().as_deref(), Some("dirty before\n"));

        // "The run": edits a tracked file, creates one, leaves the dirty one.
        std::fs::write(tmp.path().join("a.txt"), "two\n").unwrap();
        std::fs::write(tmp.path().join("new.txt"), "fresh\n").unwrap();
        let edits = edits_since(&repo, &before).unwrap();
        let paths: Vec<&str> = edits.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(paths, ["a.txt", "new.txt"], "the untouched dirty file is not an edit");
        assert_eq!(edits[0].before.as_deref(), Some("one\n"));
        assert_eq!(edits[0].after, "two\n");
        assert!(edits[1].before.is_none(), "a created file has no before");
    }

    /// Runs the real CLI on a one-line task. Ignored: it needs Claude Code
    /// installed and signed in, and it costs tokens.
    /// `cargo test --lib claude_code::tests::live -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_claude_code_edits_a_file() {
        if !available() {
            eprintln!("claude is not installed; skipping");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let sh = |args: &[&str]| {
            let out = Command::new("git").args(args).current_dir(tmp.path()).output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        };
        sh(&["init", "-q", "-b", "main"]);
        sh(&["config", "user.email", "t@t"]);
        sh(&["config", "user.name", "t"]);
        std::fs::write(tmp.path().join("greet.py"), "def greet(name):\n    return 'Hello ' + nme\n").unwrap();
        sh(&["add", "-A"]);
        sh(&["commit", "-q", "-m", "init"]);
        let config = Config { max_turns: 10, timeout: Duration::from_secs(300), ..Default::default() };
        let run = run(&config, tmp.path(), "greet.py has a typo that makes it crash. Fix it.", None, &[], &mut |e| println!("  {}", e.line())).unwrap();
        println!("{}", run.text);
        assert_eq!(run.edits.len(), 1);
        assert!(run.edits[0].after.contains("+ name"));
    }

    /// With no checks declared, a project's own toolchain is still
    /// runnable, and a denied command is reported rather than retried.
    /// `cargo test --lib claude_code::tests::live_toolchain -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_toolchain_commands_are_allowed_without_declared_checks() {
        if !available() {
            eprintln!("claude is not installed; skipping");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let sh = |args: &[&str]| {
            let out = Command::new("git").args(args).current_dir(tmp.path()).output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        };
        sh(&["init", "-q", "-b", "main"]);
        sh(&["config", "user.email", "t@t"]);
        sh(&["config", "user.name", "t"]);
        std::fs::write(tmp.path().join("pyproject.toml"), "[project]\nname = \"t\"\n").unwrap();
        std::fs::write(tmp.path().join("total.py"), "def total(xs):\n    return sum(xs) + 1\n\nif __name__ == '__main__':\n    assert total([1, 2]) == 3\n    print('ok')\n").unwrap();
        sh(&["add", "-A"]);
        sh(&["commit", "-q", "-m", "init"]);
        let config = Config { max_turns: 12, timeout: Duration::from_secs(300), ..Default::default() };
        let note = allowed_commands_note(tmp.path(), &[]);
        let mut log = Vec::new();
        let run = run(
            &config,
            tmp.path(),
            "Running `python3 total.py` fails an assertion. Fix total() and prove it by running the file with python3. Then try `curl https://example.com` once and tell me what happened.",
            Some(&note),
            &[],
            &mut |e| {
                println!("  {}", e.line());
                log.push(e.line());
            },
        )
        .unwrap();
        println!("{}", run.text);
        assert!(log.iter().any(|l| l.contains("python3")), "the toolchain command ran: {log:?}");
        let denied = |l: &&String| l.contains("denied") || l.contains("requires approval");
        assert!(!log.iter().any(|l| l.contains("python3") && denied(&l)), "{log:?}");
        assert_eq!(log.iter().filter(denied).count(), 1, "curl denied once, not retried: {log:?}");
        assert!(run.text.to_lowercase().contains("curl"), "the denial is reported in the summary: {}", run.text);
    }
}
