//! The command palette: everything the app can do, by name.
//!
//! The point of a palette is discoverability — a keyboard-driven index of
//! features that would otherwise need to be found in a menu, a tab, or a
//! shortcut nobody remembers. So the list is written for someone who does
//! not know what the feature is called: each entry has a hint, and the
//! filter matches both.

use super::{App, Dialog, Tab};

/// One thing the palette can do.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Cmd {
    // Files and views
    OpenFile,
    SearchFiles,
    GoToLine,
    Find,
    Replace,
    ToggleOutline,
    Save,
    Format,
    // Git
    Refresh,
    Commit,
    Push,
    Pull,
    Fetch,
    Branches,
    Worktrees,
    PullRequests,
    Stack,
    Tickets,
    Stash,
    History,
    SearchCommits,
    Undo,
    TidyHistory,
    SplitCommits,
    Conflicts,
    // AI
    AiCommitMessage,
    AiExplain,
    AiFix,
    AiTests,
    AiDocument,
    AiAgent,
    Review,
    // Tools
    Terminal,
    RunChecks,
    Settings,
    Shortcuts,
    ToggleTheme,
    ZoomIn,
    ZoomOut,
    ZoomReset,
    Graph,
}

/// One row in the palette.
pub struct Command {
    pub name: &'static str,
    /// What it is for, in the words someone would search with.
    pub hint: &'static str,
    pub cmd: Cmd,
}

/// Everything the palette offers, in the order it is shown.
pub fn commands() -> Vec<Command> {
    let c = |name, hint, cmd| Command { name, hint, cmd };
    vec![
        c("Open file", "editor quick open filter tracked files", Cmd::OpenFile),
        c("Search in files", "grep find text across the repository", Cmd::SearchFiles),
        c("Find", "search within this file", Cmd::Find),
        c("Replace", "find and replace in this file", Cmd::Replace),
        c("Go to line", "jump to a line number", Cmd::GoToLine),
        c("Save file", "write the buffer to disk", Cmd::Save),
        c("Format file", "language server formatting", Cmd::Format),
        c("Toggle outline", "symbols in this file", Cmd::ToggleOutline),
        c("Terminal", "shell console command line", Cmd::Terminal),
        c("Commit", "commit the selected changes", Cmd::Commit),
        c("Push", "publish this branch", Cmd::Push),
        c("Pull", "bring down remote changes", Cmd::Pull),
        c("Fetch", "update remote refs without merging", Cmd::Fetch),
        c("Refresh", "reload status and branches", Cmd::Refresh),
        c("Branches", "switch create delete branches", Cmd::Branches),
        c(
            "Worktrees",
            "worktree checkout a branch in its own directory parallel agents new window",
            Cmd::Worktrees,
        ),
        c("Pull requests", "github open create review", Cmd::PullRequests),
        c(
            "Stacked pull requests",
            "stack chain of branches restack submit sync dependent prs",
            Cmd::Stack,
        ),
        c(
            "Write Jira tickets",
            "jira tickets issues from a list backlog sprint atlassian",
            Cmd::Tickets,
        ),
        c("Stashes", "shelve changes for later", Cmd::Stash),
        c("Commit graph", "visualise branches and merges", Cmd::Graph),
        c("History", "commit log", Cmd::History),
        c("Search commits", "pickaxe message author path history", Cmd::SearchCommits),
        c("Undo…", "reflog go back after a bad merge or rebase", Cmd::Undo),
        c("Tidy history", "ai squash reword commits before a pull request", Cmd::TidyHistory),
        c("Split into commits", "ai group changes into separate commits", Cmd::SplitCommits),
        c("Resolve conflicts", "merge conflict resolver", Cmd::Conflicts),
        c("AI commit message", "generate a message from the staged diff", Cmd::AiCommitMessage),
        c("AI: explain this", "what does the selected code do", Cmd::AiExplain),
        c("AI: fix this", "repair the errors in the selection", Cmd::AiFix),
        c("AI: write tests", "generate tests for the selection", Cmd::AiTests),
        c("AI: document this", "write doc comments for the selection", Cmd::AiDocument),
        c("AI coding agent", "give the agent a task", Cmd::AiAgent),
        c("AI code review", "review the outgoing diff now", Cmd::Review),
        c("Run checks", "local ci build test lint", Cmd::RunChecks),
        c("Settings", "preferences ollama claude appearance", Cmd::Settings),
        c("Keyboard shortcuts", "rebind keys", Cmd::Shortcuts),
        c("Toggle light/dark theme", "appearance colours", Cmd::ToggleTheme),
        c("Zoom in", "bigger text interface scale", Cmd::ZoomIn),
        c("Zoom out", "smaller text interface scale", Cmd::ZoomOut),
        c("Reset zoom", "interface scale 100%", Cmd::ZoomReset),
    ]
}

