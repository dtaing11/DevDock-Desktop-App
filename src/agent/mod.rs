//! The agentic harness: a provider-agnostic tool-use loop.
//!
//! Both AI features that need to *look around the repository* run through
//! here rather than through a single prompt-and-answer request:
//!
//! - **Conflict resolution** gets a [`workspace::Access::ReadWrite`]
//!   workspace, so the model can read any tracked file for context and
//!   propose edits anywhere in the worktree — a conflict is often only
//!   resolvable by also touching the caller, the import, or the test that
//!   the two sides disagree about.
//! - **Code review** gets a [`workspace::Access::ReadOnly`] workspace, so
//!   the reviewer can open the files a diff touches and judge the change
//!   against the code around it instead of against a keyhole view.
//!
//! Nothing here writes to disk. Edit tools accumulate into an overlay
//! ([`workspace::Workspace::edits`]) that the app presents to the user file
//! by file; the user's confirmation is what actually writes. That is the
//! whole safety story for giving a model worktree-wide edit access: its
//! reach is proposals, not writes.
//!
//! # Providers
//!
//! [`Provider`] is the only thing the loop knows about a model. Claude
//! ([`crate::claude::Client`]) and Ollama ([`crate::ollama::Agent`]) each
//! implement it, so a task picks provider and model exactly the way commit
//! message generation does.
//!
//! # Budgets
//!
//! A loop that can read files can also read the whole repository into a
//! paid context window. [`Limits`] bounds turns, tool calls, and total bytes
//! read; when a budget runs out the tools are withdrawn and the model is
//! asked for its final answer with what it already has, which degrades to
//! roughly the quality of the old single-shot path rather than to an error.

pub mod assist;
pub mod coding;
pub mod conflict;
pub mod pr;
pub mod rebase;
pub mod split;
pub mod tickets;
pub mod workspace;

use serde::{Deserialize, Serialize};
pub use workspace::{Access, PendingEdit, Workspace, WriteMode};

/// One tool offered to the model, in a provider-neutral shape.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    /// JSON Schema for the tool's arguments.
    pub schema: serde_json::Value,
}

/// A tool invocation requested by the model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Provider-assigned id, echoed back with the result. Ollama does not
    /// supply one, so the loop synthesizes it there.
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

/// The outcome of running one [`ToolCall`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub id: String,
    pub name: String,
    pub content: String,
    pub is_error: bool,
}

/// One entry in the conversation the loop maintains. Providers translate
/// these into their own wire formats.
#[derive(Debug, Clone)]
pub enum Message {
    User(String),
    Assistant { text: String, calls: Vec<ToolCall> },
    ToolResults(Vec<ToolResult>),
}

/// Tokens one turn cost, as the provider reports them. Zero for providers
/// that do not say (Ollama).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Input tokens served from the provider's prompt cache: paid at a
    /// fraction of the price and not re-read by the model.
    pub cache_read_tokens: u64,
    /// Input tokens written to the cache this turn.
    pub cache_write_tokens: u64,
}

impl Usage {
    pub fn add(&mut self, other: &Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
    }

    pub fn is_zero(&self) -> bool {
        *self == Usage::default()
    }

    /// Everything the model was given, cached or not.
    pub fn total_input(&self) -> u64 {
        self.input_tokens + self.cache_read_tokens + self.cache_write_tokens
    }
}

/// What a provider returned for one turn: prose, tool calls, or both.
#[derive(Debug, Clone, Default)]
pub struct Reply {
    pub text: String,
    pub calls: Vec<ToolCall>,
    pub usage: Usage,
}

/// A model that can be driven in a tool-use loop.
pub trait Provider {
    /// Human-readable provider/model label, for error messages and the UI.
    fn label(&self) -> String;

    /// Sends one turn. `tools` may be empty, which means "answer now".
    fn turn(
        &self,
        system: &str,
        messages: &[Message],
        tools: &[ToolSpec],
        max_tokens: u32,
    ) -> Result<Reply, String>;

    /// How many bytes of transcript this model can actually hold, when the
    /// provider knows it is less than a run's budget. A local model with a
    /// fixed context window drops the oldest messages silently once it is
    /// full; eliding old tool output on purpose, before that happens, is
    /// what keeps it remembering the task. `None` means no such limit.
    fn transcript_budget(&self) -> Option<usize> {
        None
    }
}

