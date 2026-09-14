//! The AI actions that act on the code in front of you: explain it, fix
//! what the language server is complaining about, write its tests, write
//! its documentation.
//!
//! Each one is the same harness with a different question. What makes them
//! useful rather than a toy is that they are given the *place* — the file,
//! the selected lines, the diagnostics on them — and can then read whatever
//! else they need. "Explain this" with only the selection produces a
//! paraphrase; with the repository it can say what the caller expects.

use super::{Access, Event, Limits, Provider, Run, Workspace};
use crate::lsp::protocol::Diagnostic;

/// Which action was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Explain,
    Fix,
    Tests,
    Document,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Explain => "Explain",
            Self::Fix => "Fix",
            Self::Tests => "Write tests",
            Self::Document => "Document",
        }
    }

    /// Whether it proposes edits, or only answers.
    pub fn edits(self) -> bool {
        !matches!(self, Self::Explain)
    }

    pub fn access(self) -> Access {
        if self.edits() { Access::ReadWrite } else { Access::ReadOnly }
    }

    fn instruction(self) -> &'static str {
        match self {
            Self::Explain => {
                "Explain the selected code: what it does, why it is written this way, and \
                 anything about it a reader should be careful of. Read what it calls and \
                 what calls it — an explanation that only paraphrases the lines in front \
                 of you is worth nothing. Be concrete about the cases it handles and the \
                 ones it does not. Do not edit anything; answer in Markdown."
            }
            Self::Fix => {
                "Fix the problems in the selected code. The language server's diagnostics \
                 are below when there are any; if there are none, look for what is \
                 actually wrong rather than inventing something to change. Read the \
                 definitions involved before deciding what the correct behaviour is. Make \
                 the smallest change that fixes it, and explain what was wrong."
            }
            Self::Tests => {
                "Write tests for the selected code. Read the existing tests in this \
                 repository first and match how they are written: the same framework, the \
                 same naming, the same file layout — a test that does not look like the \
                 others is a test nobody maintains. Cover the cases that actually matter, \
                 including the failing ones, and skip the trivially true. Add them where \
                 this project puts its tests."
            }
            Self::Document => {
                "Write documentation comments for the selected code, in this project's \
                 own style — read the surrounding file first. Say what the reader cannot \
                 work out from the signature: why it exists, what it assumes, what it \
                 does at the edges. Do not restate the parameter names in prose, and do \
                 not document the obvious."
            }
        }
    }
}

const BASE_PROMPT: &str = r#"You are working on one piece of a real repository, on behalf of the developer looking at it.

Tools: list_files, read_file and search see every tracked file. Use them — the selection alone rarely says enough to answer well.

Finish with a short Markdown account of what you did or found. Do not pad it, and do not restate the code back."#;

const EDIT_NOTE: &str = r#"

write_file and edit_file propose changes. Nothing reaches disk: the developer reviews every change as a diff and accepts or rejects it, so propose the change you believe is right and say what it does."#;

pub fn limits() -> Limits {
    Limits { max_turns: 14, max_tool_calls: 24, max_read_bytes: 200_000, max_tokens: 4096, max_transcript_bytes: 400_000 }
}

/// Builds the task turn: where we are, what is selected, what is wrong.
fn task_prompt(
    kind: Kind,
    rel: &str,
    selection: &str,
    lines: Option<(u32, u32)>,
    diagnostics: &[Diagnostic],
) -> String {
    let mut prompt = String::new();
    match lines {
        Some((start, end)) if start != end => {
            prompt.push_str(&format!("In `{rel}`, lines {start}-{end}:\n\n"));
        }
        Some((start, _)) => prompt.push_str(&format!("In `{rel}`, at line {start}:\n\n")),
        None => prompt.push_str(&format!("In `{rel}`:\n\n")),
    }
    prompt.push_str("```\n");
    prompt.push_str(selection.trim_end());
    prompt.push_str("\n```\n");

    if !diagnostics.is_empty() {
        prompt.push_str("\nThe language server reports:\n");
        for diagnostic in diagnostics.iter().take(20) {
            prompt.push_str(&format!("- {}\n", diagnostic.line()));
        }
    }
    prompt.push('\n');
    prompt.push_str(kind.instruction());
    prompt
}

/// Where the action is being asked about.
pub struct Target<'a> {
    /// Repo-relative path of the file.
    pub rel: &'a str,
    /// The selected text, or the whole file when nothing is selected.
    pub selection: &'a str,
    /// 1-based line range of the selection, when there is one.
    pub lines: Option<(u32, u32)>,
    /// Diagnostics that fall inside it.
    pub diagnostics: &'a [Diagnostic],
    /// Project guidance, appended to the system prompt.
    pub instructions: Option<&'a str>,
}

