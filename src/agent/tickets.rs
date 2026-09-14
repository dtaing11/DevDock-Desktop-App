//! Turning a list of work into Jira tickets.
//!
//! The input is a list someone already has: a planning document's bullets, a
//! stand-up's actions, the findings of a review. The model reads the
//! repository while it drafts, so a ticket can name the file and the function
//! rather than restating the bullet in more words — which is the only reason
//! to have a model do this at all.
//!
//! # What is checked
//!
//! The list is numbered, and each draft says which items it covers. Every
//! item has to be covered by at least one ticket, and no draft may cite an
//! item that does not exist. A drafting pass that quietly dropped the third
//! bullet would leave a hole in a sprint with nobody the wiser — the same
//! failure the commit splitter guards against, for the same reason.
//!
//! Nothing is created here. [`run`] returns drafts; creating them is a
//! separate, confirmed step.

use super::{Event, Limits, Provider, Workspace};

const SYSTEM_PROMPT: &str = r#"You are writing Jira tickets from a list of work, for a developer who will read them in a sprint.

You have read-only access to the repository. Use it. A ticket that names the file, the function, and what is currently there is worth ten that restate the bullet in more words. Read before you write.

For each ticket:
- Summary: what to do, imperative, under 100 characters, no ticket-speak. "Add a --json flag to devdock status", not "Implementation of JSON output capability".
- Description: why it is needed, what currently exists (with the paths you found), and what done looks like. Markdown. Keep it to what a developer needs to start; do not pad it with headings that say nothing.
- Type: one of the types you are given, by name.
- Labels: at most three, lower case, single words or hyphenated.

Rules about coverage:
- Every numbered item must be covered by at least one ticket. This is checked.
- One ticket may cover several items when they are one piece of work; say so with several numbers.
- One item may become several tickets when it is plainly several pieces of work. Prefer not to: a list of twenty tickets from five bullets is not help.
- Do not invent work that is not on the list.

Answer with JSON only:
{"tickets": [{"items": [1], "type": "Task", "summary": "Add a --json flag to devdock status",
              "description": "…markdown…", "labels": ["cli"]}],
 "notes": "one sentence for the developer, or empty"}"#;

/// One drafted ticket, before anyone has agreed to create it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub summary: String,
    pub description: String,
    /// Issue type by name, as the project spells it.
    pub issue_type: String,
    pub labels: Vec<String>,
    /// The list items this covers, in the words they were written in, so the
    /// user can see what became what.
    pub source: Vec<String>,
}

impl Draft {
    /// The issue this would create in `project`.
    pub fn to_issue(&self, project_key: &str) -> crate::jira::NewIssue {
        crate::jira::NewIssue {
            project_key: project_key.to_string(),
            issue_type: self.issue_type.clone(),
            summary: self.summary.clone(),
            description: self.description.clone(),
            labels: self.labels.clone(),
            parent: None,
        }
    }
}

/// A drafted set of tickets, plus what the model says it did.
#[derive(Debug, Clone, Default)]
pub struct Proposal {
    pub drafts: Vec<Draft>,
    pub notes: String,
}

/// The budget for drafting `items` tickets.
///
/// Per item, not per run. A fixed pool is spent on the first few and the rest
/// are written from the list alone — which is the version of this feature
/// that is not worth having, since a ticket that could have been written
/// without reading the repository did not need a model with the repository
/// open to it.
pub fn limits(items: usize) -> Limits {
    Limits {
        max_turns: (4 + 3 * items).clamp(10, 40),
        max_tool_calls: (8 * items).clamp(24, 120),
        max_read_bytes: 200_000,
        max_tokens: 8192,
        max_transcript_bytes: 400_000,
    }
}

/// Jira's own cap on a summary.
const MAX_SUMMARY: usize = 255;

/// Splits a pasted list into the items it contains.
///
/// One item per line, with the bullet or number taken off. Blank lines are
/// separators, not items, and a line that is only a bullet is not an item —
/// people leave those behind when they edit a list.
pub fn parse_list(text: &str) -> Vec<String> {
    text.lines()
        .map(|line| {
            let line = line.trim();
            for bullet in ["- [ ] ", "- [x] ", "- ", "* ", "+ ", "• "] {
                if let Some(rest) = line.strip_prefix(bullet) {
                    return rest.trim();
                }
            }
            let digits = line.chars().take_while(char::is_ascii_digit).count();
            if (1..=3).contains(&digits) {
                for sep in [". ", ") ", "- "] {
                    if let Some(rest) = line[digits..].strip_prefix(sep) {
                        return rest.trim();
                    }
                }
            }
            line
        })
        .filter(|item| !item.is_empty() && item.chars().any(char::is_alphanumeric))
        .map(str::to_string)
        .collect()
}