/// Bounds on one run. The defaults are sized for a desktop app where the
/// user is watching a spinner: enough turns to read a handful of files and
/// think, not enough to grind.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_turns: usize,
    pub max_tool_calls: usize,
    pub max_read_bytes: usize,
    pub max_tokens: u32,
    /// Above this many bytes of transcript, old tool results are elided so
    /// the run can keep going instead of drowning in its own reads.
    pub max_transcript_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_turns: 12,
            max_tool_calls: 40,
            max_read_bytes: 240_000,
            max_tokens: 8192,
            max_transcript_bytes: 400_000,
        }
    }
}

/// One step of an agent's plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanStep {
    pub text: String,
    pub done: bool,
}

/// Progress from a run, for the UI to show while it happens.
#[derive(Debug, Clone)]
pub enum Event {
    /// The model's plan changed: it wrote one, or ticked something off.
    Plan(Vec<PlanStep>),
    /// A tool call, already summarized for display ("read src/git.rs").
    Tool { summary: String, is_error: bool },
    /// Prose the model emitted alongside its tool calls.
    Thought(String),
    /// A budget ran out; the model was asked to conclude.
    BudgetExhausted(String),
    /// The model tried to finish without checking its work; it was sent
    /// back to do so. Once per run.
    Nudge(String),
    /// Old tool output was elided to keep the transcript within budget.
    Compacted { freed: usize },
}

impl Event {
    /// One line for a progress log.
    pub fn line(&self) -> String {
        match self {
            Self::Plan(steps) => {
                let done = steps.iter().filter(|s| s.done).count();
                format!("· plan: {done}/{} done", steps.len())
            }
            Self::Tool { summary, is_error } => {
                if *is_error {
                    format!("! {summary}")
                } else {
                    format!("· {summary}")
                }
            }
            Self::Thought(t) => format!("… {}", first_line(t)),
            Self::BudgetExhausted(why) => format!("! {why}"),
            Self::Nudge(why) => format!("! not finished yet: {}", first_line(why)),
            Self::Compacted { freed } => format!("· trimmed {freed} bytes of old tool output"),
        }
    }
}

/// What one run produced.
#[derive(Debug, Clone, Default)]
pub struct Run {
    /// The model's final message: the merged-file explanation, the review
    /// JSON, whatever the task asked for.
    pub text: String,
    /// Proposed file changes, none of them written. Empty for read-only runs.
    pub edits: Vec<PendingEdit>,
    /// Progress lines, kept so the user can audit what the model looked at.
    pub log: Vec<String>,
    /// Whether a budget cut the run short.
    pub truncated: bool,
    /// Model turns taken.
    pub turns: usize,
    /// Tokens across every turn, when the provider reports them.
    pub usage: Usage,
}

