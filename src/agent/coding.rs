//! The coding agent: prompts and entry point.
//!
//! Where [`super::conflict`] resolves a merge and [`crate::review`] judges a
//! diff, this one takes an open-ended instruction — "add a flag for X", "why
//! is Y slow", "extract this into a module" — and works on the repository
//! until it can report back.
//!
//! It gets the full toolset the workspace can offer: reading and searching,
//! editing, whatever the language server knows ([`Workspace::with_language_support`]),
//! and the project's own checks ([`Workspace::with_checks`]). Which of those
//! actually appear depends on how the caller built the workspace, and the
//! prompt is assembled to match — telling a model about tools it does not
//! have produces confident nonsense about work it never did.
//!
//! Every file it changes is confirmed by the user. In
//! [`super::WriteMode::Overlay`] that means nothing reaches disk until the
//! user accepts it; in [`super::WriteMode::Live`] the changes are written as
//! it works, so it can compile and test them, and the confirmation at the
//! end is keep-or-revert.

use super::{claude_code, Event, Limits, Provider, Run, Workspace, WriteMode};

/// What does the work: this crate's tool-use loop over a model, or Claude
/// Code run headless in the tree. Same task, same events, same review.
pub enum Engine {
    Harness(Box<dyn Provider>),
    ClaudeCode(claude_code::Config),
}

impl Engine {
    /// Says which harness, then which model: "DevDock harness · Claude
    /// (claude-sonnet-5)" or "Claude Code agent (sonnet)". A label that only
    /// named the model left the question it exists to answer open.
    pub fn label(&self) -> String {
        match self {
            Self::Harness(p) => format!("DevDock harness · {}", p.label()),
            Self::ClaudeCode(c) => {
                if c.model.is_empty() || c.model == "default" {
                    "Claude Code agent".into()
                } else {
                    format!("Claude Code agent ({})", c.model)
                }
            }
        }
    }

    /// Claude Code writes to disk as it goes; it cannot propose.
    pub fn needs_live_tree(&self) -> bool {
        matches!(self, Self::ClaudeCode(_))
    }
}

/// One earlier exchange in the same session.
#[derive(Debug, Clone)]
pub struct Turn {
    pub task: String,
    pub summary: String,
}

const BASE_PROMPT: &str = r#"You are a coding agent working in a real repository that a developer is watching.

How to work:
- Start by calling update_plan with the steps you intend to take, then tick each one off as you finish it. The developer watches that list while you work; it is the only thing telling them what you are doing. Keep it short — the steps of the job, not every tool call.
- Orient before you act. You are given an overview of the repository; use search and list_files to find the code the task is about rather than guessing paths or names. Then read the whole function or block you are going to change, and the code around it — the callers, the tests, the type it returns. A change that looks right in isolation and breaks its caller is worse than no change.
- Make the smallest change that does the job, with edit_file and a snippet copied exactly from what you read, or replace_lines with the line numbers read_file showed when the text is awkward to reproduce (escapes, tabs, long lines). If an edit fails to match, do not keep retrying variants of the snippet: switch to replace_lines. write_file is for new files or a wholesale rewrite. Match the surrounding code's style, naming, and structure — someone reviewing the diff should not be able to tell which lines you wrote.
- Do not rewrite, reformat, or "clean up" code the task did not ask about. An unrequested refactor buried in a real change is how a review gets abandoned. Leave no TODOs, commented-out code, or debugging output behind.
- An edit's result already tells you what the file now says; do not re-read a file just to confirm an edit applied, and do not repeat an edit that reported success.
- Verify before you trust. After editing, use whatever can see your work: diagnostics on every file you changed, the project's checks when there are any. Read a failure properly and fix its cause; never weaken a test or a check to make it pass. Rerun until it is clean. Before you report, show_changes gives you the complete diff to read the way the developer will.
- When the task is a question rather than a change, answer it, with file:line references. Do not edit files to prove a point.
- If the task is ambiguous in a way that changes what you would write, say so in your summary and implement the reading you think is right, rather than guessing silently or refusing.
- If you cannot do something, say which part and why. A partial change plus an honest account of what is missing is useful; a confident summary of work you did not do is not.

Finish with a short Markdown summary: what you changed and why, one bullet per file; then a line starting "Verified:" naming exactly which checks and diagnostics you ran and what they said — or "Verified: nothing" and why; then anything you deliberately left alone."#;

