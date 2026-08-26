//! The conflict-resolution task: prompts and entry point.
//!
//! The single-shot resolver ([`crate::ollama::merge_prompt`]) hands the model
//! three versions of one file and takes back a merged file. It cannot see
//! anything else, which is exactly the information a hard conflict needs:
//! two branches renamed the same function differently, or one side added a
//! caller the other side's signature no longer fits. Resolving that requires
//! reading the rest of the repository, and sometimes editing more than the
//! conflicted file.
//!
//! So this task runs the harness with a [`Access::ReadWrite`] workspace over
//! the whole worktree. Every proposed write is held in the overlay and
//! returned as [`crate::agent::PendingEdit`]s for the user to accept or
//! reject file by file — the model's reach is proposals, never writes.

use super::{Access, Event, Limits, Provider, Run, Workspace};

/// One conflicted file, as the app knows it.
#[derive(Debug, Clone)]
pub struct Brief {
    /// Repo-relative path.
    pub path: String,
    /// The current branch's version.
    pub ours: Option<String>,
    /// The incoming version.
    pub theirs: Option<String>,
}

const SYSTEM_PROMPT: &str = r#"You are resolving git merge conflicts in a repository you can read and edit.

Tools: list_files, read_file, search read every file git tracks. write_file and edit_file propose changes to any file in the worktree.

Nothing you propose is written to disk. The user is shown every proposed change as a diff and accepts or rejects each one, so propose the resolution you believe is correct and explain it — do not hedge by leaving conflict markers or commented-out alternatives for the user to sort out.

How to resolve:
- Read each conflicted file first. Its working copy on disk still contains the <<<<<<< ======= >>>>>>> markers, showing both sides.
- Understand what each side was trying to do before choosing. Read the surrounding code, the definitions the conflicting lines call, and the callers of anything whose signature or name is in conflict.
- Keep the intent of BOTH sides wherever they do not contradict. Integrate them where they touch the same lines. Only drop one side when keeping both is genuinely impossible, and say so in your summary when you do.
- Remove every conflict marker. A file you propose must be complete, valid, and free of markers.
- When the merge requires changes outside the conflicted files — a caller that must be updated, an import that must move, a test that names the renamed symbol — propose those too. That is why you have access to the whole worktree.
- Do not reformat, refactor, or "improve" code the conflict did not touch. Keep the diff to what the merge requires.
- If a file is too tangled to resolve confidently, leave it alone and say why. An honest "resolve this one by hand" is worth more than a plausible wrong merge that the user has to discover later.

When you are done, reply with a short Markdown summary: one bullet per file you changed, saying what you kept from each side and why, then a line naming anything you deliberately left for the user."#;

/// The system prompt with the repository's own conflict instructions
/// appended. Custom text extends the built-in rules rather than replacing
/// them, the same way the single-shot resolver treats them.
pub fn system_prompt(extra_instructions: Option<&str>) -> String {
    match extra_instructions.map(str::trim).filter(|s| !s.is_empty()) {
        Some(extra) => format!("{SYSTEM_PROMPT}\n\nAdditional instructions:\n{extra}"),
        None => SYSTEM_PROMPT.to_string(),
    }
}

/// How much of the conflicting versions to paste into the opening turn.
/// Enough that a small conflict needs no tool call at all; past it the model
/// reads the marked-up working copies itself.
pub const INLINE_BUDGET: usize = 24_000;

/// The opening turn: which files are conflicted, plus the two sides inlined
/// while they fit.
pub fn task_prompt(files: &[Brief], inline_budget: usize) -> String {
    let mut p = String::from(
        "Resolve the merge conflicts in this repository.\n\nConflicted files:\n",
    );
    for file in files {
        p.push_str(&format!("- {}\n", file.path));
    }

    let total: usize = files
        .iter()
        .map(|f| {
            f.ours.as_deref().unwrap_or("").len() + f.theirs.as_deref().unwrap_or("").len()
        })
        .sum();

    if total <= inline_budget {
        p.push_str("\nThe two sides of each file follow. The working copy on disk also \
                    still has the conflict markers if you want to see them in place.\n");
        for file in files {
            p.push_str(&format!(
                "\n## {}\n\nOURS (current branch):\n```\n{}\n```\n\nTHEIRS (incoming):\n```\n{}\n```\n",
                file.path,
                file.ours.as_deref().unwrap_or("[deleted on this branch]"),
                file.theirs.as_deref().unwrap_or("[deleted on the incoming branch]"),
            ));
        }
    } else {
        p.push_str(&format!(
            "\nThe conflicting versions are too large to paste here ({total} characters). \
             Read each file with read_file: the working copy still contains the \
             <<<<<<< ======= >>>>>>> markers, so both sides are visible in place.\n"
        ));
    }

    p.push_str(
        "\nPropose the resolution for each file, plus any other file the merge requires \
         changing. Then summarize what you did.",
    );
    p
}

/// Runs the conflict resolver over `files`.
///
/// `workspace` must be [`Access::ReadWrite`]; the returned [`Run::edits`] are
/// proposals awaiting the user's confirmation, and nothing has been written.
pub fn run(
    provider: &dyn Provider,
    workspace: &mut Workspace,
    files: &[Brief],
    extra_instructions: Option<&str>,
    limits: Limits,
    on_event: &mut dyn FnMut(Event),
) -> Result<Run, String> {
    if files.is_empty() {
        return Err("No conflicted files to resolve.".into());
    }
    let system = system_prompt(extra_instructions);
    let task = task_prompt(files, INLINE_BUDGET);
    super::run(provider, workspace, &system, &task, limits, on_event)
}