/// Runs the loop to a final answer.
///
/// `on_event` is called as work happens so a GUI can show progress; pass
/// `&mut |_| {}` when nobody is watching.
pub fn run(
    provider: &dyn Provider,
    workspace: &mut Workspace,
    system: &str,
    task: &str,
    limits: Limits,
    on_event: &mut dyn FnMut(Event),
) -> Result<Run, String> {
    let mut messages = vec![Message::User(task.to_string())];
    let mut log: Vec<String> = Vec::new();
    let mut truncated = false;
    let mut nudged = false;
    let mut nudged_code = 0u8;
    let mut nudged_giveup = false;
    let mut usage = Usage::default();
    let mut turns;

    let emit = |event: Event, log: &mut Vec<String>, on_event: &mut dyn FnMut(Event)| {
        log.push(event.line());
        on_event(event);
    };

    for turn in 0..limits.max_turns {
        let out_of_calls = workspace.calls_used() >= limits.max_tool_calls;
        let out_of_bytes = workspace.bytes_read() >= limits.max_read_bytes;
        let last_turn = turn + 1 == limits.max_turns;
        let withhold_tools = out_of_calls || out_of_bytes || last_turn;

        if withhold_tools && !truncated {
            truncated = true;
            let why = if out_of_calls {
                format!("tool-call budget spent ({} calls)", limits.max_tool_calls)
            } else if out_of_bytes {
                format!("read budget spent ({} bytes)", limits.max_read_bytes)
            } else {
                format!("turn budget spent ({} turns)", limits.max_turns)
            };
            emit(Event::BudgetExhausted(format!("{why}; asking for a final answer")), &mut log, on_event);
            messages.push(Message::User(format!(
                "Your {why}. No more tools are available. Answer now, in the \
                 required format, using only what you have already gathered."
            )));
        }

        let tools: Vec<ToolSpec> = if withhold_tools { Vec::new() } else { workspace.tools() };
        let reply = provider.turn(system, &messages, &tools, limits.max_tokens)?;
        turns = turn + 1;
        usage.add(&reply.usage);

        if reply.calls.is_empty() {
            if reply.text.trim().is_empty() {
                return Err(format!("{} returned an empty answer.", provider.label()));
            }
            // A model that answers with the code instead of applying it —
            // the classic small-model failure, and one every model shows now
            // and then — is sent back once to use the tools it was given.
            if !withhold_tools
                && nudged_code < 2
                && workspace.can_edit()
                && workspace.edits().is_empty()
                && looks_like_unapplied_code(&reply.text)
            {
                nudged_code += 1;
                let why = "your answer contains code, but no file was changed".to_string();
                emit(Event::Nudge(why.clone()), &mut log, on_event);
                messages.push(Message::Assistant { text: reply.text, calls: Vec::new() });
                // The second time, spell out the exact call.
                let instruction = match (nudged_code, workspace.last_read()) {
                    (1, _) => "Writing code in the answer does nothing — apply it to the \
                               repository with write_file, edit_file, or replace_lines \
                               (repository-relative paths), verify it, and then answer. If no \
                               change is actually needed, say so plainly without a code block."
                        .to_string(),
                    (_, Some(path)) => format!(
                        "Do not write the code in your answer again. Make one tool call now: \
                         write_file with path \"{path}\" and content set to the complete new \
                         text of that file. Then verify and answer."
                    ),
                    (_, None) => "Do not write the code in your answer again. Make one tool \
                                  call now: write_file with the repository-relative path of the \
                                  file and its complete new content. Then verify and answer."
                        .to_string(),
                };
                messages.push(Message::User(format!("Not yet: {why}. {instruction}")));
                continue;
            }
            // A model that gives up and asks the developer a question — on
            // a run where it could have read the file or run the check that
            // would have answered it — is sent back once. Nobody is there to
            // answer; the tools are.
            if !withhold_tools
                && !nudged_giveup
                && workspace.can_edit()
                && workspace.edits().is_empty()
                && looks_like_giving_up(&reply.text)
            {
                nudged_giveup = true;
                let checks = workspace.checks_available();
                let hint = if !checks.is_empty() && workspace.check_runs() == 0 {
                    format!(
                        "Run the check first (run_check: {}) to see exactly what fails, read \
                         the files it names, and fix the cause.",
                        checks.join(", ")
                    )
                } else {
                    "Find the files with list_files, read them, and make the change.".to_string()
                };
                let why = "you asked the developer instead of using the tools".to_string();
                emit(Event::Nudge(why.clone()), &mut log, on_event);
                messages.push(Message::Assistant { text: reply.text, calls: Vec::new() });
                messages.push(Message::User(format!(
                    "Not yet: {why}. Nobody can answer questions during a run; decide for \
                     yourself and state your assumptions in the summary. {hint}"
                )));
                continue;
            }
            // A model that edited files and then stopped without checking
            // them is sent back once. The harness knows what was checked;
            // the model only knows what it remembers doing.
            if !withhold_tools && !nudged {
                if let Some(gap) = workspace.verification_gap() {
                    nudged = true;
                    emit(Event::Nudge(gap.clone()), &mut log, on_event);
                    messages.push(Message::Assistant { text: reply.text, calls: Vec::new() });
                    messages.push(Message::User(format!(
                        "Not yet. Before you finish: {gap} Do that now, fix anything it \
                         turns up, then give your final answer."
                    )));
                    continue;
                }
            }
            return Ok(Run {
                text: reply.text,
                edits: workspace.edits(),
                log,
                truncated,
                turns,
                usage,
            });
        }

        if !reply.text.trim().is_empty() {
            emit(Event::Thought(reply.text.clone()), &mut log, on_event);
        }

        let mut results = Vec::with_capacity(reply.calls.len());
        for call in &reply.calls {
            let result = workspace.dispatch(call, limits.max_tool_calls);
            // A plan update is not a step to log; it *is* the progress, and
            // the UI shows it as a checklist rather than another line.
            if call.name == "update_plan" && !result.is_error {
                emit(Event::Plan(workspace.plan()), &mut log, on_event);
            } else {
                // A failure says why, on the same line: the log is where a
                // developer finds out that an edit did not match, and a
                // bare "!" tells them nothing.
                let mut summary = workspace::summarize(call);
                if result.is_error {
                    summary.push_str(": ");
                    summary.push_str(&first_line(&result.content));
                }
                emit(
                    Event::Tool { summary, is_error: result.is_error },
                    &mut log,
                    on_event,
                );
            }
            results.push(result);
        }
        messages.push(Message::Assistant { text: reply.text, calls: reply.calls });
        messages.push(Message::ToolResults(results));

        let budget = provider
            .transcript_budget()
            .map_or(limits.max_transcript_bytes, |b| b.min(limits.max_transcript_bytes));
        if transcript_bytes(&messages) > budget {
            // Keep two turns of results whole; if that is still too much for
            // the window, keep one.
            let mut freed = compact(&mut messages, 2);
            if transcript_bytes(&messages) > budget {
                freed += compact(&mut messages, 1);
            }
            if freed > 0 {
                emit(Event::Compacted { freed }, &mut log, on_event);
            }
        }
    }

    Err(format!(
        "{} kept calling tools without producing an answer ({} turns).",
        provider.label(),
        limits.max_turns
    ))
}