/// The standard every change is held to, whatever the language: the fixer
/// is told to write to it, and the reviewer to send back what does not.
pub const CODE_STANDARD: &str = r#"The code you write, whatever the language or the task:
- Reusable over ad hoc: when the same logic is needed twice, it lives once — a function, a method, a type — and is called from both places. Never paste a block with small variations. When the repository already has a helper, a base type, or a pattern for what you are doing, use it rather than writing another.
- One responsibility per unit: a function does one thing and its name says which; a type owns one concept and its data stays behind its methods. Where the language has classes or traits, put behaviour with the data it belongs to rather than in free-floating procedures over bare fields. Prefer composition; keep interfaces small.
- Clean: names that say what a thing is or does, no abbreviations a stranger would have to decode; no magic numbers — a named constant; no deep nesting — return early; no dead code, commented-out code, TODOs, or debugging output; errors handled where they occur, not swallowed.
- Consistent with the code around it: match the repository's conventions, module layout, error handling, and test style, so the diff reads as if the maintainer wrote it.
- Tested: behaviour you add or fix gets a test in the repository's own style, and a bug fix gets the test that would have caught it."#;

const READ_TOOLS: &str = r#"
Reading the repository: list_files, read_file, and search (literal or regex, optionally with context lines) see every file git tracks."#;

const EDIT_TOOLS_OVERLAY: &str = r#"
Changing it: write_file, edit_file, replace_lines, and show_changes to review your diff. Your changes are held for the developer to review as diffs — nothing reaches disk until they accept it, file by file. That also means you cannot compile or test what you write in this run, so be correspondingly careful and say plainly in your summary what you could not verify."#;

const EDIT_TOOLS_LIVE: &str = r#"
Changing it: write_file, edit_file, and replace_lines write to the developer's working tree immediately, so the language server and the project's checks see your work; show_changes reviews your diff. They review every change at the end and can revert any of it, so leave the tree in a state you would be willing to show: no debugging leftovers, no half-finished edit you meant to come back to."#;

const LANGUAGE_TOOLS: &str = r#"
The language server: diagnostics, definition, references, find_symbol. Use it rather than guessing — references finds every caller a signature change breaks, which a text search cannot do reliably, and diagnostics is how you check your own work. After changing a file, ask for its diagnostics before moving on; the language server sees your edits even before they are accepted."#;

const CHECK_TOOLS: &str = r#"
The project's checks: run_check runs one of the commands this repository already declares (build, tests, lint). Run the relevant one before you report a change as done, and if it fails, fix what you broke rather than reporting it as finished. You will be sent back if you try to finish an edit without running one."#;

const COMMAND_TOOLS: &str = r#"
Commands: run_command runs a shell command in the repository root — the toolchain (build, a single test, a formatter, a linter), installing dependencies, a script — anything the named checks do not cover. Prefer it over guessing whether something compiles. It cannot commit, push, or rewrite git history; that is done for you. Stay inside the repository."#;

const MCP_TOOLS: &str = r#"
MCP tools: tools named mcp__<server>__<tool> come from the servers this repository declares in .mcp.json, running for this run. They know this project — a package manager, an analyzer, a database — so prefer one over reconstructing the same thing in the shell. Their output comes back as text."#;

const ASK_TOOLS: &str = r#"
Asking: ask_developer puts one question to the developer and waits for the answer. Use it only when something genuinely uncertain would change what you build — a product choice, two reasonable readings of the task, a value nobody wrote down — and ask it specifically, with the options you see. Everything else you decide and state in your summary. If no answer comes you are told; then proceed on your best assumption."#;

/// What Claude Code is told about asking: it has no tool for it in an
/// unattended run, so a question is a line at the end of its reply, and
/// the run is resumed with the answer.
pub const CLAUDE_CODE_ASK: &str = "If you need the developer to decide something before you can proceed — a \
    product choice, two reasonable readings of the task — do not guess: end your reply with a \
    single line `QUESTION: <one specific question, with the options you see>` and stop without \
    changing anything more. You will be resumed with the answer. Ask only when it truly changes \
    what you would build.";

/// Where a run's attached images are put for Claude Code to read.
pub const IMAGE_DIR: &str = ".devdock/prompt-images";

