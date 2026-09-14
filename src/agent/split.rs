//! Splitting a working tree that holds several unrelated changes into the
//! commits it should have been.
//!
//! The model groups the changed files and writes a message for each group.
//! What it proposes is checked against the actual set of changed files
//! before anything is staged: every changed file exactly once, nothing
//! invented. A split that quietly leaves a file out of every group would
//! leave it uncommitted with nobody the wiser.

use super::{Access, Event, Limits, Provider, Workspace};

const SYSTEM_PROMPT: &str = r#"You are splitting a working tree into separate commits.

You are given the changed files and the diff. Group the files into the commits they should have been, and write each commit's message.

What to do:
- Put files that change together for one reason in one commit. A feature and its test belong together; a feature and an unrelated typo fix do not.
- Order the groups so each one would build on the last: a refactor before the feature that uses it, a helper before its caller.
- Write a conventional-commit style summary under 72 characters, imperative mood, plus a body where it explains something the summary cannot.
- If every change really is one thing, say so by returning a single group.

Hard rules:
- Use every file you were given, exactly once. Do not drop one, repeat one, or invent a path. This is checked.
- Group by file. You cannot split a single file across two commits.

Answer with JSON only:
{"commits": [{"files": ["src/a.rs", "src/a_test.rs"], "summary": "feat: add a()",
              "description": "Why, when the summary cannot say it. May be empty."}],
 "notes": "one sentence for the developer"}"#;

/// One proposed commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub files: Vec<String>,
    pub summary: String,
    pub description: String,
}

impl Group {
    pub fn message(&self) -> String {
        if self.description.trim().is_empty() {
            self.summary.trim().to_string()
        } else {
            format!("{}\n\n{}", self.summary.trim(), self.description.trim())
        }
    }
}

/// A proposed split, plus what the model says it did.
#[derive(Debug, Clone)]
pub struct Proposal {
    pub groups: Vec<Group>,
    pub notes: String,
}

pub fn limits() -> Limits {
    Limits { max_turns: 10, max_tool_calls: 16, max_read_bytes: 150_000, max_tokens: 4096, max_transcript_bytes: 400_000 }
}

/// Checks a proposal covers exactly the changed files.
fn check(groups: &[Group], changed: &[String]) -> Result<(), String> {
    let mut seen: Vec<&str> = Vec::new();
    for group in groups {
        if group.summary.trim().is_empty() {
            return Err("a commit has no message".into());
        }
        for file in &group.files {
            if seen.contains(&file.as_str()) {
                return Err(format!("{file} is in two commits"));
            }
            if !changed.iter().any(|c| c == file) {
                return Err(format!("{file} is not one of the changed files"));
            }
            seen.push(file);
        }
    }
    let missing: Vec<&String> =
        changed.iter().filter(|c| !seen.contains(&c.as_str())).collect();
    if !missing.is_empty() {
        return Err(format!(
            "{} changed file(s) are in no commit, including {}",
            missing.len(),
            missing[0]
        ));
    }
    Ok(())
}

/// Asks for a split, validating it and retrying once with the reason.
pub fn run(
    provider: &dyn Provider,
    workspace: &mut Workspace,
    changed: &[String],
    diff: &str,
    extra_instructions: Option<&str>,
    on_event: &mut dyn FnMut(Event),
) -> Result<Proposal, String> {
    if changed.len() < 2 {
        return Err("There is nothing to split: only one file changed.".into());
    }
    let system = match extra_instructions.map(str::trim).filter(|s| !s.is_empty()) {
        Some(extra) => format!("{SYSTEM_PROMPT}\n\nAdditional instructions:\n{extra}"),
        None => SYSTEM_PROMPT.to_string(),
    };

    let mut task = format!(
        "{} changed file(s):\n{}\n\nThe diff:\n\n```diff\n{}\n```\n\n\
         Group them into the commits they should have been.",
        changed.len(),
        changed.iter().map(|f| format!("- {f}")).collect::<Vec<_>>().join("\n"),
        crate::ollama::truncate_for_prompt(diff, 40_000)
    );

    let mut last_error = String::new();
    for attempt in 0..2 {
        if attempt > 0 {
            task = format!(
                "{task}\n\nYour previous grouping was rejected: {last_error}\nUse every \
                 file exactly once. Try again."
            );
        }
        let run = super::run(provider, workspace, &system, &task, limits(), on_event)?;
        let Some((groups, notes)) = parse(&run.text) else {
            last_error = "the answer was not a usable grouping".into();
            continue;
        };
        match check(&groups, changed) {
            Ok(()) => return Ok(Proposal { groups, notes }),
            Err(e) => last_error = e,
        }
    }
    Err(format!("The model could not produce a valid split: {last_error}"))
}