/// An answer that hands the task back — a question to the developer, or a
/// request for information the tools could have found.
fn looks_like_giving_up(text: &str) -> bool {
    let lower = text.to_lowercase();
    const ASKS: &[&str] = &[
        "could you please",
        "can you please",
        "please provide",
        "please clarify",
        "please let me know",
        "let me know which",
        "let me know if you",
        "would you like me to",
        "do you want me to",
        "which file should",
        "more details about",
        "more information so",
        "i'm not able to locate",
        "i am not able to locate",
        "unable to locate",
        "could not find the file",
        "couldn't find the file",
        "cannot be completed",
        "can't be completed",
        "was not found in the repository",
        "does not exist in the repository",
        "doesn't exist in the repository",
        "no file named",
        "there is no file",
    ];
    ASKS.iter().any(|a| lower.contains(a))
}

/// An answer that carries a substantial fenced code block: several lines of
/// what is probably the change the model meant to make. A one-line snippet
/// quoted in an explanation does not count.
fn looks_like_unapplied_code(text: &str) -> bool {
    let mut in_block = false;
    let mut lines_in_block = 0;
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            if in_block && lines_in_block >= 4 {
                return true;
            }
            in_block = !in_block;
            lines_in_block = 0;
        } else if in_block {
            lines_in_block += 1;
        }
    }
    false
}

/// How much of the transcript the provider is sent each turn.
fn transcript_bytes(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|m| match m {
            Message::User(t) => t.len(),
            Message::Assistant { text, calls } => {
                text.len() + calls.iter().map(|c| c.input.to_string().len()).sum::<usize>()
            }
            Message::ToolResults(results) => results.iter().map(|r| r.content.len()).sum(),
        })
        .sum()
}

/// Elided-result marker, so a result is not elided twice.
const ELIDED: &str = "[elided]";

/// Replaces large tool results, except those from the most recent
/// `keep_recent` tool turns, with a note saying what they were. The model
/// has already acted on them; if it needs one again it can call the tool
/// again, which costs one call instead of carrying every file it ever read
/// through every remaining turn. Returns the bytes freed.
pub fn compact(messages: &mut [Message], keep_recent: usize) -> usize {
    /// Results shorter than this are cheap to keep and often load-bearing
    /// (an error, a plan acknowledgement).
    const MIN_ELIDE: usize = 400;
    let result_turns: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| matches!(m, Message::ToolResults(_)))
        .map(|(i, _)| i)
        .collect();
    let elidable = result_turns.len().saturating_sub(keep_recent);
    let mut freed = 0;
    for &index in &result_turns[..elidable] {
        let Message::ToolResults(results) = &mut messages[index] else { continue };
        for result in results.iter_mut() {
            if result.content.len() < MIN_ELIDE || result.content.starts_with(ELIDED) {
                continue;
            }
            let was = result.content.len();
            result.content = format!(
                "{ELIDED} earlier {} output, {was} bytes, removed to save context. Call the \
                 tool again if you still need it.",
                result.name
            );
            freed += was - result.content.len();
        }
    }
    freed
}