/// The paragraph that points Claude Code at the attached images.
pub fn image_note(paths: &[String]) -> String {
    format!(
        "\n\nThe developer attached {} image(s) to this task — a screenshot, a mockup, what \
         words describe badly. Look at each with the Read tool before you start, and treat \
         what they show as part of the task:\n{}\n",
        paths.len(),
        paths.iter().map(|p| format!("- {p}")).collect::<Vec<_>>().join("\n")
    )
}

fn sanitize_name(name: &str) -> String {
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    let cleaned: String = stem.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect();
    let cleaned = cleaned.trim_matches('-').chars().take(40).collect::<String>();
    if cleaned.is_empty() { "image".into() } else { cleaned }
}

/// The question a Claude Code reply ends with, if it ends with one.
pub fn question_in(reply: &str) -> Option<String> {
    reply
        .trim()
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .and_then(|last| last.trim().strip_prefix("QUESTION:"))
        .map(|q| q.trim().to_string())
        .filter(|q| !q.is_empty())
}

/// The system prompt for a run, describing exactly the tools it has.
pub fn system_prompt(
    write_mode: WriteMode,
    has_language_tools: bool,
    has_checks: bool,
    has_commands: bool,
    has_ask: bool,
    has_mcp: bool,
    extra_instructions: Option<&str>,
) -> String {
    let mut prompt = String::from(BASE_PROMPT);
    prompt.push_str("\n\n");
    prompt.push_str(CODE_STANDARD);
    prompt.push_str("\n\nYour tools:\n");
    prompt.push_str(READ_TOOLS);
    prompt.push_str(match write_mode {
        WriteMode::Overlay => EDIT_TOOLS_OVERLAY,
        WriteMode::Live => EDIT_TOOLS_LIVE,
    });
    if has_language_tools {
        prompt.push_str(LANGUAGE_TOOLS);
    }
    if has_checks {
        prompt.push_str(CHECK_TOOLS);
    }
    if has_commands {
        prompt.push_str(COMMAND_TOOLS);
    }
    if has_mcp {
        prompt.push_str(MCP_TOOLS);
    }
    if has_ask {
        prompt.push_str(ASK_TOOLS);
    }
    if let Some(extra) = extra_instructions.map(str::trim).filter(|s| !s.is_empty()) {
        prompt.push_str("\n\nProject-specific instructions:\n");
        prompt.push_str(extra);
    }
    prompt
}

/// The opening turn: the task, plus what happened earlier in this session.
///
/// Earlier turns are summarized rather than replayed in full. The tool
/// transcript of a previous task is mostly file contents that have since
/// changed, and feeding it back invites the model to act on stale reads.
pub fn task_prompt(
    task: &str,
    history: &[Turn],
    branch: Option<&str>,
    overview: Option<&str>,
    context: Option<&str>,
) -> String {
    let mut prompt = String::new();
    if let Some(overview) = overview.map(str::trim).filter(|o| !o.is_empty()) {
        prompt.push_str(&format!("Repository: {overview}\n\n"));
    }
    if let Some(branch) = branch {
        prompt.push_str(&format!("You are on branch `{branch}`.\n\n"));
    }
    if let Some(context) = context.map(str::trim).filter(|c| !c.is_empty()) {
        prompt.push_str(context);
        prompt.push_str("\n\n");
    }
    if !history.is_empty() {
        prompt.push_str("Earlier in this session:\n\n");
        for turn in history {
            prompt.push_str(&format!("- Asked: {}\n  You reported: {}\n", turn.task, turn.summary));
        }
        prompt.push_str(
            "\nThose changes may still be under review, so the files may or may not \
             have them. Read before assuming.\n\n",
        );
    }
    prompt.push_str("Task:\n");
    prompt.push_str(task.trim());
    prompt
}

/// Budgets for a coding run: longer than a review or a merge, because the
/// work is open-ended and the verify loop costs turns.
pub fn limits() -> Limits {
    Limits {
        max_turns: 60,
        max_tool_calls: 200,
        max_read_bytes: 1_500_000,
        max_tokens: 8192,
        max_transcript_bytes: 800_000,
    }
}

