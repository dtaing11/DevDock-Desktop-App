//! Pull request text, written by a model that can read the repository.
//!
//! The commits and the diff are handed over up front ([`crate::ollama::pr_prompt`]),
//! which is usually enough. What the harness adds is the ability to *check*:
//! open the file a hunk lives in, read the function a commit says it fixed,
//! search for the other callers a signature change affects. A diff shows what
//! changed; the repository shows what it means.
//!
//! Read-only, like the review gate. Describing a change is not a reason to be
//! able to alter it.

use super::{Access, Event, Limits, Provider, Workspace};
use crate::git::BranchSummary;
use crate::ollama::CommitSuggestion;

/// Added to the pull request system prompt when the model can read the repo.
const CONTEXT_PROMPT: &str = r#"

You can read this repository while you write. Tools: list_files, read_file, search. They see every file git tracks.

The commits and the diff below are the change. Open what you need to describe it accurately:
- The file a hunk sits in, when the surrounding code is what makes the change make sense.
- The definition of anything the change calls or alters, to describe its effect rather than its shape.
- The other callers of anything whose signature moved, so the description can say what else it touches.
- A test the change adds, to say what behaviour is now covered.

Read what you need and no more; a handful of targeted reads beats crawling the tree. Then describe the branch as a whole. Do not turn the description into a file-by-file tour of the diff — say what the change does, and mention specific files only where a reviewer needs to look."#;

/// Budgets for one pull request run. Smaller than a review's: this is a
/// description, not an audit, and the commits already say most of it.
pub fn limits() -> Limits {
    Limits { max_turns: 10, max_tool_calls: 16, max_read_bytes: 150_000, max_tokens: 2048 }
}

/// Writes a pull request title and body, with the repository open.
pub fn run(
    provider: &dyn Provider,
    workspace: &mut Workspace,
    summary: &BranchSummary,
    extra_instructions: Option<&str>,
    max_diff_chars: usize,
    on_event: &mut dyn FnMut(Event),
) -> Result<CommitSuggestion, String> {
    if summary.is_empty() {
        return Err(
            "Nothing to describe: this branch has no commits the base does not.".into()
        );
    }
    let system = format!(
        "{}{CONTEXT_PROMPT}",
        crate::ollama::pr_system_prompt(extra_instructions)
    );
    let prompt = crate::ollama::pr_prompt(summary, max_diff_chars);
    let run = super::run(provider, workspace, &system, &prompt, limits(), on_event)?;
    let suggestion = crate::ollama::parse_suggestion_text(&run.text);
    if suggestion.summary.trim().is_empty() {
        return Err(format!(
            "The model did not return a usable title: {}",
            crate::review::excerpt(&run.text)
        ));
    }
    Ok(suggestion)
}

/// Whether this task should have read access at all: a workspace that cannot
/// read is no use to it.
pub fn access() -> Access {
    Access::ReadOnly
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Message, Reply, ToolCall, ToolSpec};
    use std::cell::RefCell;

    struct Scripted {
        replies: RefCell<Vec<Reply>>,
        systems: RefCell<Vec<String>>,
    }

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
            self.systems.borrow_mut().push(system.to_string());
            Ok(self.replies.borrow_mut().remove(0))
        }
    }

    fn fixture() -> (tempfile::TempDir, Workspace, BranchSummary) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("lib.rs"), "pub fn halve(n: u32) -> u32 { n / 2 }\n")
            .unwrap();
        let ws =
            Workspace::new(tmp.path(), vec!["lib.rs".into()], Access::ReadOnly).unwrap();
        let summary = BranchSummary {
            branch: "feat/halve".into(),
            base: "main".into(),
            commits: vec![crate::git::Commit {
                sha: "0".repeat(40),
                short_sha: "0000000".into(),
                author: "T".into(),
                email: "t@t.io".into(),
                date: "2026-01-01T00:00:00Z".into(),
                subject: "feat: add halve()".into(),
                body: String::new(),
                parents: Vec::new(),
                refs: Vec::new(),
            }],
            stat: " lib.rs | 1 +".into(),
            diff: "diff --git a/lib.rs b/lib.rs\n+pub fn halve(n: u32) -> u32 { n / 2 }\n"
                .into(),
        };
        (tmp, ws, summary)
    }

    #[test]
    fn it_can_read_the_repository_while_it_writes() {
        let (_tmp, mut ws, summary) = fixture();
        let provider = Scripted {
            replies: RefCell::new(vec![
                Reply {
                    text: String::new(),
                    calls: vec![ToolCall {
                        id: "1".into(),
                        name: "read_file".into(),
                        input: serde_json::json!({"path": "lib.rs"}),
                    }],
                },
                Reply {
                    text: r#"{"summary": "Add halve()", "description": "Halves a number."}"#
                        .into(),
                    calls: vec![],
                },
            ]),
            systems: RefCell::new(Vec::new()),
        };

        let text = run(&provider, &mut ws, &summary, None, 10_000, &mut |_| {}).unwrap();
        assert_eq!(text.summary, "Add halve()");
        // The prompt has to offer the tools *and* keep the PR contract.
        let system = &provider.systems.borrow()[0];
        assert!(system.contains("pull request"));
        assert!(system.contains("list_files, read_file, search"));
    }

    #[test]
    fn an_empty_branch_is_refused_before_a_model_is_called() {
        let (_tmp, mut ws, _) = fixture();
        let provider = Scripted {
            replies: RefCell::new(Vec::new()),
            systems: RefCell::new(Vec::new()),
        };
        let err = run(
            &provider,
            &mut ws,
            &BranchSummary::default(),
            None,
            10_000,
            &mut |_| {},
        )
        .unwrap_err();
        assert!(err.contains("Nothing to describe"));
    }

    /// A model that answers in plain text instead of JSON still produces
    /// something usable: the first line becomes the title and the rest the
    /// body, which the user edits in the form before anything is created.
    #[test]
    fn a_plain_text_reply_still_fills_the_form() {
        let (_tmp, mut ws, summary) = fixture();
        let provider = Scripted {
            replies: RefCell::new(vec![Reply {
                text: "Add halve()\n\nHalves a number, with a test.".into(),
                calls: vec![],
            }]),
            systems: RefCell::new(Vec::new()),
        };
        let text = run(&provider, &mut ws, &summary, None, 10_000, &mut |_| {}).unwrap();
        assert_eq!(text.summary, "Add halve()");
        assert!(text.description.contains("Halves a number"));
    }

    /// An empty answer is an error rather than an empty pull request.
    #[test]
    fn an_empty_reply_is_an_error() {
        let (_tmp, mut ws, summary) = fixture();
        let provider = Scripted {
            replies: RefCell::new(vec![Reply { text: "   ".into(), calls: vec![] }]),
            systems: RefCell::new(Vec::new()),
        };
        let err = run(&provider, &mut ws, &summary, None, 10_000, &mut |_| {}).unwrap_err();
        assert!(err.to_lowercase().contains("empty"), "{err}");
    }
}