/// Checks a proposal covers the list and names types that exist.
///
/// `types` is what the project accepts; an empty list means the caller does
/// not know yet, and any name passes.
fn check(drafts: &[Draft], covered: &[Vec<usize>], items: usize, types: &[String]) -> Result<(), String> {
    if drafts.is_empty() {
        return Err("no tickets were drafted".into());
    }
    for draft in drafts {
        if draft.summary.trim().is_empty() {
            return Err("a ticket has no summary".into());
        }
        if draft.summary.chars().count() > MAX_SUMMARY {
            return Err(format!(
                "the summary \"{}…\" is longer than Jira allows ({MAX_SUMMARY})",
                draft.summary.chars().take(40).collect::<String>()
            ));
        }
        if !types.is_empty()
            && !types.iter().any(|t| t.eq_ignore_ascii_case(draft.issue_type.trim()))
        {
            return Err(format!(
                "\"{}\" is not an issue type in this project; use one of: {}",
                draft.issue_type,
                types.join(", ")
            ));
        }
    }
    let mut seen = vec![false; items];
    for indices in covered {
        for &i in indices {
            match seen.get_mut(i) {
                Some(slot) => *slot = true,
                None => return Err(format!("item {} is not on the list", i + 1)),
            }
        }
    }
    let missing: Vec<String> =
        seen.iter().enumerate().filter(|(_, s)| !**s).map(|(i, _)| (i + 1).to_string()).collect();
    if !missing.is_empty() {
        return Err(format!("item(s) {} are in no ticket", missing.join(", ")));
    }
    Ok(())
}

/// Drafts tickets for `list`, validating the result and retrying once with
/// the reason it was rejected.
pub fn run(
    provider: &dyn Provider,
    workspace: &mut Workspace,
    list: &str,
    types: &[String],
    extra_instructions: Option<&str>,
    on_event: &mut dyn FnMut(Event),
) -> Result<Proposal, String> {
    let items = parse_list(list);
    if items.is_empty() {
        return Err("There is nothing in the list to write tickets for.".into());
    }

    let system = match extra_instructions.map(str::trim).filter(|s| !s.is_empty()) {
        Some(extra) => format!("{SYSTEM_PROMPT}\n\nAdditional instructions:\n{extra}"),
        None => SYSTEM_PROMPT.to_string(),
    };
    let numbered: String = items
        .iter()
        .enumerate()
        .map(|(i, item)| format!("{}. {item}", i + 1))
        .collect::<Vec<_>>()
        .join("\n");
    let type_line = if types.is_empty() {
        "Use the type \"Task\" unless the item is plainly a bug.".to_string()
    } else {
        format!("Issue types in this project: {}.", types.join(", "))
    };
    let mut task = format!(
        "{} item(s) to write tickets for:\n\n{numbered}\n\n{type_line}\n\n\
         Read the repository for whatever you need to make each ticket specific.",
        items.len()
    );

    let mut last_error = String::new();
    for attempt in 0..2 {
        if attempt > 0 {
            task = format!(
                "{task}\n\nYour previous draft was rejected: {last_error}\nCover every \
                 numbered item and try again."
            );
        }
        let run =
            super::run(provider, workspace, &system, &task, limits(items.len()), on_event)?;
        let Some((drafts, covered, notes)) = parse(&run.text, &items) else {
            last_error = "the answer was not a usable set of tickets".into();
            continue;
        };
        match check(&drafts, &covered, items.len(), types) {
            Ok(()) => return Ok(Proposal { drafts, notes }),
            Err(e) => last_error = e,
        }
    }
    Err(format!("The model could not draft usable tickets: {last_error}"))
}