/// One request to the coding agent: what to do, and what it should know.
pub struct Request<'a> {
    pub task: &'a str,
    /// Earlier exchanges in this session.
    pub history: &'a [Turn],
    /// The branch being worked on, for context.
    pub branch: Option<&'a str>,
    /// Project guidance, appended to the system prompt.
    pub instructions: Option<&'a str>,
    /// What is going on in the repository right now — recent commits,
    /// uncommitted files — for the opening turn.
    pub context: Option<&'a str>,
    /// Images the developer attached to the task.
    pub images: &'a [super::Attachment],
    pub limits: Limits,
}

impl<'a> Request<'a> {
    /// A request with default budgets and no context.
    pub fn new(task: &'a str) -> Self {
        Self {
            task,
            images: &[],
            history: &[],
            branch: None,
            instructions: None,
            context: None,
            limits: limits(),
        }
    }
}

/// Runs the coding agent on whichever engine was chosen.
pub fn run_with(
    engine: &Engine,
    workspace: &mut Workspace,
    request: Request<'_>,
    on_event: &mut dyn FnMut(Event),
) -> Result<Run, String> {
    on_event(Event::Engine(engine.label()));
    match engine {
        Engine::Harness(provider) => run(provider.as_ref(), workspace, request, on_event),
        Engine::ClaudeCode(config) => run_claude_code(config, workspace, request, on_event),
    }
}

/// The same request, handed to Claude Code. The tree has to be live —
/// Claude Code edits files, it does not propose — and its commands are
/// limited to the repository's checks, its toolchains, and reading.
fn run_claude_code(
    config: &claude_code::Config,
    workspace: &Workspace,
    request: Request<'_>,
    on_event: &mut dyn FnMut(Event),
) -> Result<Run, String> {
    if request.task.trim().is_empty() {
        return Err("Describe what you want done first.".into());
    }
    if workspace.write_mode() != WriteMode::Live {
        return Err(
            "Claude Code writes to the working tree as it works, so it needs \"Let it \
             iterate\" on. Turn it on, or pick a Claude or Ollama model for the built-in \
             agent."
                .into(),
        );
    }
    let mut extra = String::from(
        "You are working unattended for a developer who reviews every change afterwards. \
         Keep changes to what the task asks; no unrelated refactors, no leftover debugging \
         output. Run the repository's checks before you finish, and do not weaken a test \
         to make it pass. If the task cannot be done without a decision from a person, say \
         so and change nothing. Finish with a short summary: what you changed and why, one \
         bullet per file, then a line starting \"Verified:\" naming what you ran.",
    );
    extra.push_str("\n\n");
    extra.push_str(CODE_STANDARD);
    let checks = workspace.check_commands();
    extra.push_str("\n\n");
    extra.push_str(&claude_code::allowed_commands_note(workspace.root(), &checks));
    let asker = workspace.asker();
    if asker.is_some() {
        extra.push_str("\n\n");
        extra.push_str(CLAUDE_CODE_ASK);
    }
    if let Some(instructions) = request.instructions.map(str::trim).filter(|s| !s.is_empty()) {
        extra.push_str("\n\nProject-specific instructions:\n");
        extra.push_str(instructions);
    }
    let overview = workspace.overview();
    let mut prompt = task_prompt(request.task, request.history, request.branch, Some(&overview), request.context);
    // Claude Code takes no image in its prompt, but reads image files:
    // the attachments go into the worktree for the run and are removed
    // after — never part of the change.
    let image_dir = workspace.root().join(IMAGE_DIR);
    if !request.images.is_empty() {
        std::fs::create_dir_all(&image_dir).map_err(|e| e.to_string())?;
        let mut paths = Vec::new();
        for (i, image) in request.images.iter().enumerate() {
            let path = image_dir.join(format!("{}-{}.{}", i + 1, sanitize_name(&image.name), image.extension()));
            std::fs::write(&path, image.bytes()).map_err(|e| e.to_string())?;
            paths.push(format!("{IMAGE_DIR}/{}", path.file_name().unwrap().to_string_lossy()));
        }
        prompt.push_str(&image_note(&paths));
    }
    let mut config = claude_code::Config { max_turns: request.limits.max_turns, ..config.clone() };
    // With a sandbox, Claude Code itself runs inside it: installed there
    // the first time, signed in with this machine's sign-in, kept after.
    if let Some(sandbox) = workspace.sandbox() {
        let mut log = |line: String| on_event(Event::Tool { summary: line, is_error: false });
        sandbox.provision(&["claude"], &mut log)?;
        sandbox.seed_claude_credentials(&mut log)?;
        log("Claude Code runs inside the sandbox".into());
        config.sandbox = Some(sandbox);
    }
    let outcome = claude_code::run(&config, workspace.root(), &prompt, Some(&extra), &checks, on_event);
    if !request.images.is_empty() {
        let _ = std::fs::remove_dir_all(&image_dir);
        // And the parent, when nothing else of DevDock's is in it.
        if let Some(parent) = image_dir.parent() {
            let _ = std::fs::remove_dir(parent);
        }
    }
    let mut run = outcome?;
    // A question at the end of the reply: put it to the developer, resume
    // the same session with the answer, and let it finish. A few times at
    // most; after that it decides.
    if let Some(asker) = asker {
        for _ in 0..5 {
            let Some(question) = question_in(&run.text) else { break };
            let Some(session) = run.session.clone() else { break };
            on_event(Event::Tool { summary: format!("asked you: {question}"), is_error: false });
            let answer = match asker(&question) {
                Ok(a) if !a.trim().is_empty() => format!("The developer answered: {}\n\nContinue and finish the task.", a.trim()),
                Ok(_) => "The developer sent no answer. Decide yourself, state the assumption in your summary, and finish the task.".to_string(),
                Err(why) => format!("No answer came ({why}). Decide yourself, state the assumption in your summary, and finish the task."),
            };
            let more = claude_code::resume(&config, workspace.root(), &session, &answer, Some(&extra), on_event)?;
            run.text = more.text;
            run.session = more.session;
            run.turns += more.turns;
            run.truncated = more.truncated;
            run.usage = run.usage.plus(more.usage);
            run.log.extend(more.log);
            run.edits = more.edits;
        }
    }
    Ok(run)
}

