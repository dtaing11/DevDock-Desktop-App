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

pub mod coding;
pub mod conflict;
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

/// What a provider returned for one turn: prose, tool calls, or both.
#[derive(Debug, Clone, Default)]
pub struct Reply {
    pub text: String,
    pub calls: Vec<ToolCall>,
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
}

impl Default for Limits {
    fn default() -> Self {
        Self { max_turns: 12, max_tool_calls: 40, max_read_bytes: 240_000, max_tokens: 8192 }
    }
}

/// Progress from a run, for the UI to show while it happens.
#[derive(Debug, Clone)]
pub enum Event {
    /// A tool call, already summarized for display ("read src/git.rs").
    Tool { summary: String, is_error: bool },
    /// Prose the model emitted alongside its tool calls.
    Thought(String),
    /// A budget ran out; the model was asked to conclude.
    BudgetExhausted(String),
}

impl Event {
    /// One line for a progress log.
    pub fn line(&self) -> String {
        match self {
            Self::Tool { summary, is_error } => {
                if *is_error {
                    format!("! {summary}")
                } else {
                    format!("· {summary}")
                }
            }
            Self::Thought(t) => format!("… {}", first_line(t)),
            Self::BudgetExhausted(why) => format!("! {why}"),
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

        if reply.calls.is_empty() {
            if reply.text.trim().is_empty() {
                return Err(format!("{} returned an empty answer.", provider.label()));
            }
            return Ok(Run { text: reply.text, edits: workspace.edits(), log, truncated });
        }

        if !reply.text.trim().is_empty() {
            emit(Event::Thought(reply.text.clone()), &mut log, on_event);
        }

        let mut results = Vec::with_capacity(reply.calls.len());
        for call in &reply.calls {
            let result = workspace.dispatch(call, limits.max_tool_calls);
            emit(
                Event::Tool {
                    summary: workspace::summarize(call),
                    is_error: result.is_error,
                },
                &mut log,
                on_event,
            );
            results.push(result);
        }
        messages.push(Message::Assistant { text: reply.text, calls: reply.calls });
        messages.push(Message::ToolResults(results));
    }

    Err(format!(
        "{} kept calling tools without producing an answer ({} turns).",
        provider.label(),
        limits.max_turns
    ))
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
                return Ok(Reply { text: "done".into(), calls: Vec::new() });
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
        let provider = Scripted::new(vec![Reply { text: "verdict".into(), calls: vec![] }]);
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
                calls: vec![call("1", "read_file", serde_json::json!({"path": "a.txt"}))],
            },
            Reply { text: "final".into(), calls: vec![] },
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
                calls: vec![call("1", "read_file", serde_json::json!({"path": "a.txt"}))],
            },
            Reply { text: "wrapping up".into(), calls: vec![] },
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
                )],
            })
            .collect();
        let provider = Scripted::new(looping);
        let limits = Limits { max_turns: 3, ..Limits::default() };
        let err = run(&provider, &mut ws, "sys", "task", limits, &mut |_| {}).unwrap_err();
        assert!(err.contains("without producing an answer"), "{err}");
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
                )],
            },
            Reply { text: "merged".into(), calls: vec![] },
        ]);
        let run = run(&provider, &mut ws, "sys", "task", Limits::default(), &mut |_| {}).unwrap();
        assert_eq!(run.edits.len(), 1);
        assert_eq!(run.edits[0].after, "new\n");
        assert_eq!(std::fs::read_to_string(tmp.path().join("a.txt")).unwrap(), "old\n");
    }
}