/// Parses the reply into drafts, the items each covers, and the notes.
fn parse(text: &str, items: &[String]) -> Option<(Vec<Draft>, Vec<Vec<usize>>, String)> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    let value: serde_json::Value = serde_json::from_str(&text[start..=end]).ok()?;
    let tickets = value.get("tickets")?.as_array()?;

    let mut drafts = Vec::new();
    let mut covered = Vec::new();
    for ticket in tickets {
        // One-based on the wire, because that is how the list was shown.
        let indices: Vec<usize> = ticket
            .get("items")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|n| n.as_u64())
                    .filter(|n| *n >= 1)
                    .map(|n| n as usize - 1)
                    .collect()
            })
            .unwrap_or_default();
        let source = indices.iter().filter_map(|i| items.get(*i).cloned()).collect();
        drafts.push(Draft {
            summary: string_at(ticket, "summary").trim().to_string(),
            description: string_at(ticket, "description").trim().to_string(),
            issue_type: {
                let t = string_at(ticket, "type");
                if t.trim().is_empty() { "Task".to_string() } else { t.trim().to_string() }
            },
            labels: ticket
                .get("labels")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|l| l.as_str())
                        .map(|l| l.trim().to_lowercase())
                        .filter(|l| !l.is_empty())
                        .take(3)
                        .collect()
                })
                .unwrap_or_default(),
            source,
        });
        covered.push(indices);
    }
    let notes = string_at(&value, "notes").trim().to_string();
    Some((drafts, covered, notes))
}

