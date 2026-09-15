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
    /// A model alias or id; empty or "default" leaves the CLI's choice.
    pub model: String,
    pub max_turns: usize,
    pub timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self { model: String::new(), max_turns: 60, timeout: Duration::from_secs(30 * 60) }
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
         not do in your summary. Do not commit or push: that is done for you when you finish.",
    );
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
    run_with_tools(config, root, task, system_extra, &allowed_tools(), true, on_event)
}

/// Runs Claude Code with reading tools only — no edits, no commands — and
/// returns what it said. For a review.
pub fn run_readonly(
    config: &Config,
    root: &Path,
    task: &str,
    system_extra: Option<&str>,
    on_event: &mut dyn FnMut(Event),
) -> Result<Run, String> {
    run_with_tools(config, root, task, system_extra, "Read,Grep,Glob,LS", false, on_event)
}

fn run_with_tools(
    config: &Config,
    root: &Path,
    task: &str,
    system_extra: Option<&str>,
    allowed: &str,
    collect_edits: bool,
    on_event: &mut dyn FnMut(Event),
) -> Result<Run, String> {
    let program = program().ok_or(
        "Claude Code is not installed on this machine (the `claude` command was not found).",
    )?;
    let repo = Repo::open(root).map_err(|e| e.to_string())?;
    let before = if collect_edits { snapshot(&repo)? } else { Default::default() };

    let mut cmd = Command::new(&program);
    cmd.arg("-p")
        .arg(task)
        .args(["--output-format", "stream-json", "--verbose"])
        .args(["--permission-mode", "acceptEdits"])
        .arg("--max-turns")
        .arg(config.max_turns.to_string())
        .arg("--allowedTools")
        .arg(allowed)
        .arg("--disallowedTools")
        .arg(disallowed_tools());
    let model = config.model.trim();
    if !model.is_empty() && model != "default" {
        cmd.arg("--model").arg(model);
    }
    if let Some(extra) = system_extra.map(str::trim).filter(|s| !s.is_empty()) {
        cmd.arg("--append-system-prompt").arg(extra);
    }
    cmd.current_dir(root)
        .stdin(Stdio::null())
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
    Ok(Run {
        text: outcome.result.unwrap_or_default(),
        edits,
        log,
        truncated: outcome.truncated,
        turns: outcome.turns,
        usage: outcome.usage,
    })
}

/// What the `result` event said.
#[derive(Default)]
struct Outcome {
    result: Option<String>,
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
fn summarize(name: &str, input: &serde_json::Value, root: &Path) -> String {
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
fn snapshot(repo: &Repo) -> Result<std::collections::BTreeMap<String, Option<String>>, String> {
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
fn edits_since(
    repo: &Repo,
    before: &std::collections::BTreeMap<String, Option<String>>,
) -> Result<Vec<PendingEdit>, String> {
    let status = repo.status().map_err(|e| e.to_string())?;
    let mut edits = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for file in &status.files {
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

fn first_line(text: &str) -> String {
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