fn parse(text: &str) -> Option<(Vec<Group>, String)> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    let value: serde_json::Value = serde_json::from_str(&text[start..=end]).ok()?;
    let groups: Vec<Group> = value
        .get("commits")?
        .as_array()?
        .iter()
        .filter_map(|item| {
            Some(Group {
                files: item
                    .get("files")?
                    .as_array()?
                    .iter()
                    .filter_map(|f| f.as_str().map(str::to_string))
                    .collect(),
                summary: item.get("summary")?.as_str()?.trim().to_string(),
                description: item
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string(),
            })
        })
        .collect();
    if groups.is_empty() {
        return None;
    }
    let notes = value
        .get("notes")
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    Some((groups, notes))
}

/// This task only reads; the commits it proposes are made by the app after
/// the user accepts them.
pub fn access() -> Access {
    Access::ReadOnly
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Message, Reply, ToolSpec};
    use std::cell::RefCell;

    struct Scripted(RefCell<Vec<String>>);
    impl Provider for Scripted {
        fn label(&self) -> String {
            "scripted".into()
        }
        fn turn(&self, _: &str, _: &[Message], _: &[ToolSpec], _: u32) -> Result<Reply, String> {
            Ok(Reply { text: self.0.borrow_mut().remove(0), calls: vec![], ..Default::default() })
        }
    }

    fn workspace(tmp: &tempfile::TempDir) -> Workspace {
        Workspace::new(tmp.path(), Vec::new(), Access::ReadOnly).unwrap()
    }

    fn changed() -> Vec<String> {
        vec!["src/feature.rs".into(), "tests/feature.rs".into(), "README.md".into()]
    }

    #[test]
    fn a_valid_split_comes_back_parsed() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ws = workspace(&tmp);
        let answer = r#"{"commits": [
            {"files": ["src/feature.rs", "tests/feature.rs"], "summary": "feat: add the feature",
             "description": "with a test"},
            {"files": ["README.md"], "summary": "docs: mention the feature"}],
          "notes": "two unrelated changes"}"#;
        let provider = Scripted(RefCell::new(vec![answer.into()]));

        let proposal = run(&provider, &mut ws, &changed(), "d", None, &mut |_| {}).unwrap();
        assert_eq!(proposal.groups.len(), 2);
        assert_eq!(proposal.groups[0].files.len(), 2);
        assert_eq!(proposal.groups[1].summary, "docs: mention the feature");
        assert_eq!(
            proposal.groups[0].message(),
            "feat: add the feature\n\nwith a test"
        );
        assert_eq!(proposal.notes, "two unrelated changes");
    }

    #[test]
    fn a_split_that_forgets_a_file_is_retried_then_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ws = workspace(&tmp);
        let forgets = r#"{"commits": [{"files": ["src/feature.rs"], "summary": "s"}]}"#;
        let provider = Scripted(RefCell::new(vec![forgets.into(), forgets.into()]));
        let err = run(&provider, &mut ws, &changed(), "d", None, &mut |_| {}).unwrap_err();
        assert!(err.contains("are in no commit"), "{err}");

        // The retry is what makes a first bad answer survivable.
        let good = r#"{"commits": [{"files": ["src/feature.rs", "tests/feature.rs", "README.md"],
                                    "summary": "chore: everything"}]}"#;
        let provider = Scripted(RefCell::new(vec![forgets.into(), good.into()]));
        let proposal = run(&provider, &mut ws, &changed(), "d", None, &mut |_| {}).unwrap();
        assert_eq!(proposal.groups.len(), 1);
    }

    #[test]
    fn invented_and_duplicated_files_are_refused() {
        assert!(check(
            &[Group {
                files: vec!["nope.rs".into()],
                summary: "s".into(),
                description: String::new()
            }],
            &changed()
        )
        .unwrap_err()
        .contains("not one of the changed files"));

        let twice = vec![
            Group {
                files: vec!["README.md".into()],
                summary: "a".into(),
                description: String::new(),
            },
            Group {
                files: vec!["README.md".into()],
                summary: "b".into(),
                description: String::new(),
            },
        ];
        assert!(check(&twice, &changed()).unwrap_err().contains("in two commits"));
    }

    #[test]
    fn one_changed_file_needs_no_split() {
        let tmp = tempfile::tempdir().unwrap();
        let mut ws = workspace(&tmp);
        let provider = Scripted(RefCell::new(Vec::new()));
        let err = run(&provider, &mut ws, &["only.rs".into()], "d", None, &mut |_| {})
            .unwrap_err();
        assert!(err.contains("nothing to split"), "{err}");
    }
}
