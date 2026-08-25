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

use super::{Event, Limits, Provider, Run, Workspace, WriteMode};

/// One earlier exchange in the same session.
#[derive(Debug, Clone)]
pub struct Turn {
    pub task: String,
    pub summary: String,
}

const BASE_PROMPT: &str = r#"You are a coding agent working in a real repository that a developer is watching.

How to work:
- Understand before you change anything. Read the files involved, and the code around them. A change that looks right in isolation and breaks its caller is worse than no change.
- Prefer the smallest change that does the job. Match the surrounding code's style, naming, and structure — someone reviewing the diff should not be able to tell which lines you wrote.
- Do not rewrite, reformat, or "clean up" code the task did not ask about. An unrequested refactor buried in a real change is how a review gets abandoned.
- When the task is a question rather than a change, answer it. Do not edit files to prove a point.
- If the task is ambiguous in a way that changes what you would write, say so in your summary and implement the reading you think is right, rather than guessing silently or refusing.
- If you cannot do something, say which part and why. A partial change plus an honest account of what is missing is useful; a confident summary of work you did not do is not.

Finish with a short Markdown summary: what you changed and why, one bullet per file, then anything you could not verify or deliberately left alone."#;

const READ_TOOLS: &str = r#"
Reading the repository: list_files, read_file, and search see every file git tracks."#;

const EDIT_TOOLS_OVERLAY: &str = r#"
Changing it: write_file and edit_file. Your changes are held for the developer to review as diffs — nothing reaches disk until they accept it, file by file. That also means you cannot compile or test what you write in this run, so be correspondingly careful and say plainly in your summary what you could not verify."#;

const EDIT_TOOLS_LIVE: &str = r#"
Changing it: write_file and edit_file write to the developer's working tree immediately, so the language server and the project's checks see your work. They review every change at the end and can revert any of it, so leave the tree in a state you would be willing to show: no debugging leftovers, no half-finished edit you meant to come back to."#;

const LANGUAGE_TOOLS: &str = r#"
The language server: diagnostics, definition, references, find_symbol. Use it rather than guessing — references finds every caller a signature change breaks, which a text search cannot do reliably, and diagnostics is how you check your own work. After changing a file, ask for its diagnostics before moving on."#;

const CHECK_TOOLS: &str = r#"
The project's checks: run_check runs one of the commands this repository already declares (build, tests, lint). Run the relevant one before you report a change as done, and if it fails, fix what you broke rather than reporting it as finished."#;

/// The system prompt for a run, describing exactly the tools it has.
pub fn system_prompt(
    write_mode: WriteMode,
    has_language_tools: bool,
    has_checks: bool,
    extra_instructions: Option<&str>,
) -> String {
    let mut prompt = String::from(BASE_PROMPT);
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
pub fn task_prompt(task: &str, history: &[Turn], branch: Option<&str>) -> String {
    let mut prompt = String::new();
    if let Some(branch) = branch {
        prompt.push_str(&format!("You are on branch `{branch}`.\n\n"));
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
    Limits { max_turns: 40, max_tool_calls: 120, max_read_bytes: 600_000, max_tokens: 8192 }
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
    pub limits: Limits,
}

impl<'a> Request<'a> {
    /// A request with default budgets and no context.
    pub fn new(task: &'a str) -> Self {
        Self {
            task,
            history: &[],
            branch: None,
            instructions: None,
            limits: limits(),
        }
    }
}

/// Runs the coding agent.
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
    let system = system_prompt(
        workspace.write_mode(),
        tools.contains(&"diagnostics"),
        tools.contains(&"run_check"),
        request.instructions,
    );
    let prompt = task_prompt(request.task, request.history, request.branch);
    super::run(provider, workspace, &system, &prompt, request.limits, on_event)
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
        let bare = system_prompt(WriteMode::Overlay, false, false, None);
        assert!(bare.contains("list_files"));
        assert!(!bare.contains("diagnostics"), "no language server was offered");
        assert!(!bare.contains("run_check"), "no checks were offered");
        assert!(bare.contains("nothing reaches disk"));

        let full = system_prompt(WriteMode::Live, true, true, Some("Never touch vendor/."));
        assert!(full.contains("diagnostics") && full.contains("run_check"));
        assert!(full.contains("write to the developer's working tree immediately"));
        assert!(full.contains("Never touch vendor/."));
    }

    #[test]
    fn the_task_prompt_carries_the_session_so_far() {
        let history = vec![Turn {
            task: "add a flag".into(),
            summary: "added --verbose to cli.rs".into(),
        }];
        let prompt = task_prompt("now document it", &history, Some("feat/flags"));
        assert!(prompt.contains("feat/flags"));
        assert!(prompt.contains("add a flag"));
        assert!(prompt.contains("added --verbose"));
        assert!(prompt.ends_with("now document it"));
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
                }],
            },
            Reply { text: "- a.rs: returns a value now".into(), calls: vec![] },
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
}