fn string_at(value: &serde_json::Value, key: &str) -> String {
    value.get(key).and_then(|v| v.as_str()).unwrap_or_default().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_budget_grows_with_the_list() {
        // A ten-item list cannot be drafted on the budget a two-item list
        // needs, and the failure is silent: the later tickets get written
        // from the bullet alone.
        let small = limits(2);
        let large = limits(10);
        assert!(large.max_tool_calls > small.max_tool_calls, "{small:?} {large:?}");
        assert!(large.max_turns > small.max_turns);
        // And it is bounded, so a hundred-item list cannot spend forever.
        let huge = limits(1000);
        assert!(huge.max_tool_calls <= 120 && huge.max_turns <= 40, "{huge:?}");
        // A one-item list still gets enough to read something.
        assert!(limits(1).max_tool_calls >= 24);
    }

    #[test]
    fn a_list_is_read_however_it_was_written() {
        let list = "\
- Add a --json flag
* Fix the crash on empty repos
+ Document the CI config
1. Numbered items too
2) And this spelling
- [ ] An unticked checkbox
- [x] A ticked one
• A bullet someone pasted from a document
Just a bare line

-
   
Indented but real";
        let items = parse_list(list);
        assert_eq!(
            items,
            [
                "Add a --json flag",
                "Fix the crash on empty repos",
                "Document the CI config",
                "Numbered items too",
                "And this spelling",
                "An unticked checkbox",
                "A ticked one",
                "A bullet someone pasted from a document",
                "Just a bare line",
                "Indented but real",
            ],
            "a lone bullet and a blank line are not items"
        );
        assert!(parse_list("").is_empty());
        assert!(parse_list("- \n\n  \n-").is_empty());
    }

    fn draft(summary: &str, issue_type: &str) -> Draft {
        Draft {
            summary: summary.into(),
            description: "why".into(),
            issue_type: issue_type.into(),
            labels: vec![],
            source: vec![],
        }
    }

    #[test]
    fn every_item_has_to_end_up_in_a_ticket() {
        let drafts = vec![draft("one", "Task")];
        // Three items, one ticket covering the first: the other two are lost.
        let err = check(&drafts, &[vec![0]], 3, &[]).unwrap_err();
        assert!(err.contains("2, 3"), "{err}");

        // Covered between two tickets is fine.
        let drafts = vec![draft("one", "Task"), draft("two", "Task")];
        check(&drafts, &[vec![0, 2], vec![1]], 3, &[]).unwrap();
        // And one item may become two tickets.
        check(&drafts, &[vec![0], vec![0]], 1, &[]).unwrap();
    }

    #[test]
    fn a_ticket_cannot_cite_an_item_that_does_not_exist() {
        let err = check(&[draft("one", "Task")], &[vec![0, 7]], 2, &[]).unwrap_err();
        assert!(err.contains("item 8"), "{err}");
    }

    #[test]
    fn a_type_the_project_does_not_have_is_rejected_with_the_ones_it_does() {
        let types = vec!["Task".to_string(), "Bug".to_string()];
        check(&[draft("one", "Task")], &[vec![0]], 1, &types).unwrap();
        // Case is the model's business, not the user's.
        check(&[draft("one", "task")], &[vec![0]], 1, &types).unwrap();

        let err = check(&[draft("one", "Epic")], &[vec![0]], 1, &types).unwrap_err();
        assert!(err.contains("Epic"), "{err}");
        assert!(err.contains("Task, Bug"), "the message must offer the real ones: {err}");
    }

    #[test]
    fn a_summary_longer_than_jira_allows_is_rejected() {
        let long = "x".repeat(MAX_SUMMARY + 1);
        let err = check(&[draft(&long, "Task")], &[vec![0]], 1, &[]).unwrap_err();
        assert!(err.contains("longer than Jira allows"), "{err}");
        // The limit itself is fine.
        check(&[draft(&"x".repeat(MAX_SUMMARY), "Task")], &[vec![0]], 1, &[]).unwrap();
    }

    #[test]
    fn an_empty_summary_is_not_a_ticket() {
        assert!(check(&[draft("   ", "Task")], &[vec![0]], 1, &[]).is_err());
        assert!(check(&[], &[], 1, &[]).is_err());
    }

    #[test]
    fn a_reply_becomes_drafts_carrying_the_words_they_came_from() {
        let items = vec!["Add a --json flag".to_string(), "Fix the crash".to_string()];
        let reply = r#"Here you go:
        {"tickets": [
           {"items": [1], "type": "Task", "summary": "Add a --json flag to status",
            "description": "Scripts need it.", "labels": ["CLI", " Output ", "", "a", "b"]},
           {"items": [2], "type": "Bug", "summary": "Fix the crash on an empty repository",
            "description": "It unwraps.", "labels": []}],
         "notes": "the second one is the urgent one"}"#;
        let (drafts, covered, notes) = parse(reply, &items).unwrap();

        assert_eq!(drafts.len(), 2);
        assert_eq!(drafts[0].summary, "Add a --json flag to status");
        assert_eq!(drafts[0].issue_type, "Task");
        // Labels are lower-cased, trimmed, emptied out, and capped at three.
        assert_eq!(drafts[0].labels, ["cli", "output", "a"]);
        // The item's own words travel with the draft, so the user can see
        // what became what without counting indices.
        assert_eq!(drafts[0].source, ["Add a --json flag"]);
        assert_eq!(drafts[1].source, ["Fix the crash"]);
        assert_eq!(covered, [vec![0], vec![1]]);
        assert_eq!(notes, "the second one is the urgent one");
    }

    #[test]
    fn a_ticket_with_no_type_defaults_rather_than_failing() {
        let items = vec!["one".to_string()];
        let reply = r#"{"tickets":[{"items":[1],"summary":"Do the thing"}],"notes":""}"#;
        let (drafts, ..) = parse(reply, &items).unwrap();
        assert_eq!(drafts[0].issue_type, "Task");
        assert!(drafts[0].description.is_empty());
    }

    #[test]
    fn an_unusable_reply_is_none_rather_than_a_panic() {
        let items = vec!["one".to_string()];
        assert!(parse("not json at all", &items).is_none());
        assert!(parse(r#"{"something": "else"}"#, &items).is_none());
        assert!(parse(r#"{"tickets": "not a list"}"#, &items).is_none());
        // A ticket citing item 0 or a negative index is ignored rather than
        // wrapping around to the end of the list.
        let (_, covered, _) =
            parse(r#"{"tickets":[{"items":[0,-1,1],"summary":"s"}]}"#, &items).unwrap();
        assert_eq!(covered, [vec![0]]);
    }

    #[test]
    fn a_draft_becomes_an_issue_for_the_chosen_project() {
        let draft = Draft {
            summary: "Add a flag".into(),
            description: "Because.".into(),
            issue_type: "Task".into(),
            labels: vec!["cli".into()],
            source: vec!["Add a --json flag".into()],
        };
        let issue = draft.to_issue("ABC");
        assert_eq!(issue.project_key, "ABC");
        assert_eq!(issue.summary, "Add a flag");
        assert_eq!(issue.issue_type, "Task");
        assert_eq!(issue.labels, ["cli"]);
        assert!(issue.parent.is_none());
        // And it makes a document Jira will take.
        assert_eq!(issue.fields()["description"]["type"], "doc");
    }
}