/// Runs one assist action.
pub fn run(
    provider: &dyn Provider,
    workspace: &mut Workspace,
    kind: Kind,
    target: Target<'_>,
    on_event: &mut dyn FnMut(Event),
) -> Result<Run, String> {
    let Target { rel, selection, lines, diagnostics, instructions: extra_instructions } = target;
    if selection.trim().is_empty() {
        return Err("Nothing selected, and the file is empty.".into());
    }
    let mut system = String::from(BASE_PROMPT);
    if kind.edits() {
        system.push_str(EDIT_NOTE);
    }
    if let Some(extra) = extra_instructions.map(str::trim).filter(|s| !s.is_empty()) {
        system.push_str("\n\nProject-specific instructions:\n");
        system.push_str(extra);
    }
    let prompt = task_prompt(kind, rel, selection, lines, diagnostics);
    super::run(provider, workspace, &system, &prompt, limits(), on_event)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Message, Reply, ToolCall, ToolSpec};
    use crate::lsp::protocol::{Position, Range, Severity};
    use std::cell::RefCell;

    struct Scripted {
        replies: RefCell<Vec<Reply>>,
        prompts: RefCell<Vec<String>>,
        systems: RefCell<Vec<String>>,
    }

    impl Provider for Scripted {
        fn label(&self) -> String {
            "scripted".into()
        }
        fn turn(
            &self,
            system: &str,
            messages: &[Message],
            _: &[ToolSpec],
            _: u32,
        ) -> Result<Reply, String> {
            self.systems.borrow_mut().push(system.to_string());
            if let Some(Message::User(text)) = messages.first() {
                self.prompts.borrow_mut().push(text.clone());
            }
            Ok(self.replies.borrow_mut().remove(0))
        }
    }

    fn scripted(replies: Vec<Reply>) -> Scripted {
        Scripted {
            replies: RefCell::new(replies),
            prompts: RefCell::new(Vec::new()),
            systems: RefCell::new(Vec::new()),
        }
    }

    fn fixture(access: Access) -> (tempfile::TempDir, Workspace) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("lib.rs"), "pub fn halve(n: u32) -> u32 { n / 0 }\n")
            .unwrap();
        let ws = Workspace::new(tmp.path(), vec!["lib.rs".into()], access).unwrap();
        (tmp, ws)
    }

    fn diagnostic(message: &str) -> Diagnostic {
        Diagnostic {
            range: Range { start: Position::new(0, 30), end: Position::new(0, 35) },
            severity: Severity::Error,
            code: Some("E0080".into()),
            message: message.into(),
            source: Some("rustc".into()),
        }
    }

    #[test]
    fn explaining_reads_the_repository_and_cannot_edit_it() {
        let (tmp, mut ws) = fixture(Kind::Explain.access());
        let provider = scripted(vec![
            Reply {
                text: String::new(),
                calls: vec![ToolCall {
                    id: "1".into(),
                    name: "write_file".into(),
                    input: serde_json::json!({"path": "lib.rs", "content": "nope"}),
                }], ..Default::default()
            },
            Reply { text: "It halves a number.".into(), calls: vec![], ..Default::default() },
        ]);

        let run = run(
            &provider,
            &mut ws,
            Kind::Explain,
            Target {
                rel: "lib.rs",
                selection: "pub fn halve(n: u32) -> u32 { n / 0 }",
                lines: Some((1, 1)),
                diagnostics: &[],
                instructions: None,
            },
            &mut |_| {},
        )
        .unwrap();

        assert_eq!(run.text, "It halves a number.");
        assert!(run.edits.is_empty(), "explaining must not change anything");
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("lib.rs")).unwrap(),
            "pub fn halve(n: u32) -> u32 { n / 0 }\n"
        );
        // The prompt says where we are, and the system prompt offers no
        // edit tools.
        assert!(provider.prompts.borrow()[0].contains("lib.rs"));
        assert!(!provider.systems.borrow()[0].contains("write_file"));
    }

    #[test]
    fn fixing_carries_the_diagnostics_and_proposes_an_edit() {
        let (_tmp, mut ws) = fixture(Kind::Fix.access());
        let provider = scripted(vec![
            Reply {
                text: String::new(),
                calls: vec![ToolCall {
                    id: "1".into(),
                    name: "edit_file".into(),
                    input: serde_json::json!({
                        "path": "lib.rs", "old_text": "n / 0", "new_text": "n / 2"
                    }),
                }], ..Default::default()
            },
            Reply { text: "Divided by two, not zero.".into(), calls: vec![], ..Default::default() },
        ]);

        let diagnostics = [diagnostic("this operation will panic at runtime")];
        let run = run(
            &provider,
            &mut ws,
            Kind::Fix,
            Target {
                rel: "lib.rs",
                selection: "pub fn halve(n: u32) -> u32 { n / 0 }",
                lines: Some((1, 1)),
                diagnostics: &diagnostics,
                instructions: None,
            },
            &mut |_| {},
        )
        .unwrap();

        assert_eq!(run.edits.len(), 1);
        assert!(run.edits[0].after.contains("n / 2"));
        let prompt = &provider.prompts.borrow()[0];
        assert!(prompt.contains("this operation will panic"), "{prompt}");
        assert!(provider.systems.borrow()[0].contains("reviews every change"));
    }

    #[test]
    fn each_action_asks_for_something_different() {
        for (kind, expected) in [
            (Kind::Explain, "Explain the selected code"),
            (Kind::Fix, "Fix the problems"),
            (Kind::Tests, "match how they are written"),
            (Kind::Document, "this project's own style"),
        ] {
            let prompt = task_prompt(kind, "a.rs", "code", Some((1, 2)), &[]);
            assert!(prompt.contains(expected), "{kind:?}: {prompt}");
        }
        assert!(!Kind::Explain.edits());
        assert!(Kind::Tests.edits());
    }

    #[test]
    fn an_empty_selection_is_refused_before_a_model_is_called() {
        let (_tmp, mut ws) = fixture(Access::ReadOnly);
        let provider = scripted(Vec::new());
        let err = run(
            &provider,
            &mut ws,
            Kind::Explain,
            Target {
                rel: "lib.rs",
                selection: "   \n",
                lines: None,
                diagnostics: &[],
                instructions: None,
            },
            &mut |_| {},
        )
        .unwrap_err();
        assert!(err.contains("Nothing selected"), "{err}");
    }
}