/// Budgets for a conflict run. Resolving takes more reading and more turns
/// than reviewing does, so these sit above [`Limits::default`].
pub fn limits() -> Limits {
    Limits { max_turns: 16, max_tool_calls: 48, ..Limits::default() }
}

/// Whether a resolution still contains conflict markers, which means it is
/// not resolved whatever the model said about it. Checked before an edit is
/// offered to the user, so a half-done merge cannot be accepted by mistake.
pub fn has_conflict_markers(content: &str) -> bool {
    /// Git writes exactly seven marker characters, optionally followed by a
    /// space and a label. Requiring the count to be exact keeps a row of
    /// eight `=` used as a text divider from reading as a conflict, and
    /// requiring the start of the line keeps prose *about* markers out.
    fn marker(line: &str, glyph: char) -> bool {
        let run = line.chars().take_while(|c| *c == glyph).count();
        run == 7 && line[run..].chars().next().is_none_or(|c| c == ' ')
    }
    content.lines().any(|line| {
        marker(line, '<')
            || marker(line, '=')
            || marker(line, '>')
            // The base section, present only under merge.conflictStyle=diff3.
            || marker(line, '|')
    })
}

/// Sanity check for one workspace: it must be able to write.
pub fn require_write_access(access: Access) -> Result<(), String> {
    match access {
        Access::ReadWrite => Ok(()),
        Access::ReadOnly => {
            Err("The conflict resolver needs write access to propose changes.".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn brief(path: &str, ours: &str, theirs: &str) -> Brief {
        Brief {
            path: path.into(),
            ours: Some(ours.into()),
            theirs: Some(theirs.into()),
        }
    }

    #[test]
    fn inlines_small_conflicts() {
        let files = vec![brief("a.rs", "let x = 1;", "let x = 2;")];
        let p = task_prompt(&files, INLINE_BUDGET);
        assert!(p.contains("- a.rs"));
        assert!(p.contains("let x = 1;"));
        assert!(p.contains("let x = 2;"));
    }

    #[test]
    fn points_at_the_working_copy_when_too_large() {
        let files = vec![brief("a.rs", &"x".repeat(200), &"y".repeat(200))];
        let p = task_prompt(&files, 100);
        assert!(p.contains("too large to paste"));
        assert!(!p.contains(&"x".repeat(200)));
        assert!(p.contains("read_file"));
    }

    #[test]
    fn notes_a_side_that_deleted_the_file() {
        let files = vec![Brief { path: "a.rs".into(), ours: Some("x".into()), theirs: None }];
        let p = task_prompt(&files, INLINE_BUDGET);
        assert!(p.contains("[deleted on the incoming branch]"));
    }

    #[test]
    fn custom_instructions_extend_the_prompt() {
        let p = system_prompt(Some("Always keep our license header."));
        assert!(p.starts_with("You are resolving git merge conflicts"));
        assert!(p.contains("Always keep our license header."));
        assert_eq!(system_prompt(Some("   ")), SYSTEM_PROMPT);
    }

    #[test]
    fn detects_leftover_markers() {
        assert!(has_conflict_markers("a\n<<<<<<< HEAD\nb\n=======\nc\n>>>>>>> other\n"));
        assert!(!has_conflict_markers("a\nb\nc\n"));
        // A line that merely mentions the marker text is not a marker.
        assert!(!has_conflict_markers("// the <<<<<<< marker means ours\n"));
    }

    #[test]
    fn marker_detection_covers_the_shapes_git_actually_writes() {
        // Labelled, which is the usual case.
        assert!(has_conflict_markers("a\n<<<<<<< HEAD\nb\n=======\nc\n>>>>>>> other\n"));
        // Bare, with no label after the marker.
        assert!(has_conflict_markers("<<<<<<<\nours\n=======\ntheirs\n>>>>>>>\n"));
        // diff3 style, which adds a base section.
        assert!(has_conflict_markers("<<<<<<< ours\na\n||||||| base\nb\n=======\nc\n>>>>>>> theirs\n"));

        assert!(!has_conflict_markers("a\nb\nc\n"));
        // Prose about markers is not a marker.
        assert!(!has_conflict_markers("// the <<<<<<< marker means ours\n"));
        // A divider is not a marker: git writes exactly seven.
        assert!(!has_conflict_markers("========\nA heading underline\n"));
        assert!(!has_conflict_markers("<<<<<<<<<<\n"));
        // Seven, but with something other than a space after them.
        assert!(!has_conflict_markers("=======no\n"));
    }

    #[test]
    fn empty_input_is_refused() {
        struct Never;
        impl Provider for Never {
            fn label(&self) -> String {
                "never".into()
            }
            fn turn(
                &self,
                _: &str,
                _: &[super::super::Message],
                _: &[super::super::ToolSpec],
                _: u32,
            ) -> Result<super::super::Reply, String> {
                panic!("must not be called")
            }
        }
        let tmp = tempfile::tempdir().unwrap();
        let mut ws = Workspace::new(tmp.path(), Vec::new(), Access::ReadWrite).unwrap();
        let err = run(&Never, &mut ws, &[], None, limits(), &mut |_| {}).unwrap_err();
        assert!(err.contains("No conflicted files"));
    }
}