/// Runs the coding agent on the built-in harness.
pub fn run(
    provider: &dyn Provider,
    workspace: &mut Workspace,
    request: Request<'_>,
    on_event: &mut dyn FnMut(Event),
) -> Result<Run, String> {
    if request.task.trim().is_empty() {
        return Err("Describe what you want done first.".into());
    }
    let tools: Vec<&str> = workspace.tools().iter().map(|t| t.name).collect();
    // The repository's own instructions for agents come first: a file
    // named AGENTS.md is the project speaking, and the app's review
    // guidance is appended after it.
    let mut instructions = String::new();
    if let Some((name, text)) = workspace.project_instructions() {
        instructions.push_str(&format!("From {name} in this repository:\n{}", text.trim()));
    }
    if let Some(extra) = request.instructions.map(str::trim).filter(|s| !s.is_empty()) {
        if !instructions.is_empty() {
            instructions.push_str("\n\n");
        }
        instructions.push_str(extra);
    }
    let system = system_prompt(
        workspace.write_mode(),
        tools.contains(&"diagnostics"),
        tools.contains(&"run_check"),
        tools.contains(&"run_command"),
        tools.contains(&"ask_developer"),
        tools.iter().any(|t| t.starts_with("mcp__")),
        (!instructions.is_empty()).then_some(instructions.as_str()),
    );
    let overview = workspace.overview();
    let prompt = task_prompt(
        request.task,
        request.history,
        request.branch,
        Some(&overview),
        request.context,
    );
    super::run_with_images(provider, workspace, &system, &prompt, request.images, request.limits, on_event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Access;

    fn workspace(tmp: &tempfile::TempDir) -> Workspace {
        std::fs::write(tmp.path().join("a.rs"), "fn a() {}\n").unwrap();
        Workspace::new(tmp.path(), vec!["a.rs".into()], Access::ReadWrite).unwrap()
    }

    #[test]
    fn the_prompt_describes_only_the_tools_that_exist() {
        let bare = system_prompt(WriteMode::Overlay, false, false, false, false, false, None);
        assert!(bare.contains("list_files"));
        assert!(bare.contains("show_changes"));
        assert!(!bare.contains("The language server:"), "no language server was offered");
        assert!(!bare.contains("run_check"), "no checks were offered");
        assert!(!bare.contains("run_command"), "no commands were offered");
        assert!(bare.contains("nothing reaches disk"));

        let full = system_prompt(WriteMode::Live, true, true, true, true, true, Some("Never touch vendor/."));
        assert!(!bare.contains("MCP tools:") && full.contains("mcp__<server>__<tool>"));
        assert!(!bare.contains("ask_developer") && full.contains("ask_developer"));
        assert_eq!(question_in("I did x.\n\nQUESTION: Red or blue?").as_deref(), Some("Red or blue?"));
        assert_eq!(question_in("QUESTION: only this"), Some("only this".into()));
        assert_eq!(question_in("Done. Verified: tests"), None);
        let note = image_note(&[".devdock/prompt-images/1-mockup.png".into()]);
        assert!(note.contains("1 image(s)") && note.contains("- .devdock/prompt-images/1-mockup.png"));
        assert_eq!(sanitize_name("Screen Shot 2026 (1).png"), "Screen-Shot-2026--1");
        assert_eq!(sanitize_name("...png"), "image");
        assert!(full.contains("The language server:") && full.contains("run_check"));
        assert!(full.contains("run_command") && full.contains("cannot commit, push"));
        assert!(bare.contains("Reusable over ad hoc") && full.contains("One responsibility per unit"), "the standard is in every prompt");
        assert!(full.contains("write to the developer's working tree immediately"));
        assert!(full.contains("Never touch vendor/."));
    }

    #[test]
    fn the_task_prompt_carries_the_session_so_far() {
        let history = vec![Turn {
            task: "add a flag".into(),
            summary: "added --verbose to cli.rs".into(),
        }];
        let prompt = task_prompt(
            "now document it",
            &history,
            Some("feat/flags"),
            Some("3 tracked file(s)."),
            Some("Recent commits:\n- abc feat: flag"),
        );
        assert!(prompt.starts_with("Repository: 3 tracked file(s)."), "{prompt}");
        assert!(prompt.contains("feat/flags"));
        assert!(prompt.contains("Recent commits"));
        assert!(prompt.contains("add a flag"));
        assert!(prompt.contains("added --verbose"));
        assert!(prompt.ends_with("now document it"));
    }

    #[test]
    fn claude_code_needs_a_live_tree() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ws = workspace(&tmp);
        let engine = Engine::ClaudeCode(claude_code::Config::default());
        assert!(engine.needs_live_tree());
        assert_eq!(engine.label(), "Claude Code agent");
        let mut events = Vec::new();
        let err = run_with(&engine, &mut ws, Request::new("do it"), &mut |e| events.push(e.line())).unwrap_err();
        assert!(err.contains("Let it iterate"), "{err}");
        assert_eq!(events.first().map(String::as_str), Some("engine: Claude Code agent"), "announced first");
    }

    #[test]
    fn an_empty_task_is_refused_before_a_model_is_called() {
        struct Never;
        impl Provider for Never {
            fn label(&self) -> String {
                "never".into()
            }
            fn turn(
                &self,
                _: &str,
                _: &[crate::agent::Message],
                _: &[crate::agent::ToolSpec],
                _: u32,
            ) -> Result<crate::agent::Reply, String> {
                panic!("must not be called")
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let mut ws = workspace(&tmp);
        let err = run(&Never, &mut ws, Request::new("   "), &mut |_| {}).unwrap_err();
        assert!(err.contains("Describe what you want"));
    }

    #[test]
    fn the_run_reports_what_it_changed() {
        use crate::agent::{Message, Reply, ToolCall, ToolSpec};
        use std::cell::RefCell;

        struct Scripted(RefCell<Vec<Reply>>);
        impl Provider for Scripted {
            fn label(&self) -> String {
                "scripted".into()
            }
            fn turn(
                &self,
                system: &str,
                _: &[Message],
                _: &[ToolSpec],
                _: u32,
            ) -> Result<Reply, String> {
                assert!(system.contains("coding agent"));
                Ok(self.0.borrow_mut().remove(0))
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let mut ws = workspace(&tmp);
        let provider = Scripted(RefCell::new(vec![
            Reply {
                text: String::new(),
                calls: vec![ToolCall {
                    id: "1".into(),
                    name: "edit_file".into(),
                    input: serde_json::json!({
                        "path": "a.rs", "old_text": "fn a() {}", "new_text": "fn a() -> u32 { 1 }"
                    }),
                }], ..Default::default()
            },
            Reply { text: "- a.rs: returns a value now".into(), calls: vec![], ..Default::default() },
        ]));
        let run = run(
            &provider,
            &mut ws,
            Request { branch: Some("main"), ..Request::new("make a() return something") },
            &mut |_| {},
        )
        .unwrap();
        assert!(run.text.contains("returns a value"));
        assert_eq!(run.edits.len(), 1);
        assert_eq!(run.edits[0].path, "a.rs");
        // Overlay by default: the file on disk is untouched.
        assert_eq!(std::fs::read_to_string(tmp.path().join("a.rs")).unwrap(), "fn a() {}\n");
    }

    /// The built-in harness with a real model, on a tree with no checks
    /// declared: the only way to verify is run_command, and the model uses
    /// it rather than guessing. Stored Claude credentials, or
    /// `LIVE_OLLAMA_MODEL=qwen2.5-coder:7b` for a local model.
    /// `cargo test --lib coding::tests::live_harness -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_harness_runs_the_toolchain_through_run_command() {
        let provider: Box<dyn Provider> = match std::env::var("LIVE_OLLAMA_MODEL") {
            Ok(model) => Box::new(crate::ollama::Client::new("http://localhost:11434").agent(model)),
            Err(_) => match crate::claude::Client::from_store("claude-haiku-4-5-20251001") {
                Some(client) => Box::new(client),
                None => {
                    eprintln!("no Claude credentials; skipping");
                    return;
                }
            },
        };
        let client = provider.as_ref();
        let tmp = tempfile::tempdir().unwrap();
        let sh = |args: &[&str]| {
            let out = std::process::Command::new("git").args(args).current_dir(tmp.path()).output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        };
        sh(&["init", "-q", "-b", "main"]);
        sh(&["config", "user.email", "t@t"]);
        sh(&["config", "user.name", "t"]);
        std::fs::write(tmp.path().join("pyproject.toml"), "[project]\nname = \"t\"\n").unwrap();
        std::fs::write(tmp.path().join("total.py"), "def total(xs):\n    return sum(xs) + 1\n\nif __name__ == '__main__':\n    assert total([1, 2]) == 3\n    print('ok')\n").unwrap();
        sh(&["add", "-A"]);
        sh(&["commit", "-q", "-m", "init"]);
        let repo = crate::git::Repo::open(tmp.path()).unwrap();
        let mut ws = Workspace::new(repo.path(), repo.tracked_files().unwrap(), Access::ReadWrite)
            .unwrap()
            .with_write_mode(WriteMode::Live)
            .with_commands(true);
        let mut log = Vec::new();
        let run = run(
            client,
            &mut ws,
            Request::new("Running `python3 total.py` fails an assertion. Fix total() and prove it by running the file. Then run `git commit -am fix` and tell me what happened."),
            &mut |e| {
                println!("  {}", e.line());
                log.push(e.line());
            },
        )
        .unwrap();
        println!("{}", run.text);
        assert!(log.iter().any(|l| l.contains("run `python3")), "ran the file: {log:?}");
        assert!(log.iter().any(|l| l.contains("git commit") && l.starts_with('!')), "the commit was refused: {log:?}");
        assert_eq!(std::fs::read_to_string(tmp.path().join("total.py")).unwrap().matches("+ 1").count(), 0);
        let head = repo.log(1, None).unwrap()[0].subject.clone();
        assert_eq!(head, "init", "nothing was committed");
    }

    /// Both engines ask when told to, wait for the answer, and use it.
    /// `LIVE_ENGINE=claude-code` runs Claude Code, else the harness with Claude.
    /// `cargo test --lib coding::tests::live_asks -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_asks_the_developer_and_uses_the_answer() {
        let tmp = tempfile::tempdir().unwrap();
        let sh = |args: &[&str]| {
            let out = std::process::Command::new("git").args(args).current_dir(tmp.path()).output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        };
        sh(&["init", "-q", "-b", "main"]);
        sh(&["config", "user.email", "t@t"]);
        sh(&["config", "user.name", "t"]);
        std::fs::write(tmp.path().join("greeting.txt"), "Hello\n").unwrap();
        sh(&["add", "-A"]);
        sh(&["commit", "-q", "-m", "init"]);
        let repo = crate::git::Repo::open(tmp.path()).unwrap();
        let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let record = asked.clone();
        let asker: crate::agent::Asker = std::sync::Arc::new(move |q: &str| {
            println!("  [question] {q}");
            record.lock().unwrap().push(q.to_string());
            Ok("Use Portuguese: the word is Olá.".into())
        });
        let mut ws = Workspace::new(repo.path(), repo.tracked_files().unwrap(), Access::ReadWrite)
            .unwrap()
            .with_write_mode(WriteMode::Live)
            .with_commands(true)
            .with_asker(asker);
        // `LIVE_SANDBOX=1`: the whole run — Claude Code included — inside.
        if std::env::var("LIVE_SANDBOX").is_ok() {
            let sandbox = std::sync::Arc::new(crate::sandbox::Sandbox::start(&crate::sandbox::Spec::default(), repo.path(), &mut |l| println!("  {l}")).unwrap());
            let mut runners = crate::local_ci::runner::RunnerRegistry::with_builtins();
            runners.register(Box::new(crate::sandbox::SandboxRunner(sandbox.clone())));
            ws = ws.with_sandbox(sandbox, std::sync::Arc::new(runners));
        }
        let engine = match std::env::var("LIVE_ENGINE").as_deref() {
            Ok("claude-code") => Engine::ClaudeCode(claude_code::Config::default()),
            _ => match crate::claude::Client::from_store("claude-haiku-4-5-20251001") {
                Some(c) => Engine::Harness(Box::new(c)),
                None => {
                    eprintln!("no Claude credentials; skipping");
                    return;
                }
            },
        };
        let mut log = Vec::new();
        let run = run_with(
            &engine,
            &mut ws,
            Request::new("Translate the greeting in greeting.txt into another language. I have not said which language: you must ask me which one before changing anything, then write that translation into greeting.txt."),
            &mut |e| {
                println!("  {}", e.line());
                log.push(e.line());
            },
        )
        .unwrap();
        println!("{}", run.text);
        assert_eq!(asked.lock().unwrap().len(), 1, "asked exactly once: {:?}", asked.lock().unwrap());
        let text = std::fs::read_to_string(tmp.path().join("greeting.txt")).unwrap();
        assert!(text.contains("Olá"), "the answer was used: {text}");
        assert!(log.iter().any(|l| l.contains("asked you:")), "{log:?}");
    }

    /// An attached image reaches the model: a picture with a word in it,
    /// and the task to write that word down. `LIVE_IMAGE` is the PNG;
    /// `LIVE_ENGINE=claude-code` reads it through Claude Code's Read tool.
    /// `cargo test --lib coding::tests::live_sees -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_sees_an_attached_image() {
        let Ok(image_path) = std::env::var("LIVE_IMAGE") else {
            eprintln!("set LIVE_IMAGE to a png with a word in it; skipping");
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let sh = |args: &[&str]| {
            let out = std::process::Command::new("git").args(args).current_dir(tmp.path()).output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        };
        sh(&["init", "-q", "-b", "main"]);
        sh(&["config", "user.email", "t@t"]);
        sh(&["config", "user.name", "t"]);
        std::fs::write(tmp.path().join("word.txt"), "?\n").unwrap();
        sh(&["add", "-A"]);
        sh(&["commit", "-q", "-m", "init"]);
        let repo = crate::git::Repo::open(tmp.path()).unwrap();
        let image = crate::agent::Attachment::from_file(std::path::Path::new(&image_path)).unwrap();
        let mut ws = Workspace::new(repo.path(), repo.tracked_files().unwrap(), Access::ReadWrite)
            .unwrap()
            .with_write_mode(WriteMode::Live)
            .with_commands(true);
        let engine = match std::env::var("LIVE_ENGINE").as_deref() {
            Ok("claude-code") => Engine::ClaudeCode(claude_code::Config::default()),
            _ => match crate::claude::Client::from_store("claude-haiku-4-5-20251001") {
                Some(c) => Engine::Harness(Box::new(c)),
                None => return,
            },
        };
        let images = [image];
        let run = run_with(
            &engine,
            &mut ws,
            Request { images: &images, ..Request::new("The attached image shows a secret word. Replace the contents of word.txt with exactly that word, in capitals, and nothing else.") },
            &mut |e| println!("  {}", e.line()),
        )
        .unwrap();
        println!("{}", run.text);
        let word = std::fs::read_to_string(tmp.path().join("word.txt")).unwrap();
        assert!(word.trim().eq_ignore_ascii_case("MANGO"), "the model saw the image: {word:?}");
        assert!(!tmp.path().join(".devdock").exists(), "no image files left behind");
    }
}