fn first_line(text: &str) -> String {
    let line = text.trim().lines().next().unwrap_or_default();
    if line.chars().count() > 100 {
        let cut: String = line.chars().take(100).collect();
        format!("{cut}…")
    } else {
        line.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A provider that replays a fixed script of replies.
    struct Scripted {
        replies: RefCell<Vec<Reply>>,
        /// Whether each turn was offered tools, recorded in order.
        offered: RefCell<Vec<bool>>,
    }

    impl Scripted {
        fn new(replies: Vec<Reply>) -> Self {
            Self { replies: RefCell::new(replies), offered: RefCell::new(Vec::new()) }
        }
    }

    impl Provider for Scripted {
        fn label(&self) -> String {
            "scripted".into()
        }
        fn turn(
            &self,
            _system: &str,
            _messages: &[Message],
            tools: &[ToolSpec],
            _max_tokens: u32,
        ) -> Result<Reply, String> {
            self.offered.borrow_mut().push(!tools.is_empty());
            let mut replies = self.replies.borrow_mut();
            if replies.is_empty() {
                return Ok(Reply { text: "done".into(), calls: Vec::new(), ..Default::default() });
            }
            Ok(replies.remove(0))
        }
    }

    fn call(id: &str, name: &str, input: serde_json::Value) -> ToolCall {
        ToolCall { id: id.into(), name: name.into(), input }
    }

    fn workspace(dir: &std::path::Path, files: &[(&str, &str)], access: Access) -> Workspace {
        for (path, body) in files {
            let full = dir.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(full, body).unwrap();
        }
        Workspace::new(
            dir,
            files.iter().map(|(p, _)| p.to_string()).collect(),
            access,
        )
        .unwrap()
    }

    #[test]
    fn returns_final_text_when_no_tools_are_called() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ws = workspace(tmp.path(), &[("a.txt", "hi\n")], Access::ReadOnly);
        let provider = Scripted::new(vec![Reply { text: "verdict".into(), calls: vec![], ..Default::default() }]);
        let run = run(&provider, &mut ws, "sys", "task", Limits::default(), &mut |_| {}).unwrap();
        assert_eq!(run.text, "verdict");
        assert!(run.edits.is_empty());
        assert!(!run.truncated);
    }

    #[test]
    fn feeds_tool_results_back_and_logs_them() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ws = workspace(tmp.path(), &[("a.txt", "hello\n")], Access::ReadOnly);
        let provider = Scripted::new(vec![
            Reply {
                text: "looking".into(),
                calls: vec![call("1", "read_file", serde_json::json!({"path": "a.txt"}))], ..Default::default()
            },
            Reply { text: "final".into(), calls: vec![], ..Default::default() },
        ]);
        let mut events = Vec::new();
        let run = run(&provider, &mut ws, "sys", "task", Limits::default(), &mut |e| {
            events.push(e.line())
        })
        .unwrap();
        assert_eq!(run.text, "final");
        assert!(run.log.iter().any(|l| l.contains("read a.txt")), "log: {:?}", run.log);
        assert!(events.iter().any(|l| l.contains("read a.txt")));
    }

    #[test]
    fn withdraws_tools_once_the_call_budget_is_spent() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ws = workspace(tmp.path(), &[("a.txt", "hello\n")], Access::ReadOnly);
        let provider = Scripted::new(vec![
            Reply {
                text: String::new(),
                calls: vec![call("1", "read_file", serde_json::json!({"path": "a.txt"}))], ..Default::default()
            },
            Reply { text: "wrapping up".into(), calls: vec![], ..Default::default() },
        ]);
        let limits = Limits { max_tool_calls: 1, ..Limits::default() };
        let run = run(&provider, &mut ws, "sys", "task", limits, &mut |_| {}).unwrap();
        assert!(run.truncated);
        assert_eq!(provider.offered.borrow().as_slice(), &[true, false]);
    }

    #[test]
    fn errors_when_the_model_never_answers() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ws = workspace(tmp.path(), &[("a.txt", "x\n")], Access::ReadOnly);
        let looping: Vec<Reply> = (0..4)
            .map(|i| Reply {
                text: String::new(),
                calls: vec![call(
                    &i.to_string(),
                    "read_file",
                    serde_json::json!({"path": "a.txt"}),
                )], ..Default::default()
            })
            .collect();
        let provider = Scripted::new(looping);
        let limits = Limits { max_turns: 3, ..Limits::default() };
        let err = run(&provider, &mut ws, "sys", "task", limits, &mut |_| {}).unwrap_err();
        assert!(err.contains("without producing an answer"), "{err}");
    }

    #[test]
    fn old_tool_output_is_elided_once_the_transcript_is_large() {
        let tmp = tempfile::tempdir().unwrap();
        let big = "x".repeat(2_000) + "\n";
        let mut ws = workspace(tmp.path(), &[("a.txt", &big)], Access::ReadOnly);
        let read = |id: &str| Reply {
            text: String::new(),
            calls: vec![call(id, "read_file", serde_json::json!({"path": "a.txt"}))],
            ..Default::default()
        };
        let provider = Scripted::new(vec![read("1"), read("2"), read("3"), read("4")]);
        let limits = Limits { max_transcript_bytes: 5_000, ..Limits::default() };
        let mut events = Vec::new();
        let run = run(&provider, &mut ws, "sys", "task", limits, &mut |e| events.push(e.line()))
            .unwrap();
        assert_eq!(run.text, "done");
        assert_eq!(run.turns, 5);
        assert!(events.iter().any(|l| l.contains("trimmed")), "{events:?}");
    }

    #[test]
    fn a_provider_with_a_small_window_gets_compacted_sooner() {
        struct Small(Scripted);
        impl Provider for Small {
            fn label(&self) -> String {
                self.0.label()
            }
            fn turn(
                &self,
                s: &str,
                m: &[Message],
                t: &[ToolSpec],
                x: u32,
            ) -> Result<Reply, String> {
                self.0.turn(s, m, t, x)
            }
            fn transcript_budget(&self) -> Option<usize> {
                Some(3_000)
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let big = "x".repeat(2_000) + "\n";
        let mut ws = workspace(tmp.path(), &[("a.txt", &big)], Access::ReadOnly);
        let read = |id: &str| Reply {
            text: String::new(),
            calls: vec![call(id, "read_file", serde_json::json!({"path": "a.txt"}))],
            ..Default::default()
        };
        let provider = Small(Scripted::new(vec![read("1"), read("2"), read("3")]));
        let mut events = Vec::new();
        // The run's own budget is huge; the provider's window is what bites.
        run(&provider, &mut ws, "sys", "task", Limits::default(), &mut |e| events.push(e.line()))
            .unwrap();
        assert!(events.iter().any(|l| l.contains("trimmed")), "{events:?}");
    }

    #[test]
    fn compaction_keeps_recent_results_and_small_ones() {
        let big = "y".repeat(1_000);
        let result = |id: &str, content: &str| {
            Message::ToolResults(vec![ToolResult {
                id: id.into(),
                name: "read_file".into(),
                content: content.into(),
                is_error: false,
            }])
        };
        let mut messages = vec![
            Message::User("task".into()),
            result("1", &big),
            result("2", "short error"),
            result("3", &big),
            result("4", &big),
        ];
        let freed = compact(&mut messages, 1);
        assert!(freed > 1_500, "{freed}");
        let contents: Vec<String> = messages
            .iter()
            .filter_map(|m| match m {
                Message::ToolResults(r) => Some(r[0].content.clone()),
                _ => None,
            })
            .collect();
        assert!(contents[0].starts_with(ELIDED));
        assert_eq!(contents[1], "short error", "small results are kept");
        assert!(contents[2].starts_with(ELIDED));
        assert_eq!(contents[3], big, "the most recent turn is kept whole");
        // Running again frees nothing more.
        assert_eq!(compact(&mut messages, 1), 0);
    }

    #[test]
    fn an_answer_with_unapplied_code_is_sent_back_once() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ws = workspace(tmp.path(), &[("a.py", "x = 1\n")], Access::ReadWrite);
        let prose_with_code = "Here is the fix:\n```python\nx = 1\ny = 2\nz = 3\nw = 4\nq = 5\n```\n";
        let provider = Scripted::new(vec![
            Reply {
                text: String::new(),
                calls: vec![call("0", "read_file", serde_json::json!({"path": "a.py"}))],
                ..Default::default()
            },
            Reply { text: prose_with_code.into(), ..Default::default() },
            // Twice: the second nudge names the file it just read.
            Reply { text: prose_with_code.into(), ..Default::default() },
            Reply {
                text: String::new(),
                calls: vec![call(
                    "1",
                    "write_file",
                    serde_json::json!({"path": "a.py", "content": "x = 1\ny = 2\n"}),
                )],
                ..Default::default()
            },
            Reply { text: "applied".into(), ..Default::default() },
        ]);
        let mut events = Vec::new();
        let run = run(&provider, &mut ws, "sys", "task", Limits::default(), &mut |e| {
            events.push(e.line())
        })
        .unwrap();
        assert_eq!(run.text, "applied");
        assert_eq!(run.edits.len(), 1);
        assert_eq!(events.iter().filter(|l| l.contains("no file was changed")).count(), 2, "{events:?}");

        // A read-only run, or an answer with only a short snippet, is left alone.
        let mut ro = workspace(tmp.path(), &[("a.py", "x = 1\n")], Access::ReadOnly);
        let provider = Scripted::new(vec![Reply { text: prose_with_code.into(), ..Default::default() }]);
        let ro_run = super::run(&provider, &mut ro, "sys", "task", Limits::default(), &mut |_| {}).unwrap();
        assert_eq!(ro_run.text, prose_with_code);
        assert!(!looks_like_unapplied_code("Use `x = 1`.\n```\nx = 1\n```\n"));
    }

    #[test]
    fn asking_the_developer_gets_the_model_sent_back_to_the_tools() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ws = workspace(tmp.path(), &[("a.py", "x = 1\n")], Access::ReadWrite)
            .with_write_mode(WriteMode::Live)
            .with_checks(vec![crate::local_ci::Job {
                name: "tests".into(),
                commands: vec!["true".into()],
                ..Default::default()
            }]);
        let provider = Scripted::new(vec![
            Reply {
                text: "I could not find the file. Could you please provide more details?".into(),
                ..Default::default()
            },
            Reply {
                text: String::new(),
                calls: vec![call("1", "run_check", serde_json::json!({"name": "tests"}))],
                ..Default::default()
            },
            Reply { text: "Nothing fails; no change needed.".into(), ..Default::default() },
        ]);
        let mut events = Vec::new();
        let run = run(&provider, &mut ws, "sys", "task", Limits::default(), &mut |e| {
            events.push(e.line())
        })
        .unwrap();
        assert!(run.text.contains("no change needed"));
        assert!(events.iter().any(|l| l.contains("asked the developer")), "{events:?}");
        assert_eq!(run.turns, 3);
    }

    #[test]
    fn a_live_run_that_skips_its_checks_is_sent_back_once() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ws = workspace(tmp.path(), &[("a.txt", "old\n")], Access::ReadWrite)
            .with_write_mode(WriteMode::Live)
            .with_checks(vec![crate::local_ci::Job {
                name: "tests".into(),
                commands: vec!["true".into()],
                ..Default::default()
            }]);
        let provider = Scripted::new(vec![
            Reply {
                text: String::new(),
                calls: vec![call(
                    "1",
                    "write_file",
                    serde_json::json!({"path": "a.txt", "content": "new\n"}),
                )],
                ..Default::default()
            },
            // Tries to finish without running anything.
            Reply { text: "all done".into(), ..Default::default() },
            // Sent back, it runs the check.
            Reply {
                text: String::new(),
                calls: vec![call("2", "run_check", serde_json::json!({"name": "tests"}))],
                ..Default::default()
            },
            Reply { text: "verified".into(), ..Default::default() },
        ]);
        let mut events = Vec::new();
        let run = run(&provider, &mut ws, "sys", "task", Limits::default(), &mut |e| {
            events.push(e.line())
        })
        .unwrap();
        assert_eq!(run.text, "verified");
        assert!(events.iter().any(|l| l.contains("not finished yet")), "{events:?}");
        assert_eq!(run.turns, 4);
    }

    #[test]
    fn collects_edits_without_touching_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ws = workspace(tmp.path(), &[("a.txt", "old\n")], Access::ReadWrite);
        let provider = Scripted::new(vec![
            Reply {
                text: String::new(),
                calls: vec![call(
                    "1",
                    "write_file",
                    serde_json::json!({"path": "a.txt", "content": "new\n"}),
                )], ..Default::default()
            },
            Reply { text: "merged".into(), calls: vec![], ..Default::default() },
        ]);
        let run = run(&provider, &mut ws, "sys", "task", Limits::default(), &mut |_| {}).unwrap();
        assert_eq!(run.edits.len(), 1);
        assert_eq!(run.edits[0].after, "new\n");
        assert_eq!(std::fs::read_to_string(tmp.path().join("a.txt")).unwrap(), "old\n");
    }
}