/// Commands whose name or hint matches, best first.
///
/// A match on the name ranks above a match on the hint: someone typing
/// "commit" wants Commit before "AI commit message", and someone typing
/// "grep" — a word in no name at all — still finds search.
pub fn matches<'a>(commands: &'a [Command], query: &str) -> Vec<&'a Command> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return commands.iter().collect();
    }
    let mut by_name = Vec::new();
    let mut by_hint = Vec::new();
    for command in commands {
        if command.name.to_lowercase().contains(&needle) {
            by_name.push(command);
        } else if command.hint.contains(&needle) {
            by_hint.push(command);
        }
    }
    by_name.extend(by_hint);
    by_name
}

/// Carries out a command.
pub fn run(app: &mut App, cmd: Cmd) {
    use crate::agent::assist::Kind;
    app.dialog = Dialog::None;
    match cmd {
        Cmd::OpenFile => app.editor_quick_open(),
        Cmd::SearchFiles => {
            app.tab = Tab::Editor;
            app.editor.search.open = true;
        }
        Cmd::Find | Cmd::Replace => {
            app.tab = Tab::Editor;
            app.editor.find.open = true;
            app.editor.find.replacing = cmd == Cmd::Replace;
            app.editor.find.focus = true;
        }
        Cmd::GoToLine => {
            app.tab = Tab::Editor;
            app.editor.goto_line = Some(String::new());
        }
        Cmd::Save => app.editor_save(),
        Cmd::Format => app.editor_format(),
        Cmd::ToggleOutline => {
            app.tab = Tab::Editor;
            app.editor.outline_open = !app.editor.outline_open;
        }
        Cmd::Terminal => {
            #[cfg(unix)]
            app.terminal_open(false);
        }
        Cmd::Refresh => app.refresh(),
        Cmd::Commit => {
            app.tab = Tab::Changes;
            app.do_commit();
        }
        Cmd::Push => app.shortcut_sync("push"),
        Cmd::Pull => app.shortcut_sync("pull"),
        Cmd::Fetch => app.shortcut_sync("fetch"),
        Cmd::Branches => app.tab = Tab::Changes,
        Cmd::Worktrees => app.open_worktrees(),
        Cmd::PullRequests => app.dialog = Dialog::PullRequests,
        Cmd::Stack => app.open_stack(),
        Cmd::Tickets => app.open_tickets(),
        Cmd::Stash => app.tab = Tab::Changes,
        Cmd::Graph => {
            app.graph_open = !app.graph_open;
            if app.graph_open {
                app.load_graph();
            }
        }
        Cmd::History => {
            app.tab = Tab::History;
            app.load_history();
        }
        Cmd::SearchCommits => {
            app.tab = Tab::History;
            app.history_query.clear();
        }
        Cmd::Undo => app.open_reflog(),
        Cmd::TidyHistory => app.start_tidy(),
        Cmd::SplitCommits => app.start_split(),
        Cmd::Conflicts => app.load_conflicts(),
        Cmd::AiCommitMessage => app.request_ai_message(),
        Cmd::AiExplain => app.editor_assist(Kind::Explain),
        Cmd::AiFix => app.editor_assist(Kind::Fix),
        Cmd::AiTests => app.editor_assist(Kind::Tests),
        Cmd::AiDocument => app.editor_assist(Kind::Document),
        Cmd::AiAgent => app.tab = Tab::Agent,
        Cmd::Review => app.review_now(),
        Cmd::RunChecks => {
            app.tab = Tab::Checks;
            app.run_local_ci();
        }
        Cmd::Settings => app.dialog = Dialog::Settings,
        Cmd::Shortcuts => app.dialog = Dialog::Settings,
        Cmd::ToggleTheme => {
            let light = !app.config.light_theme;
            app.set_light_theme(light);
        }
        Cmd::ZoomIn => app.zoom_by(0.1),
        Cmd::ZoomOut => app.zoom_by(-0.1),
        Cmd::ZoomReset => app.set_zoom(1.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_name_wins_over_the_hint() {
        let commands = commands();
        let hits = matches(&commands, "commit");
        // "Commit" itself, before "AI commit message", before anything that
        // only mentions committing in its hint.
        assert_eq!(hits[0].name, "Commit");
        assert!(hits.iter().any(|c| c.name == "AI commit message"));
    }

    #[test]
    fn a_word_in_no_name_still_finds_the_command() {
        let commands = commands();
        // Nobody calls it "Search in files" in their head.
        assert_eq!(matches(&commands, "grep")[0].name, "Search in files");
        assert_eq!(matches(&commands, "pickaxe")[0].name, "Search commits");
        assert_eq!(matches(&commands, "shell")[0].name, "Terminal");
        assert_eq!(matches(&commands, "reflog")[0].name, "Undo…");
    }

    #[test]
    fn an_empty_query_lists_everything() {
        let commands = commands();
        assert_eq!(matches(&commands, "   ").len(), commands.len());
        assert!(matches(&commands, "zzzznothing").is_empty());
    }

    #[test]
    fn every_command_is_reachable_and_described() {
        for command in commands() {
            assert!(!command.name.is_empty());
            assert!(
                !command.hint.is_empty(),
                "{} needs a hint: it is what makes it findable",
                command.name
            );
            // Hints are matched lower-cased, so they must be written that way.
            assert_eq!(command.hint, command.hint.to_lowercase(), "{}", command.name);
        }
    }
}
