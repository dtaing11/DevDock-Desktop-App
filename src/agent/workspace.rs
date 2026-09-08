//! The tools the harness exposes, and the sandbox they run in.
//!
//! A [`Workspace`] is one repository worktree plus the list of files git
//! tracks in it, and an [`Access`] level. Everything the model can do to a
//! repository goes through [`Workspace::dispatch`], which is where the two
//! rules that make worktree-wide access safe live:
//!
//! 1. **Paths stay inside the repository.** Absolute paths, `..` escapes,
//!    symlinks pointing out, and anything under `.git/` are refused.
//! 2. **Reads see only tracked files.** Untracked files are invisible, so
//!    the ignored `.env`, the local scratch file, and the build output are
//!    not readable and cannot be shipped to a model provider. Files the
//!    model itself proposes are readable, because it needs to see its own
//!    work in progress.
//!
//! Writes never reach the disk here. [`Workspace::dispatch`] records them in
//! an overlay that reads see through, and [`Workspace::edits`] hands the set
//! to the caller for the user to confirm, file by file.

use super::{ToolCall, ToolResult, ToolSpec};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// Where proposed writes go.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WriteMode {
    /// Held in memory until the user accepts them. Nothing on disk changes,
    /// which means nothing can compile or test them either.
    #[default]
    Overlay,
    /// Written to the worktree as they are made, so the language server and
    /// the project's own checks can see them.
    ///
    /// The pre-run content of every touched file is kept, so rejecting a
    /// change restores it exactly. This is what "let it iterate" costs: the
    /// working tree really does change while the model works, and the
    /// confirmation at the end is a keep-or-revert rather than an
    /// apply-or-discard.
    Live,
}

/// What the model may do to the worktree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Reading and searching only: the code review gate.
    ReadOnly,
    /// Reading plus proposing edits: the conflict resolver.
    ReadWrite,
}

/// One proposed change to one file, still unwritten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingEdit {
    /// Repo-relative path.
    pub path: String,
    /// Content before the run, or `None` for a file the model created.
    pub before: Option<String>,
    /// Content the model proposes.
    pub after: String,
}

impl PendingEdit {
    /// Whether this edit creates a file that did not exist.
    pub fn is_new(&self) -> bool {
        self.before.is_none()
    }

    /// Added/removed line counts, for a one-line summary in the UI.
    pub fn line_delta(&self) -> (usize, usize) {
        let before = self.before.as_deref().unwrap_or("");
        let before_lines: Vec<&str> = before.lines().collect();
        let after_lines: Vec<&str> = self.after.lines().collect();
        let common = before_lines
            .iter()
            .zip(after_lines.iter())
            .take_while(|(a, b)| a == b)
            .count();
        let tail = before_lines[common..]
            .iter()
            .rev()
            .zip(after_lines[common..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        (
            after_lines.len().saturating_sub(common + tail),
            before_lines.len().saturating_sub(common + tail),
        )
    }
}

/// Per-call caps. A single tool result that dwarfs the diff under review is
/// as bad as no context at all, so each result is bounded before the run
/// budget in [`super::Limits`] ever comes into play.
const MAX_READ_BYTES: usize = 60_000;
const MAX_READ_LINES: usize = 600;
const MAX_LIST_ENTRIES: usize = 400;
const MAX_SEARCH_HITS: usize = 80;
/// How much file content one search may scan. A repository with tens of
/// thousands of files would otherwise turn every search into a full read of
/// the tree, and the user is watching a spinner.
const MAX_SEARCH_SCAN_BYTES: usize = 8_000_000;
/// A hard stop on a model that would otherwise run the test suite after
/// every edit.
const MAX_CHECK_RUNS: usize = 12;
/// Cap on a diff of the run's own changes.
const MAX_CHANGES_BYTES: usize = 40_000;
/// Cap on a project instructions file (AGENTS.md and friends).
const MAX_INSTRUCTIONS_BYTES: usize = 12_000;
/// Files named by convention that tell an agent how a project wants to be
/// worked on. First one tracked wins.
const INSTRUCTION_FILES: &[&str] =
    &["AGENTS.md", "CLAUDE.md", ".github/copilot-instructions.md", ".cursorrules"];

/// One repository, its tracked files, and the edits proposed so far.
pub struct Workspace {
    root: PathBuf,
    tracked: Vec<String>,
    access: Access,
    /// Proposed content, keyed by repo-relative path. Reads see through it.
    overlay: BTreeMap<String, String>,
    /// Content as it was when the run started, for the confirmation diff.
    originals: BTreeMap<String, Option<String>>,
    calls: usize,
    bytes_read: usize,
    /// The model's own plan, which it writes and ticks off as it works.
    plan: Vec<super::PlanStep>,
    write_mode: WriteMode,
    /// Language servers, when the task is allowed to consult them.
    lsp: Option<Arc<crate::lsp::Manager>>,
    /// Diagnostic epoch per file at the moment it was last written, so
    /// `diagnostics` can wait for results about the new text.
    epochs: BTreeMap<String, u64>,
    /// The project's own checks, the only commands that may be run.
    checks: Vec<crate::local_ci::Job>,
    check_runs: usize,
    /// Files edited since the model last asked for their diagnostics.
    undiagnosed: BTreeSet<String>,
    /// An edit happened after the last check run (or no check ran at all).
    edited_since_check: bool,
}

impl Workspace {
    /// Builds a workspace over `root`, whose tracked files are `tracked`
    /// (repo-relative paths, e.g. from `git ls-files`).
    pub fn new(
        root: &Path,
        mut tracked: Vec<String>,
        access: Access,
    ) -> Result<Self, String> {
        let root = root
            .canonicalize()
            .map_err(|e| format!("cannot resolve the repository root: {e}"))?;
        tracked.sort();
        tracked.dedup();
        Ok(Self {
            root,
            tracked,
            access,
            overlay: BTreeMap::new(),
            originals: BTreeMap::new(),
            calls: 0,
            bytes_read: 0,
            plan: Vec::new(),
            write_mode: WriteMode::Overlay,
            lsp: None,
            epochs: BTreeMap::new(),
            checks: Vec::new(),
            check_runs: 0,
            undiagnosed: BTreeSet::new(),
            edited_since_check: false,
        })
    }

    /// Writes straight to the worktree instead of an overlay. Only for a run
    /// that has to compile or test what it wrote — see [`WriteMode::Live`].
    pub fn with_write_mode(mut self, mode: WriteMode) -> Self {
        self.write_mode = mode;
        self
    }

    /// Lets the model consult language servers: diagnostics, definitions,
    /// references, and workspace symbols.
    pub fn with_language_support(mut self, lsp: Arc<crate::lsp::Manager>) -> Self {
        self.lsp = Some(lsp);
        self
    }

    /// Allows running the project's own checks, and nothing else. The model
    /// picks a job by name from what the repository already declares, so
    /// there is no arbitrary command to inject into.
    pub fn with_checks(mut self, checks: Vec<crate::local_ci::Job>) -> Self {
        self.checks = checks;
        self
    }

    pub fn write_mode(&self) -> WriteMode {
        self.write_mode
    }

    /// Whether this run may change files at all.
    pub fn can_edit(&self) -> bool {
        self.access == Access::ReadWrite
    }

    /// Checks this run can run, by name, and how many times it has.
    pub fn checks_available(&self) -> Vec<String> {
        if self.write_mode == WriteMode::Live {
            self.checks.iter().map(|c| c.name.clone()).collect()
        } else {
            Vec::new()
        }
    }

    pub fn check_runs(&self) -> usize {
        self.check_runs
    }

    /// Restores every file this run changed to its pre-run content. Used
    /// when a live run is rejected wholesale.
    pub fn revert_all(&self) -> Result<(), String> {
        let mut failed = Vec::new();
        for (rel, before) in &self.originals {
            let full = self.root.join(rel);
            let result = match before {
                Some(text) => std::fs::write(&full, text).map_err(|e| e.to_string()),
                // The file did not exist before this run.
                None => std::fs::remove_file(&full).map_err(|e| e.to_string()),
            };
            if let Err(e) = result {
                failed.push(format!("{rel}: {e}"));
            }
        }
        if failed.is_empty() {
            Ok(())
        } else {
            Err(failed.join("; "))
        }
    }

    pub fn calls_used(&self) -> usize {
        self.calls
    }

    pub fn bytes_read(&self) -> usize {
        self.bytes_read
    }

    /// The model's plan as it currently stands.
    pub fn plan(&self) -> Vec<super::PlanStep> {
        self.plan.clone()
    }

    /// Every proposed change, oldest path first. Unchanged files are
    /// dropped: a model that rewrites a file to its existing content has
    /// not made a change worth confirming.
    pub fn edits(&self) -> Vec<PendingEdit> {
        self.overlay
            .iter()
            .filter_map(|(path, after)| {
                let before = self.originals.get(path).cloned().flatten();
                if before.as_deref() == Some(after.as_str()) {
                    return None;
                }
                Some(PendingEdit { path: path.clone(), before, after: after.clone() })
            })
            .collect()
    }

    /// The text of a tracked file, for code that needs to check something
    /// against the repository without going through a tool call.
    pub fn read_tracked(&self, path: &str) -> Option<String> {
        let rel = self.resolve_readable(path).ok()?;
        self.current_content(&rel).ok()
    }

    /// What the model changed but never checked, if anything: the reason the
    /// harness sends it back once before accepting a final answer.
    ///
    /// Checks come first — they are the definition of done a repository
    /// declares — then diagnostics on files edited since they were last
    /// looked at. Nothing is asked for that the run cannot do: no check
    /// without a live tree, no diagnostics without a language server, and
    /// no check once the check budget is spent.
    pub fn verification_gap(&self) -> Option<String> {
        if self.edits().is_empty() {
            return None;
        }
        let changed = self.edits().len();
        if self.write_mode == WriteMode::Live
            && !self.checks.is_empty()
            && self.edited_since_check
            && self.check_runs < MAX_CHECK_RUNS
        {
            let names: Vec<&str> = self.checks.iter().map(|c| c.name.as_str()).collect();
            return Some(format!(
                "you changed {changed} file(s) but have not run a check since your last \
                 edit. Run the relevant one with run_check ({}).",
                names.join(", ")
            ));
        }
        if self.lsp.is_some() && !self.undiagnosed.is_empty() {
            let files: Vec<&str> = self.undiagnosed.iter().map(String::as_str).collect();
            return Some(format!(
                "you have not asked for diagnostics on {} since editing it. Call \
                 diagnostics for each changed file.",
                files.join(", ")
            ));
        }
        None
    }

    /// A few lines about the repository for the opening prompt: how big it
    /// is, where the code lives, what it is written in, and which project
    /// files exist — enough to search in the right place on the first turn
    /// instead of the third.
    pub fn overview(&self) -> String {
        let total = self.tracked.len();
        let mut dirs: BTreeMap<&str, usize> = BTreeMap::new();
        let mut exts: BTreeMap<&str, usize> = BTreeMap::new();
        let mut root_files = 0;
        for path in &self.tracked {
            match path.split_once('/') {
                Some((dir, _)) => *dirs.entry(dir).or_default() += 1,
                None => root_files += 1,
            }
            if let Some(ext) = Path::new(path).extension().and_then(|e| e.to_str()) {
                *exts.entry(ext).or_default() += 1;
            }
        }
        let mut dirs: Vec<(&str, usize)> = dirs.into_iter().collect();
        dirs.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        let mut exts: Vec<(&str, usize)> = exts.into_iter().collect();
        exts.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));

        let mut out = format!("{total} tracked file(s).");
        if !dirs.is_empty() {
            let shown: Vec<String> =
                dirs.iter().take(12).map(|(d, n)| format!("{d}/ ({n})")).collect();
            out.push_str(&format!(" Top-level: {}", shown.join(", ")));
            if dirs.len() > 12 {
                out.push_str(&format!(", and {} more", dirs.len() - 12));
            }
            if root_files > 0 {
                out.push_str(&format!("; {root_files} file(s) at the root"));
            }
            out.push('.');
        }
        if !exts.is_empty() {
            let shown: Vec<String> =
                exts.iter().take(8).map(|(e, n)| format!(".{e} {n}")).collect();
            out.push_str(&format!(" By type: {}.", shown.join(", ")));
        }
        const PROJECT_FILES: &[&str] = &[
            "Cargo.toml",
            "package.json",
            "pyproject.toml",
            "setup.py",
            "requirements.txt",
            "go.mod",
            "pom.xml",
            "build.gradle",
            "Makefile",
            "CMakeLists.txt",
            "Gemfile",
            "mix.exs",
            "Dockerfile",
            ".git-manage-ci.toml",
            "README.md",
        ];
        let present: Vec<&str> = PROJECT_FILES
            .iter()
            .copied()
            .filter(|f| self.tracked.iter().any(|t| t == f))
            .collect();
        if !present.is_empty() {
            out.push_str(&format!(" Project files: {}.", present.join(", ")));
        }
        out
    }

    /// The repository's own instructions for agents, if it has a file for
    /// them (`AGENTS.md`, `CLAUDE.md`, …): its name and its text, capped.
    pub fn project_instructions(&self) -> Option<(String, String)> {
        let name = INSTRUCTION_FILES.iter().find(|f| self.tracked.iter().any(|t| t == *f))?;
        let text = self.current_content(name).ok()?;
        let text = if text.len() > MAX_INSTRUCTIONS_BYTES {
            let end = (0..=MAX_INSTRUCTIONS_BYTES)
                .rev()
                .find(|i| text.is_char_boundary(*i))
                .unwrap_or(0);
            format!("{}\n[truncated]", &text[..end])
        } else {
            text
        };
        (!text.trim().is_empty()).then(|| (name.to_string(), text))
    }

    /// The tools available at this access level.
    pub fn tools(&self) -> Vec<ToolSpec> {
        let mut tools = vec![
            ToolSpec {
                name: "list_files",
                description: "List the repository's tracked files. Optionally filter with a \
                              glob such as \"src/*.rs\" or \"*/tests/*\". This is how to find a \
                              file by name; search looks inside files, not at their names.",
                schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "glob": {
                            "type": "string",
                            "description": "Optional glob filter over repo-relative paths. \
                                            '*' matches any characters, '?' matches one."
                        }
                    }
                }),
            },
            ToolSpec {
                name: "read_file",
                description: "Read a tracked file, or a file you have already proposed \
                              changes to. Returns numbered lines. Read the parts you need \
                              rather than whole large files.",
                schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "Repo-relative path."},
                        "start_line": {"type": "integer", "description": "1-based first line. Defaults to 1."},
                        "line_count": {"type": "integer", "description": "How many lines to return."}
                    },
                    "required": ["path"]
                }),
            },
            ToolSpec {
                name: "search",
                description: "Case-insensitive search across tracked files: literal text by \
                              default, a regular expression with regex=true. Returns \
                              path:line: matching text, with surrounding lines when context \
                              is set. Use it to find definitions, callers, and other uses of \
                              a symbol before reading whole files.",
                schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Text that appears in the code — an identifier, a string, a line — not a description of what you are looking for, and not a file name (use list_files for that). A regex when regex is true."},
                        "regex": {"type": "boolean", "description": "Treat query as a regular expression (Rust regex syntax)."},
                        "glob": {"type": "string", "description": "Optional glob to restrict which files are searched."},
                        "context": {"type": "integer", "description": "Lines of context to show around each hit (0-5)."},
                        "max_results": {"type": "integer", "description": "Cap on hits returned."}
                    },
                    "required": ["query"]
                }),
            },
        ];

        tools.push(ToolSpec {
            name: "update_plan",
            description: "Write down what you are going to do, and tick items off as you \
                          finish them. Call this once at the start with the whole plan, \
                          then again each time a step is done. The developer watches this \
                          list while you work — it is how they know what you are doing and \
                          how far along you are. Send the full list every time.",
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "steps": {
                        "type": "array",
                        "description": "Every step, in order, including the finished ones.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "text": {"type": "string", "description": "One short line, in the imperative."},
                                "done": {"type": "boolean", "description": "Whether it is finished."}
                            },
                            "required": ["text", "done"]
                        }
                    }
                },
                "required": ["steps"]
            }),
        });

        if self.lsp.is_some() {
            tools.push(ToolSpec {
                name: "diagnostics",
                description: "Ask the language server what is wrong with a file: compiler \
                              errors, warnings, and lints, with line numbers. After changing \
                              a file, call this to check your work before moving on. Omit \
                              the path for every file the server has looked at.",
                schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "Repo-relative path, or omit for everything."}
                    }
                }),
            });
            tools.push(ToolSpec {
                name: "definition",
                description: "Where a symbol is defined, from the language server. Give the \
                              file and line you saw it on and the exact symbol text.",
                schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "Repo-relative path."},
                        "line": {"type": "integer", "description": "1-based line the symbol appears on."},
                        "symbol": {"type": "string", "description": "The symbol's exact text on that line."}
                    },
                    "required": ["path", "line", "symbol"]
                }),
            });
            tools.push(ToolSpec {
                name: "references",
                description: "Every use of a symbol, from the language server. This is how \
                              you find what a signature change breaks — more reliable than \
                              a text search, which cannot tell two same-named things apart.",
                schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "Repo-relative path."},
                        "line": {"type": "integer", "description": "1-based line the symbol appears on."},
                        "symbol": {"type": "string", "description": "The symbol's exact text on that line."}
                    },
                    "required": ["path", "line", "symbol"]
                }),
            });
            tools.push(ToolSpec {
                name: "find_symbol",
                description: "Search the whole workspace for a symbol by name, from the \
                              language server. Use it to locate a definition when you do \
                              not know which file it is in.",
                schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Symbol name or part of one."}
                    },
                    "required": ["query"]
                }),
            });
        }

        if !self.checks.is_empty() {
            let names: Vec<&str> = self.checks.iter().map(|c| c.name.as_str()).collect();
            tools.push(ToolSpec {
                name: "run_check",
                description: Box::leak(
                    format!(
                        "Run one of this project's own checks and get its output. \
                         Available: {}. These are the commands the repository already \
                         declares; nothing else can be run. Use this to prove a change \
                         builds and passes before you report it as done.",
                        names.join(", ")
                    )
                    .into_boxed_str(),
                ),
                schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {"type": "string", "description": "Which check to run."}
                    },
                    "required": ["name"]
                }),
            });
        }

        if self.access == Access::ReadWrite {
            tools.push(ToolSpec {
                name: "write_file",
                description: "Propose the complete new content of a file. Nothing is written \
                              to disk: every proposal is shown to the user, who accepts or \
                              rejects it file by file. Prefer edit_file for small changes.",
                schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "Repo-relative path."},
                        "content": {"type": "string", "description": "The file's full new content."}
                    },
                    "required": ["path", "content"]
                }),
            });
            tools.push(ToolSpec {
                name: "edit_file",
                description: "Propose replacing an exact snippet of a file. old_text must \
                              appear exactly once unless replace_all is true. Nothing is \
                              written to disk until the user accepts the proposal.",
                schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "Repo-relative path."},
                        "old_text": {"type": "string", "description": "Exact text to replace, including indentation."},
                        "new_text": {"type": "string", "description": "Replacement text."},
                        "replace_all": {"type": "boolean", "description": "Replace every occurrence instead of requiring a unique match."}
                    },
                    "required": ["path", "old_text", "new_text"]
                }),
            });
            tools.push(ToolSpec {
                name: "replace_lines",
                description: "Replace a range of lines, by the line numbers read_file showed, \
                              with new text. Use this instead of edit_file when the text is \
                              hard to reproduce exactly — escapes, tabs, long lines — or \
                              when edit_file could not find its snippet. Read the file first \
                              so the numbers are current; the reply shows the result.",
                schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "Repo-relative path."},
                        "start_line": {"type": "integer", "description": "1-based first line to replace."},
                        "end_line": {"type": "integer", "description": "1-based last line to replace, inclusive. Equal to start_line for one line."},
                        "new_text": {"type": "string", "description": "Replacement lines; empty deletes the range."},
                        "expect_first_line": {"type": "string", "description": "Optional: what start_line currently says (whitespace-insensitive), as a guard against stale numbers."}
                    },
                    "required": ["path", "start_line", "end_line", "new_text"]
                }),
            });
            tools.push(ToolSpec {
                name: "show_changes",
                description: "The diff of everything you have changed so far in this run, \
                              against the files as they were when it started. Review it \
                              before you report: it is what the developer will see.",
                schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string", "description": "One file, or omit for all of them."}
                    }
                }),
            });
        }
        tools
    }

    /// Runs one tool call. Tool failures come back as results with
    /// `is_error`, not as `Err`: a model that asked for a missing file
    /// should be told so and given another turn, not have the run aborted.
    pub fn dispatch(&mut self, call: &ToolCall, max_calls: usize) -> ToolResult {
        self.calls += 1;
        if self.calls > max_calls {
            return self.error(call, "Tool budget exhausted. Answer with what you have.");
        }
        let outcome = match call.name.as_str() {
            "update_plan" => self.update_plan(&call.input),
            "list_files" => self.list_files(&call.input),
            "read_file" => self.read_file(&call.input),
            "search" => self.search(&call.input),
            "write_file" if self.access == Access::ReadWrite => self.write_file(&call.input),
            "edit_file" if self.access == Access::ReadWrite => self.edit_file(&call.input),
            "replace_lines" if self.access == Access::ReadWrite => self.replace_lines(&call.input),
            "show_changes" if self.access == Access::ReadWrite => self.show_changes(&call.input),
            "diagnostics" if self.lsp.is_some() => self.diagnostics(&call.input),
            "definition" if self.lsp.is_some() => self.locate(&call.input, false),
            "references" if self.lsp.is_some() => self.locate(&call.input, true),
            "find_symbol" if self.lsp.is_some() => self.find_symbol(&call.input),
            "run_check" if !self.checks.is_empty() => self.run_check(&call.input),
            "write_file" | "edit_file" | "replace_lines" => {
                Err("This run is read-only: you can inspect the repository but not change \
                     it. Report what you found instead."
                    .to_string())
            }
            other => Err(format!("No such tool: {other}")),
        };
        match outcome {
            Ok(content) => {
                self.bytes_read += content.len();
                ToolResult {
                    id: call.id.clone(),
                    name: call.name.clone(),
                    content,
                    is_error: false,
                }
            }
            Err(message) => self.error(call, &message),
        }
    }

    fn error(&self, call: &ToolCall, message: &str) -> ToolResult {
        ToolResult {
            id: call.id.clone(),
            name: call.name.clone(),
            content: message.to_string(),
            is_error: true,
        }
    }

    // -- tools --------------------------------------------------------------

    fn list_files(&self, input: &serde_json::Value) -> Result<String, String> {
        let glob = input.get("glob").and_then(|g| g.as_str());
        let matches: Vec<&String> = self
            .visible_paths()
            .into_iter()
            .filter(|p| glob.is_none_or(|g| glob_match(g, p)))
            .collect();
        if matches.is_empty() {
            return Ok(match glob {
                Some(g) => format!("No tracked files match {g}."),
                None => "No tracked files.".into(),
            });
        }
        let shown: Vec<&str> = matches.iter().take(MAX_LIST_ENTRIES).map(|p| p.as_str()).collect();
        let mut out = shown.join("\n");
        if matches.len() > shown.len() {
            out.push_str(&format!(
                "\n\n[{} more files not shown; narrow the glob]",
                matches.len() - shown.len()
            ));
        }
        Ok(out)
    }

    fn read_file(&self, input: &serde_json::Value) -> Result<String, String> {
        let path = self.arg_str(input, "path")?;
        let rel = self.resolve_readable(&path)?;
        let content = self.current_content(&rel)?;

        let start = input
            .get("start_line")
            .and_then(|v| v.as_u64())
            .unwrap_or(1)
            .max(1) as usize;
        let count = input
            .get("line_count")
            .and_then(|v| v.as_u64())
            .unwrap_or(MAX_READ_LINES as u64)
            .min(MAX_READ_LINES as u64) as usize;

        let lines: Vec<&str> = content.lines().collect();
        if start > lines.len() && !lines.is_empty() {
            return Err(format!(
                "{rel} has {} lines; start_line {start} is past the end.",
                lines.len()
            ));
        }
        let end = (start - 1 + count).min(lines.len());
        let mut out = String::new();
        let mut bytes = 0;
        let mut last = start - 1;
        for (i, line) in lines[start - 1..end].iter().enumerate() {
            if bytes + line.len() > MAX_READ_BYTES {
                break;
            }
            bytes += line.len() + 1;
            last = start + i;
            out.push_str(&format!("{:>6}  {line}\n", start + i));
        }
        if out.is_empty() {
            return Ok(format!("{rel} is empty."));
        }
        let mut header = format!("{rel} (lines {start}-{last} of {})\n", lines.len());
        if last < lines.len() {
            header.push_str("[truncated; read on with start_line]\n");
        }
        Ok(header + &out)
    }

    fn search(&self, input: &serde_json::Value) -> Result<String, String> {
        let query = self.arg_str(input, "query")?;
        if query.trim().is_empty() {
            return Err("query must not be empty.".into());
        }
        let glob = input.get("glob").and_then(|g| g.as_str());
        let cap = input
            .get("max_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(MAX_SEARCH_HITS as u64)
            .min(MAX_SEARCH_HITS as u64) as usize;
        let context = input.get("context").and_then(|v| v.as_u64()).unwrap_or(0).min(5) as usize;
        let use_regex = input.get("regex").and_then(|v| v.as_bool()).unwrap_or(false);
        let pattern = if use_regex {
            regex::RegexBuilder::new(&query)
                .case_insensitive(true)
                .size_limit(1 << 20)
                .build()
                .map_err(|e| format!("Invalid regex: {e}"))?
        } else {
            regex::RegexBuilder::new(&regex::escape(&query))
                .case_insensitive(true)
                .build()
                .map_err(|e| format!("Invalid query: {e}"))?
        };
        let clip = |line: &str| -> String {
            let text = line.trim_end();
            if text.chars().count() > 200 {
                text.chars().take(200).collect::<String>() + "…"
            } else {
                text.to_string()
            }
        };

        let mut hits = Vec::new();
        let mut more = 0usize;
        let mut scanned = 0usize;
        let mut unscanned = 0usize;
        for rel in self.visible_paths() {
            if !glob.is_none_or(|g| glob_match(g, rel)) {
                continue;
            }
            if scanned >= MAX_SEARCH_SCAN_BYTES {
                unscanned += 1;
                continue;
            }
            let Ok(content) = self.current_content(rel) else { continue };
            scanned += content.len();
            let lines: Vec<&str> = content.lines().collect();
            for (i, line) in lines.iter().enumerate() {
                if !pattern.is_match(line) {
                    continue;
                }
                if hits.len() >= cap {
                    more += 1;
                    continue;
                }
                if context == 0 {
                    hits.push(format!("{rel}:{}: {}", i + 1, clip(line)));
                    continue;
                }
                let from = i.saturating_sub(context);
                let to = (i + context + 1).min(lines.len());
                let mut block = String::new();
                for (j, l) in lines[from..to].iter().enumerate() {
                    let n = from + j + 1;
                    let mark = if from + j == i { ':' } else { '-' };
                    block.push_str(&format!("{rel}{mark}{n}{mark} {}\n", clip(l)));
                }
                hits.push(block.trim_end().to_string());
            }
        }
        if hits.is_empty() {
            return Ok(match unscanned {
                0 => format!("No matches for {query}."),
                n => format!(
                    "No matches for {query} in what was searched; {n} file(s) were skipped \
                     after the scan limit. Narrow with a glob to cover them."
                ),
            });
        }
        let mut out = hits.join(if context > 0 { "\n--\n" } else { "\n" });
        if more > 0 {
            out.push_str(&format!("\n\n[{more} more matches; narrow the query or glob]"));
        }
        if unscanned > 0 {
            out.push_str(&format!(
                "\n\n[stopped after {MAX_SEARCH_SCAN_BYTES} bytes; {unscanned} file(s) were \
                 not searched. Narrow with a glob to cover them]"
            ));
        }
        Ok(out)
    }

    fn write_file(&mut self, input: &serde_json::Value) -> Result<String, String> {
        let path = self.arg_str(input, "path")?;
        let content = self.arg_str(input, "content")?;
        let rel = self.resolve_writable(&path)?;
        self.remember_original(&rel);
        let lines = content.lines().count();
        self.overlay.insert(rel.clone(), content);
        self.persist(&rel)?;
        self.note_edit(&rel);
        Ok(format!("Wrote {rel} ({lines} lines). {}", self.write_note()))
    }

    /// Bookkeeping for [`Self::verification_gap`].
    fn note_edit(&mut self, rel: &str) {
        self.undiagnosed.insert(rel.to_string());
        self.edited_since_check = true;
    }

    fn edit_file(&mut self, input: &serde_json::Value) -> Result<String, String> {
        let path = self.arg_str(input, "path")?;
        let old = self.arg_str(input, "old_text")?;
        let new = self.arg_str(input, "new_text")?;
        let all = input.get("replace_all").and_then(|v| v.as_bool()).unwrap_or(false);
        let rel = self.resolve_writable(&path)?;
        let current = self.current_content(&rel).map_err(|e| {
            format!("{e} Use write_file to propose a new file.")
        })?;

        if old.is_empty() {
            return Err("old_text must not be empty; use write_file for a new file.".into());
        }

        let count = current.matches(old.as_str()).count();
        let mut approximate: Option<(String, usize)> = None;
        let (updated, replaced) = if count == 0 {
            // Trailing whitespace and line endings are the usual reason an
            // otherwise exact snippet fails; a unique match modulo those is
            // what the model meant.
            match flexible_match(&current, &old) {
                Ok(Some((start, end))) => {
                    let mut updated = String::with_capacity(current.len() + new.len());
                    updated.push_str(&current[..start]);
                    updated.push_str(&new);
                    updated.push_str(&current[end..]);
                    (updated, 1)
                }
                Ok(None) => match approximate_match(&current, &old) {
                    // Close enough, and only one place it could mean: apply
                    // it there and say so, with the result, so a wrong guess
                    // is visible immediately rather than in the diff at the
                    // end.
                    Some((start, end, score)) => {
                        let mut updated = String::with_capacity(current.len() + new.len());
                        updated.push_str(&current[..start]);
                        updated.push_str(&new);
                        updated.push_str(&current[end..]);
                        let first_line = current[..start].matches('\n').count() + 1;
                        let last_line = first_line + old.lines().count().max(1) - 1;
                        let note = format!(
                            "old_text did not match exactly but was {:.0}% similar to lines \
                             {first_line}-{last_line}, the only close match, so the edit was \
                             applied there. Check the result below; use replace_lines if it \
                             is not what you meant.",
                            score * 100.0
                        );
                        approximate = Some((note, first_line));
                        (updated, 1)
                    }
                    None => {
                        return Err(format!(
                            "old_text does not appear in {rel}.{} If the text is hard to \
                             reproduce exactly (escapes, tabs), use replace_lines with the \
                             line numbers instead.",
                            nearest_hint(&current, &old)
                        ));
                    }
                },
                Err(n) => {
                    return Err(format!(
                        "old_text appears {n} times in {rel} (ignoring trailing whitespace). \
                         Include more surrounding context to make it unique, or pass \
                         replace_all."
                    ));
                }
            }
        } else if count > 1 && !all {
            return Err(format!(
                "old_text appears {count} times in {rel}. Include more surrounding context \
                 to make it unique, or pass replace_all."
            ));
        } else if all {
            (current.replace(old.as_str(), &new), count)
        } else {
            (current.replacen(old.as_str(), &new, 1), 1)
        };
        if updated == current {
            // Already the case — usually because the same edit was made a
            // turn ago and the model did not notice. Not an error: an error
            // here gets retried, which is the loop this line exists to end.
            return Ok(format!(
                "No change: {rel} already reads that way, so there was nothing to edit. \
                 If you meant something else, read the file again."
            ));
        }
        self.remember_original(&rel);
        self.overlay.insert(rel.clone(), updated.clone());
        self.persist(&rel)?;
        self.note_edit(&rel);
        let mut reply = format!(
            "Edited {rel} ({replaced} replacement{}). {}",
            if replaced != 1 { "s" } else { "" },
            self.write_note()
        );
        if let Some((note, first_line)) = approximate {
            let lines: Vec<&str> = updated.lines().collect();
            let from = first_line.saturating_sub(2);
            let to = (first_line - 1 + new.lines().count().max(1) + 1).min(lines.len());
            let echo: Vec<String> =
                lines[from..to].iter().enumerate().map(|(i, l)| format!("{:>6}  {l}", from + i + 1)).collect();
            reply = format!("{note}\n{reply}\nNow:\n{}", echo.join("\n"));
        }
        Ok(reply)
    }

    /// Replaces lines `start_line..=end_line` with `new_text`.
    ///
    /// Line numbers are what [`Self::read_file`] showed, so a model that has
    /// just read a region can change it without reproducing its exact text —
    /// the thing that goes wrong with escapes, tabs, and long lines.
    fn replace_lines(&mut self, input: &serde_json::Value) -> Result<String, String> {
        let path = self.arg_str(input, "path")?;
        let new_text = self.arg_str(input, "new_text")?;
        let start = input
            .get("start_line")
            .and_then(|v| v.as_u64())
            .ok_or("Missing required integer argument \"start_line\".")? as usize;
        let end = input
            .get("end_line")
            .and_then(|v| v.as_u64())
            .ok_or("Missing required integer argument \"end_line\".")? as usize;
        let rel = self.resolve_writable(&path)?;
        let current = self.current_content(&rel)?;
        let lines: Vec<&str> = current.lines().collect();
        if start == 0 || end < start {
            return Err(format!("Bad range {start}-{end}: lines are 1-based and end >= start."));
        }
        if end > lines.len() {
            return Err(format!("{rel} has {} lines; there is no line {end}.", lines.len()));
        }
        if let Some(expect) = input.get("expect_first_line").and_then(|v| v.as_str()) {
            let actual = lines[start - 1];
            if actual.split_whitespace().collect::<String>()
                != expect.split_whitespace().collect::<String>()
            {
                return Err(format!(
                    "Line {start} of {rel} is {actual:?}, not {expect:?}. Read the file again; \
                     the numbers may have moved."
                ));
            }
        }
        let mut updated = String::with_capacity(current.len() + new_text.len());
        for line in &lines[..start - 1] {
            updated.push_str(line);
            updated.push('\n');
        }
        let replacement: Vec<&str> = if new_text.is_empty() {
            Vec::new()
        } else {
            new_text.trim_end_matches('\n').lines().collect()
        };
        for line in &replacement {
            updated.push_str(line);
            updated.push('\n');
        }
        for line in &lines[end..] {
            updated.push_str(line);
            updated.push('\n');
        }
        if !current.ends_with('\n') && end == lines.len() && !replacement.is_empty() {
            // Keep a file that had no final newline that way.
            updated.pop();
        }
        if updated == current {
            return Ok(format!(
                "No change: lines {start}-{end} of {rel} already read exactly like new_text. \
                 If you meant something else, read the file again."
            ));
        }
        self.remember_original(&rel);
        self.overlay.insert(rel.clone(), updated.clone());
        self.persist(&rel)?;
        self.note_edit(&rel);
        // Echo the result with a line of context either side, numbered as
        // read_file would number it, so the next edit starts from the truth.
        let after: Vec<&str> = updated.lines().collect();
        let from = start.saturating_sub(2);
        let to = (start - 1 + replacement.len() + 1).min(after.len());
        let mut echo = String::new();
        for (i, line) in after[from..to].iter().enumerate() {
            echo.push_str(&format!("{:>6}  {line}\n", from + i + 1));
        }
        Ok(format!(
            "Replaced lines {start}-{end} of {rel} with {} line(s). {}\nNow:\n{echo}",
            replacement.len(),
            self.write_note()
        ))
    }

    /// The diff of the run's changes so far, as the developer will see it.
    fn show_changes(&self, input: &serde_json::Value) -> Result<String, String> {
        let only = match input.get("path").and_then(|p| p.as_str()) {
            Some(path) => Some(self.normalize(path)?),
            None => None,
        };
        let edits = self.edits();
        let mut out = String::new();
        for edit in edits.iter().filter(|e| only.as_deref().is_none_or(|p| p == e.path)) {
            let before = edit.before.clone().unwrap_or_default();
            out.push_str(&format!(
                "--- {}\n+++ {}\n",
                if edit.before.is_some() { edit.path.as_str() } else { "/dev/null" },
                edit.path
            ));
            for line in crate::app::textdiff::diff(&before, &edit.after) {
                use crate::app::textdiff::Line;
                match line {
                    Line::Context(l) => out.push_str(&format!(" {l}\n")),
                    Line::Added(l) => out.push_str(&format!("+{l}\n")),
                    Line::Removed(l) => out.push_str(&format!("-{l}\n")),
                    Line::Skipped(n) => out.push_str(&format!("@@ {n} unchanged line(s) @@\n")),
                }
            }
            out.push('\n');
            if out.len() > MAX_CHANGES_BYTES {
                out.push_str("[diff truncated; ask for one file at a time]\n");
                break;
            }
        }
        if out.is_empty() {
            return Ok(match only {
                Some(p) => format!("No changes to {p} in this run."),
                None => "No changes yet in this run.".into(),
            });
        }
        Ok(out)
    }

    /// What just happened to the file, which differs by write mode and is
    /// what the model needs to know to plan its next step.
    fn write_note(&self) -> &'static str {
        match self.write_mode {
            WriteMode::Overlay => {
                "Nothing is on disk yet: the user reviews every change at the end of the \
                 run and accepts or rejects it file by file."
            }
            WriteMode::Live => {
                "It is on disk, so diagnostics and checks now see it. The user reviews \
                 every change at the end and can revert any of them."
            }
        }
    }

    /// Writes an overlay entry to the worktree, for a [`WriteMode::Live`]
    /// run, and tells the language server about it.
    fn persist(&mut self, rel: &str) -> Result<(), String> {
        if self.write_mode != WriteMode::Live {
            return Ok(());
        }
        let Some(content) = self.overlay.get(rel).cloned() else { return Ok(()) };
        let full = self.root.join(rel);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{rel}: {e}"))?;
        }
        std::fs::write(&full, &content).map_err(|e| format!("{rel}: {e}"))?;

        // Record where the diagnostics stood *before* this edit, so a later
        // `diagnostics` call waits for results about the new text.
        if let Some(lsp) = self.lsp.clone() {
            if let Some(client) = lsp.running_for(&full) {
                self.epochs.insert(rel.to_string(), client.diagnostic_epoch(&full));
                let _ = client.did_change(&full, &content);
                let _ = client.did_save(&full, &content);
            }
        }
        Ok(())
    }

    /// Records the model's plan.
    fn update_plan(&mut self, input: &serde_json::Value) -> Result<String, String> {
        let steps = input
            .get("steps")
            .and_then(|s| s.as_array())
            .ok_or("Missing required array argument \"steps\".")?;
        let plan: Vec<super::PlanStep> = steps
            .iter()
            .filter_map(|step| {
                let text = step.get("text")?.as_str()?.trim();
                (!text.is_empty()).then(|| super::PlanStep {
                    text: text.to_string(),
                    done: step.get("done").and_then(|d| d.as_bool()).unwrap_or(false),
                })
            })
            .collect();
        if plan.is_empty() {
            return Err("A plan needs at least one step.".into());
        }
        let done = plan.iter().filter(|s| s.done).count();
        let total = plan.len();
        self.plan = plan;
        Ok(format!("Plan noted: {done}/{total} done."))
    }

    // -- language server ----------------------------------------------------

    /// Diagnostics for one file (or all of them), waiting for results that
    /// reflect the most recent edit.
    fn diagnostics(&mut self, input: &serde_json::Value) -> Result<String, String> {
        let lsp = self.lsp.clone().ok_or("no language server support in this run")?;

        let Some(path) = input.get("path").and_then(|p| p.as_str()) else {
            self.undiagnosed.clear();
            let all = lsp.all_diagnostics();
            if all.is_empty() {
                return Ok(
                    "No diagnostics. Note that a language server only reports on files it \
                     has opened; ask for a specific path to make it look."
                        .into(),
                );
            }
            let mut out = String::new();
            for (path, list) in all {
                let rel = path.strip_prefix(&self.root).unwrap_or(&path).display();
                out.push_str(&format!("{rel}:\n"));
                for d in list.iter().take(20) {
                    out.push_str(&format!("  {}\n", d.line()));
                }
            }
            return Ok(out);
        };

        let rel = self.normalize(path)?;
        self.undiagnosed.remove(&rel);
        let full = self.root.join(&rel);
        let client = lsp
            .ensure_for(&full)
            .map_err(|e| format!("{rel}: {e}"))?;

        // The server can only diagnose text it has been given.
        let text = self.current_content(&rel)?;
        if client.is_open(&full) {
            client.did_change(&full, &text)?;
            client.did_save(&full, &text)?;
        } else {
            client.did_open(&full, &text)?;
        }

        let after = self.epochs.get(&rel).copied().unwrap_or(0);
        let (list, fresh) =
            client.wait_for_diagnostics(&full, after, Duration::from_secs(20));
        self.epochs.insert(rel.clone(), client.diagnostic_epoch(&full));

        if list.is_empty() {
            return Ok(if fresh {
                format!("{rel}: no problems reported by {}.", client.spec().name)
            } else {
                format!(
                    "{rel}: no problems reported by {}, but it did not answer within 20s \
                     — it may still be indexing, so treat this as inconclusive.",
                    client.spec().name
                )
            });
        }
        let lines: Vec<String> = list.iter().take(60).map(|d| format!("  {}", d.line())).collect();
        Ok(format!("{rel}:\n{}", lines.join("\n")))
    }

    /// Go-to-definition or find-references for a symbol named on a line.
    ///
    /// Positions are asked for the way a model can actually supply them —
    /// a line number it just read and the symbol's text — rather than a
    /// column it would have to count out by hand.
    fn locate(&mut self, input: &serde_json::Value, references: bool) -> Result<String, String> {
        let lsp = self.lsp.clone().ok_or("no language server support in this run")?;
        let path = self.arg_str(input, "path")?;
        let symbol = self.arg_str(input, "symbol")?;
        let line = input
            .get("line")
            .and_then(|l| l.as_u64())
            .ok_or("Missing required integer argument \"line\".")?;

        let rel = self.resolve_readable(&path)?;
        let full = self.root.join(&rel);
        let text = self.current_content(&rel)?;

        let line_index = line.saturating_sub(1) as usize;
        let line_text = text.lines().nth(line_index).ok_or_else(|| {
            format!("{rel} has {} lines; there is no line {line}.", text.lines().count())
        })?;
        let column = line_text.find(&symbol).ok_or_else(|| {
            format!("\"{symbol}\" does not appear on line {line} of {rel}: {line_text:?}")
        })?;
        let position = crate::lsp::protocol::Position::new(
            line_index as u32,
            crate::lsp::protocol::byte_to_utf16(line_text, column),
        );

        let client = lsp.ensure_for(&full).map_err(|e| format!("{rel}: {e}"))?;
        if !client.is_open(&full) {
            client.did_open(&full, &text)?;
        }
        let locations = if references {
            client.references(&full, position)?
        } else {
            client.definition(&full, position)?
        };
        if locations.is_empty() {
            return Ok(format!(
                "The language server found no {} for {symbol}.",
                if references { "references" } else { "definition" }
            ));
        }
        Ok(self.format_locations(&locations))
    }

    fn find_symbol(&mut self, input: &serde_json::Value) -> Result<String, String> {
        let lsp = self.lsp.clone().ok_or("no language server support in this run")?;
        let query = self.arg_str(input, "query")?;
        // Any open server can answer; prefer one that already has a file
        // from this workspace open.
        let client = lsp
            .running()
            .into_iter()
            .find(|c| c.alive())
            .ok_or("no language server is running yet; read a source file first")?;
        let matches = client.workspace_symbol_names(&query)?;
        if matches.is_empty() {
            return Ok(format!("No workspace symbol matches {query}."));
        }
        let lines: Vec<String> = matches
            .iter()
            .take(40)
            .filter_map(|(name, location)| {
                let path = crate::lsp::protocol::uri_to_path(&location.uri)?;
                let rel = path.strip_prefix(&self.root).unwrap_or(&path).display();
                Some(format!("{name} — {rel}:{}", location.range.start.line + 1))
            })
            .collect();
        Ok(lines.join("\n"))
    }

    fn format_locations(&self, locations: &[crate::lsp::protocol::Location]) -> String {
        let mut lines = Vec::new();
        for location in locations.iter().take(60) {
            let Some(path) = crate::lsp::protocol::uri_to_path(&location.uri) else { continue };
            let rel = path.strip_prefix(&self.root).unwrap_or(&path).display().to_string();
            let line_number = location.range.start.line + 1;
            // Quote the line itself: a list of file:line without the code is
            // just another round of read_file calls.
            let text = self
                .current_content(&rel)
                .ok()
                .and_then(|content| {
                    content.lines().nth(location.range.start.line as usize).map(str::to_string)
                })
                .unwrap_or_default();
            lines.push(format!("{rel}:{line_number}: {}", text.trim()));
        }
        lines.join("\n")
    }

    // -- checks -------------------------------------------------------------

    /// Runs one of the repository's declared checks.
    fn run_check(&mut self, input: &serde_json::Value) -> Result<String, String> {
        /// Enough output to diagnose a failure without burying the context.
        const MAX_OUTPUT: usize = 8_000;

        if self.check_runs >= MAX_CHECK_RUNS {
            return Err(format!(
                "Check budget spent ({MAX_CHECK_RUNS} runs). Finish with what you know."
            ));
        }
        let name = self.arg_str(input, "name")?;
        let job = self
            .checks
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(name.trim()))
            .cloned()
            .ok_or_else(|| {
                format!(
                    "No check named \"{name}\". Available: {}",
                    self.checks
                        .iter()
                        .map(|c| c.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;

        if self.write_mode != WriteMode::Live {
            return Err(
                "This run proposes changes without writing them, so a check would test the \
                 old code. Report what you changed instead."
                    .into(),
            );
        }

        self.check_runs += 1;
        self.edited_since_check = false;
        let result = crate::local_ci::run_job(&self.root, &job);
        let mut output = result.output;
        if output.len() > MAX_OUTPUT {
            // Failures print the useful part last, so keep the tail.
            let start = output.len() - MAX_OUTPUT;
            let start = (start..output.len())
                .find(|i| output.is_char_boundary(*i))
                .unwrap_or(output.len());
            output = format!("[earlier output trimmed]\n{}", &output[start..]);
        }
        Ok(format!(
            "{} {} in {:.1}s\n{output}",
            result.name,
            if result.ok { "PASSED" } else { "FAILED" },
            result.duration_secs
        ))
    }

    // -- paths and content --------------------------------------------------

    fn arg_str(&self, input: &serde_json::Value, key: &str) -> Result<String, String> {
        input
            .get(key)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| format!("Missing required string argument \"{key}\"."))
    }

    /// Tracked files plus anything the model has proposed, deduplicated.
    fn visible_paths(&self) -> Vec<&String> {
        let mut paths: Vec<&String> = self.tracked.iter().collect();
        for path in self.overlay.keys() {
            if !self.tracked.contains(path) {
                paths.push(path);
            }
        }
        paths.sort();
        paths
    }

    /// Normalizes a model-supplied path and refuses anything that leaves the
    /// repository or reaches into `.git`.
    fn normalize(&self, path: &str) -> Result<String, String> {
        safe_relative(&self.root, path)
    }

    /// A path the model may read: tracked, or one it has proposed itself.
    fn resolve_readable(&self, path: &str) -> Result<String, String> {
        let rel = self.normalize(path)?;
        if self.tracked.contains(&rel) || self.overlay.contains_key(&rel) {
            return Ok(rel);
        }
        Err(format!(
            "{rel} is not a tracked file in this repository. Only files git tracks are \
             readable; use list_files or search to find the right path."
        ))
    }

    /// A path the model may propose changes to: anywhere in the worktree,
    /// since a conflict can require touching a file git has never seen.
    fn resolve_writable(&self, path: &str) -> Result<String, String> {
        self.normalize(path)
    }

    /// Current content: the model's own proposal if it has one, else disk.
    fn current_content(&self, rel: &str) -> Result<String, String> {
        if let Some(proposed) = self.overlay.get(rel) {
            return Ok(proposed.clone());
        }
        let full = self.root.join(rel);
        match std::fs::read(&full) {
            Ok(bytes) => String::from_utf8(bytes)
                .map_err(|_| format!("{rel} is not a text file.")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(format!("{rel} does not exist."))
            }
            Err(e) => Err(format!("{rel}: {e}")),
        }
    }

    /// Snapshots a file's pre-run content the first time it is proposed, so
    /// the confirmation UI can diff against what the user actually has.
    fn remember_original(&mut self, rel: &str) {
        if self.originals.contains_key(rel) {
            return;
        }
        let disk = std::fs::read_to_string(self.root.join(rel)).ok();
        self.originals.insert(rel.to_string(), disk);
    }
}

/// Normalizes a repo-relative path, refusing anything that leaves the
/// repository or reaches into `.git`.
///
/// Free-standing on purpose: the harness checks a path when the model names
/// it, and the code that finally writes an accepted proposal checks it again
/// on the way to disk. One of those is the sandbox; the other is the last
/// line before a real write.
pub fn safe_relative(root: &Path, path: &str) -> Result<String, String> {
    let trimmed = path.trim().trim_start_matches("./");
    if trimmed.is_empty() {
        return Err("path must not be empty.".into());
    }
    let candidate = Path::new(trimmed);
    if candidate.is_absolute() {
        return Err(format!(
            "{path}: use a path relative to the repository root, not an absolute path."
        ));
    }
    let mut parts: Vec<String> = Vec::new();
    for component in candidate.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().to_string()),
            Component::CurDir => {}
            _ => return Err(format!("{path}: resolves outside the repository.")),
        }
    }
    if parts.first().is_some_and(|p| p == ".git") {
        return Err(format!("{path}: the .git directory is off limits."));
    }
    let rel = parts.join("/");
    contained(root, &root.join(&rel)).map_err(|_| {
        format!("{path}: resolves outside the repository.")
    })?;
    Ok(rel)
}

/// The absolute path of `rel` under `root`, checked the same way.
pub fn safe_join(root: &Path, rel: &str) -> Result<PathBuf, String> {
    Ok(root.join(safe_relative(root, rel)?))
}

/// Whether `joined` really lands inside `root` once symlinks are resolved.
///
/// Rejecting `..` is not enough: any component of the path can be a symlink
/// pointing anywhere on the machine. Canonicalizing `joined` only works when
/// it already exists, and the dangerous case is a file that does *not* —
/// writing it follows the link and lands outside. So this walks up to the
/// nearest ancestor that does exist and canonicalizes that instead.
fn contained(root: &Path, joined: &Path) -> Result<(), ()> {
    // The comparison is between two resolved paths, so the root has to be
    // resolved too. A repository reached through a symlinked parent — a
    // symlinked home directory, `/tmp` on macOS — would otherwise fail
    // every containment check and refuse writes it should allow.
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut probe = joined.to_path_buf();
    loop {
        if let Ok(real) = probe.canonicalize() {
            return if real.starts_with(&root) { Ok(()) } else { Err(()) };
        }
        match probe.parent() {
            Some(parent) => probe = parent.to_path_buf(),
            // Walked past the filesystem root without finding anything real.
            None => return Err(()),
        }
    }
}

/// A one-line description of a tool call, for the progress log.
pub fn summarize(call: &ToolCall) -> String {
    let arg = |key: &str| call.input.get(key).and_then(|v| v.as_str()).unwrap_or("?");
    match call.name.as_str() {
        "update_plan" => "update the plan".to_string(),
        "list_files" => match call.input.get("glob").and_then(|v| v.as_str()) {
            Some(glob) => format!("list {glob}"),
            None => "list files".into(),
        },
        "read_file" => format!("read {}", arg("path")),
        "search" => format!("search \"{}\"", arg("query")),
        "write_file" => format!("propose new content for {}", arg("path")),
        "edit_file" => format!("propose an edit to {}", arg("path")),
        "replace_lines" => format!(
            "replace lines {}-{} of {}",
            call.input.get("start_line").and_then(|v| v.as_u64()).unwrap_or(0),
            call.input.get("end_line").and_then(|v| v.as_u64()).unwrap_or(0),
            arg("path")
        ),
        "show_changes" => "review the diff so far".to_string(),
        "diagnostics" => match call.input.get("path").and_then(|v| v.as_str()) {
            Some(path) => format!("diagnostics for {path}"),
            None => "diagnostics for every open file".into(),
        },
        "definition" => format!("definition of {}", arg("symbol")),
        "references" => format!("references to {}", arg("symbol")),
        "find_symbol" => format!("find symbol {}", arg("query")),
        "run_check" => format!("run check {}", arg("name")),
        other => other.to_string(),
    }
}

/// A unique match of `old` in `current` ignoring trailing whitespace on each
/// line and CRLF line endings: the byte range in `current` to replace. `Ok(None)`
/// when there is none, `Err(n)` when there are several.
fn flexible_match(current: &str, old: &str) -> Result<Option<(usize, usize)>, usize> {
    let wanted: Vec<&str> = old.lines().map(str::trim_end).collect();
    if wanted.is_empty() || wanted.iter().all(|l| l.is_empty()) {
        return Ok(None);
    }
    // Byte offsets of each line start in `current`, so a match of lines can
    // be turned back into a range of bytes.
    let mut starts = Vec::new();
    let mut offset = 0;
    for line in current.split_inclusive('\n') {
        starts.push(offset);
        offset += line.len();
    }
    let lines: Vec<&str> = current.lines().map(str::trim_end).collect();
    let mut found: Vec<(usize, usize)> = Vec::new();
    for i in 0..lines.len() {
        if i + wanted.len() > lines.len() {
            break;
        }
        if lines[i..i + wanted.len()] == wanted[..] {
            let start = starts[i];
            let last = i + wanted.len() - 1;
            // The end of the last matched line, without its line ending.
            let end = starts[last] + current[starts[last]..].lines().next().unwrap_or("").len();
            found.push((start, end));
        }
    }
    match found.len() {
        0 => Ok(None),
        1 => Ok(Some(found[0])),
        n => Err(n),
    }
}

/// The one region of `current` that is nearly `old`, if there is exactly
/// one: its byte range and how similar it is.
///
/// "Nearly" is a normalized edit distance over the same number of lines as
/// `old`, compared with trailing whitespace stripped. The bar is high on
/// purpose — an edit landing on the wrong region is the one outcome worse
/// than a failed edit — and a second candidate above the bar means no match
/// at all rather than a guess between them.
fn approximate_match(current: &str, old: &str) -> Option<(usize, usize, f64)> {
    /// Below this similarity the snippet is not the same code with a typo,
    /// it is different code.
    const MIN_SCORE: f64 = 0.9;
    /// Files past this are searched no further: the scan is quadratic in the
    /// snippet and linear in the file, and the model can use line numbers.
    const MAX_LINES: usize = 20_000;

    let wanted: Vec<&str> = old.lines().map(str::trim_end).collect();
    if wanted.is_empty() || wanted.iter().all(|l| l.trim().is_empty()) {
        return None;
    }
    let target = wanted.join("\n");
    if target.len() < 12 {
        // Too short to be nearly anything in particular.
        return None;
    }
    let mut starts = Vec::new();
    let mut offset = 0;
    for line in current.split_inclusive('\n') {
        starts.push(offset);
        offset += line.len();
    }
    let lines: Vec<&str> = current.lines().map(str::trim_end).collect();
    if lines.len() > MAX_LINES || lines.len() < wanted.len() {
        return None;
    }
    let mut best: Option<(usize, f64)> = None;
    let mut runner_up = 0.0_f64;
    for i in 0..=lines.len() - wanted.len() {
        let window = lines[i..i + wanted.len()].join("\n");
        // Cheap pre-filter: lengths within 20% of each other.
        let (a, b) = (window.len() as f64, target.len() as f64);
        if (a - b).abs() > 0.2 * a.max(b) {
            continue;
        }
        let score = 1.0 - levenshtein(&window, &target) as f64 / a.max(b).max(1.0);
        match best {
            Some((_, s)) if score <= s => runner_up = runner_up.max(score),
            Some((_, s)) => {
                runner_up = runner_up.max(s);
                best = Some((i, score));
            }
            None => best = Some((i, score)),
        }
    }
    let (i, score) = best?;
    if score < MIN_SCORE || runner_up >= MIN_SCORE {
        return None;
    }
    let start = starts[i];
    let last = i + wanted.len() - 1;
    let end = starts[last] + current[starts[last]..].lines().next().unwrap_or("").len();
    Some((start, end, score))
}

/// Edit distance in bytes, single-row dynamic programming.
fn levenshtein(a: &str, b: &str) -> usize {
    let a = a.as_bytes();
    let b = b.as_bytes();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, &ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(cur[j] + 1);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// Where the first line of a failed `old_text` does occur, so the model can
/// re-read the right place instead of the whole file.
fn nearest_hint(current: &str, old: &str) -> String {
    let Some(probe) = old.lines().map(str::trim).find(|l| l.len() >= 8) else {
        return String::new();
    };
    let lines: Vec<&str> = current.lines().collect();
    let hits: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.contains(probe))
        .map(|(i, _)| i)
        .take(3)
        .collect();
    if hits.is_empty() {
        return String::new();
    }
    // Quote the region the model was probably aiming at, verbatim and
    // numbered, so it can copy from here or switch to replace_lines.
    let span = old.lines().count().max(1);
    let first = hits[0];
    let to = (first + span + 1).min(lines.len());
    let region: Vec<String> =
        lines[first..to].iter().enumerate().map(|(i, l)| format!("{:>6}  {l}", first + i + 1)).collect();
    let others = if hits.len() > 1 {
        format!(
            " (its first line also appears at line(s) {})",
            hits[1..].iter().map(|i| (i + 1).to_string()).collect::<Vec<_>>().join(", ")
        )
    } else {
        String::new()
    };
    format!(" The file actually says{others}:\n{}\n", region.join("\n"))
}

/// Glob match over a whole repo-relative path. `*` matches any run of
/// characters including `/`, `?` matches exactly one. Deliberately simple:
/// the model gets told what the syntax is, and a near-miss just returns
/// fewer files rather than the wrong ones.
fn glob_match(pattern: &str, text: &str) -> bool {
    fn walk(p: &[char], t: &[char]) -> bool {
        match p.first() {
            None => t.is_empty(),
            Some('*') => walk(&p[1..], t) || (!t.is_empty() && walk(p, &t[1..])),
            Some('?') => !t.is_empty() && walk(&p[1..], &t[1..]),
            Some(c) => t.first() == Some(c) && walk(&p[1..], &t[1..]),
        }
    }
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    walk(&p, &t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, input: serde_json::Value) -> ToolCall {
        ToolCall { id: "1".into(), name: name.into(), input }
    }

    fn fixture(access: Access) -> (tempfile::TempDir, Workspace) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/main.rs"), "fn main() {\n    run();\n}\n").unwrap();
        std::fs::write(tmp.path().join("src/lib.rs"), "pub fn run() {}\n").unwrap();
        std::fs::write(tmp.path().join(".env"), "SECRET=hunter2\n").unwrap();
        let ws = Workspace::new(
            tmp.path(),
            vec!["src/main.rs".into(), "src/lib.rs".into()],
            access,
        )
        .unwrap();
        (tmp, ws)
    }

    #[test]
    fn reads_a_tracked_file_with_line_numbers() {
        let (_tmp, mut ws) = fixture(Access::ReadOnly);
        let out = ws.dispatch(&call("read_file", serde_json::json!({"path": "src/main.rs"})), 10);
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("1  fn main() {"), "{}", out.content);
    }

    #[test]
    fn untracked_files_are_invisible() {
        let (_tmp, mut ws) = fixture(Access::ReadOnly);
        let out = ws.dispatch(&call("read_file", serde_json::json!({"path": ".env"})), 10);
        assert!(out.is_error);
        assert!(!out.content.contains("hunter2"));
        let listed = ws.dispatch(&call("list_files", serde_json::json!({})), 10);
        assert!(!listed.content.contains(".env"));
    }

    #[test]
    fn refuses_paths_that_leave_the_repository() {
        let (_tmp, mut ws) = fixture(Access::ReadWrite);
        for path in ["../outside.txt", "/etc/passwd", ".git/config"] {
            let out = ws.dispatch(&call("read_file", serde_json::json!({"path": path})), 10);
            assert!(out.is_error, "{path} was allowed: {}", out.content);
        }
        let out = ws.dispatch(
            &call("write_file", serde_json::json!({"path": "../evil.rs", "content": "x"})),
            10,
        );
        assert!(out.is_error, "{}", out.content);
        assert!(ws.edits().is_empty());
    }

    #[test]
    fn read_only_access_has_no_edit_tools() {
        let (_tmp, mut ws) = fixture(Access::ReadOnly);
        let names: Vec<&str> = ws.tools().iter().map(|t| t.name).collect();
        // update_plan is always available: saying what you intend to do is
        // not a write.
        assert_eq!(names, vec!["list_files", "read_file", "search", "update_plan"]);
        let out = ws.dispatch(
            &call("write_file", serde_json::json!({"path": "src/lib.rs", "content": "x"})),
            10,
        );
        assert!(out.is_error);
        assert!(ws.edits().is_empty());
    }

    #[test]
    fn edits_accumulate_in_the_overlay_and_reads_see_them() {
        let (tmp, mut ws) = fixture(Access::ReadWrite);
        let out = ws.dispatch(
            &call(
                "edit_file",
                serde_json::json!({"path": "src/lib.rs", "old_text": "pub fn run() {}", "new_text": "pub fn run() { work() }"}),
            ),
            10,
        );
        assert!(!out.is_error, "{}", out.content);
        let read = ws.dispatch(&call("read_file", serde_json::json!({"path": "src/lib.rs"})), 10);
        assert!(read.content.contains("work()"), "{}", read.content);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("src/lib.rs")).unwrap(),
            "pub fn run() {}\n",
            "the overlay must not touch disk"
        );
        let edits = ws.edits();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].path, "src/lib.rs");
        assert_eq!(edits[0].before.as_deref(), Some("pub fn run() {}\n"));
    }

    #[test]
    fn ambiguous_edits_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "x = 1;\nx = 1;\n").unwrap();
        let mut ws =
            Workspace::new(tmp.path(), vec!["a.rs".into()], Access::ReadWrite).unwrap();
        let out = ws.dispatch(
            &call("edit_file", serde_json::json!({"path": "a.rs", "old_text": "x = 1;", "new_text": "x = 2;"})),
            10,
        );
        assert!(out.is_error);
        assert!(out.content.contains("appears 2 times"), "{}", out.content);

        let out = ws.dispatch(
            &call("edit_file", serde_json::json!({"path": "a.rs", "old_text": "x = 1;", "new_text": "x = 2;", "replace_all": true})),
            10,
        );
        assert!(!out.is_error, "{}", out.content);
        assert_eq!(ws.edits()[0].after, "x = 2;\nx = 2;\n");
    }

    #[test]
    fn an_edit_that_differs_only_in_trailing_whitespace_still_applies() {
        let (_tmp, mut ws) = fixture(Access::ReadWrite);
        // The file has trailing spaces the model did not see.
        ws.dispatch(
            &call(
                "write_file",
                serde_json::json!({"path": "src/main.rs", "content": "fn main() {   \n    run();\n}\n"}),
            ),
            10,
        );
        let out = ws.dispatch(
            &call(
                "edit_file",
                serde_json::json!({"path": "src/main.rs", "old_text": "fn main() {\n    run();", "new_text": "fn main() {\n    go();"}),
            ),
            10,
        );
        assert!(!out.is_error, "{}", out.content);
        let after = ws.edits().into_iter().find(|e| e.path == "src/main.rs").unwrap().after;
        assert_eq!(after, "fn main() {\n    go();\n}\n");
    }

    #[test]
    fn a_nearly_matching_edit_is_applied_and_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let body = "fn keep() {}\n\nfn go() {\n    while value.ends_with('\\\\') {\n        value.pop();\n    }\n}\n";
        std::fs::write(tmp.path().join("a.rs"), body).unwrap();
        let mut ws = Workspace::new(tmp.path(), vec!["a.rs".into()], Access::ReadWrite).unwrap();
        // The model lost a backslash: one character off over four lines.
        let out = ws.dispatch(
            &call(
                "edit_file",
                serde_json::json!({
                    "path": "a.rs",
                    "old_text": "    while value.ends_with('\\') {\n        value.pop();\n    }",
                    "new_text": "    while value.ends_with('\\\\') {\n        value.pop();\n        value.push(' ');\n    }"
                }),
            ),
            10,
        );
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("similar to lines 4-6"), "{}", out.content);
        assert!(out.content.contains("value.push"), "{}", out.content);
        let after = ws.edits()[0].after.clone();
        assert!(after.contains("value.push(' ');"), "{after}");
        assert!(after.starts_with("fn keep() {}"), "the rest is untouched");

        // Something genuinely different is still refused.
        let out = ws.dispatch(
            &call(
                "edit_file",
                serde_json::json!({"path": "a.rs", "old_text": "fn something_else() {\n    entirely();\n}", "new_text": "x"}),
            ),
            10,
        );
        assert!(out.is_error, "{}", out.content);
    }

    #[test]
    fn a_failed_edit_quotes_the_region_it_was_aiming_at() {
        let (_tmp, mut ws) = fixture(Access::ReadWrite);
        let out = ws.dispatch(
            &call(
                "edit_file",
                serde_json::json!({"path": "src/main.rs", "old_text": "fn main() {\n    stop();\n}", "new_text": "x"}),
            ),
            10,
        );
        assert!(out.is_error);
        assert!(out.content.contains("     1  fn main() {"), "{}", out.content);
        assert!(out.content.contains("     2      run();"), "{}", out.content);
        assert!(out.content.contains("replace_lines"), "{}", out.content);
    }

    #[test]
    fn replace_lines_edits_by_number_and_echoes_the_result() {
        let (_tmp, mut ws) = fixture(Access::ReadWrite);
        let out = ws.dispatch(
            &call(
                "replace_lines",
                serde_json::json!({"path": "src/main.rs", "start_line": 2, "end_line": 2, "new_text": "    go();\n    go();", "expect_first_line": "run();"}),
            ),
            10,
        );
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("Replaced lines 2-2"), "{}", out.content);
        assert!(out.content.contains("     3      go();"), "{}", out.content);
        let after = ws.edits().into_iter().find(|e| e.path == "src/main.rs").unwrap().after;
        assert_eq!(after, "fn main() {\n    go();\n    go();\n}\n");

        // A stale guard is refused, and the range is checked.
        let stale = ws.dispatch(
            &call(
                "replace_lines",
                serde_json::json!({"path": "src/main.rs", "start_line": 2, "end_line": 2, "new_text": "x", "expect_first_line": "run();"}),
            ),
            10,
        );
        assert!(stale.is_error && stale.content.contains("not"), "{}", stale.content);
        let past = ws.dispatch(
            &call(
                "replace_lines",
                serde_json::json!({"path": "src/main.rs", "start_line": 3, "end_line": 9, "new_text": "x"}),
            ),
            10,
        );
        assert!(past.is_error, "{}", past.content);
        // Deleting a range.
        let del = ws.dispatch(
            &call(
                "replace_lines",
                serde_json::json!({"path": "src/main.rs", "start_line": 2, "end_line": 3, "new_text": ""}),
            ),
            10,
        );
        assert!(!del.is_error, "{}", del.content);
        let after = ws.edits().into_iter().find(|e| e.path == "src/main.rs").unwrap().after;
        assert_eq!(after, "fn main() {\n}\n");
    }

    #[test]
    fn search_takes_a_regex_and_context() {
        let (_tmp, mut ws) = fixture(Access::ReadOnly);
        let out = ws.dispatch(
            &call("search", serde_json::json!({"query": "fn (main|run)", "regex": true})),
            10,
        );
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("src/main.rs:1:"), "{}", out.content);
        assert!(out.content.contains("src/lib.rs:1:"), "{}", out.content);

        let bad = ws.dispatch(&call("search", serde_json::json!({"query": "(", "regex": true})), 10);
        assert!(bad.is_error && bad.content.contains("regex"), "{}", bad.content);

        let ctx = ws.dispatch(
            &call("search", serde_json::json!({"query": "run();", "context": 1})),
            10,
        );
        assert!(ctx.content.contains("src/main.rs-1- fn main() {"), "{}", ctx.content);
        assert!(ctx.content.contains("src/main.rs:2:     run();"), "{}", ctx.content);
    }

    #[test]
    fn show_changes_renders_the_run_diff() {
        let (_tmp, mut ws) = fixture(Access::ReadWrite);
        let none = ws.dispatch(&call("show_changes", serde_json::json!({})), 10);
        assert!(none.content.contains("No changes yet"));
        ws.dispatch(
            &call(
                "edit_file",
                serde_json::json!({"path": "src/lib.rs", "old_text": "pub fn run() {}", "new_text": "pub fn run() { work() }"}),
            ),
            10,
        );
        let out = ws.dispatch(&call("show_changes", serde_json::json!({})), 10);
        assert!(out.content.contains("--- src/lib.rs"), "{}", out.content);
        assert!(out.content.contains("-pub fn run() {}"), "{}", out.content);
        assert!(out.content.contains("+pub fn run() { work() }"), "{}", out.content);
    }

    #[test]
    fn the_overview_describes_the_layout() {
        let (_tmp, ws) = fixture(Access::ReadOnly);
        let text = ws.overview();
        assert!(text.starts_with("2 tracked file(s)."), "{text}");
        assert!(text.contains("src/ (2)"), "{text}");
        assert!(text.contains(".rs 2"), "{text}");
    }

    #[test]
    fn project_instructions_come_from_a_tracked_agents_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("AGENTS.md"), "Run cargo test.\n").unwrap();
        std::fs::write(tmp.path().join("a.rs"), "").unwrap();
        let ws = Workspace::new(tmp.path(), vec!["a.rs".into()], Access::ReadOnly).unwrap();
        assert!(ws.project_instructions().is_none(), "untracked files are invisible");
        let ws = Workspace::new(tmp.path(), vec!["a.rs".into(), "AGENTS.md".into()], Access::ReadOnly)
            .unwrap();
        let (name, text) = ws.project_instructions().unwrap();
        assert_eq!(name, "AGENTS.md");
        assert_eq!(text, "Run cargo test.\n");
    }

    #[test]
    fn the_verification_gap_tracks_checks_and_diagnostics() {
        let (_tmp, mut ws) = fixture(Access::ReadWrite);
        assert!(ws.verification_gap().is_none(), "nothing changed yet");
        ws.dispatch(
            &call("write_file", serde_json::json!({"path": "src/lib.rs", "content": "pub fn run() { 1 }\n"})),
            10,
        );
        assert!(ws.verification_gap().is_none(), "no way to verify in an overlay run without a server");

        let (tmp, mut ws) = fixture(Access::ReadWrite);
        ws = ws.with_write_mode(WriteMode::Live).with_checks(vec![crate::local_ci::Job {
            name: "tests".into(),
            commands: vec!["true".into()],
            ..Default::default()
        }]);
        ws.dispatch(
            &call("write_file", serde_json::json!({"path": "src/lib.rs", "content": "pub fn run() { 1 }\n"})),
            10,
        );
        let gap = ws.verification_gap().expect("an unchecked live edit is a gap");
        assert!(gap.contains("run_check"), "{gap}");
        let out = ws.dispatch(&call("run_check", serde_json::json!({"name": "tests"})), 10);
        assert!(!out.is_error, "{}", out.content);
        assert!(ws.verification_gap().is_none(), "checked since the last edit");
        drop(tmp);
    }

    #[test]
    fn rewriting_a_file_to_itself_is_not_a_change() {
        let (_tmp, mut ws) = fixture(Access::ReadWrite);
        ws.dispatch(
            &call("write_file", serde_json::json!({"path": "src/lib.rs", "content": "pub fn run() {}\n"})),
            10,
        );
        assert!(ws.edits().is_empty());
    }

    #[test]
    fn search_reports_path_and_line() {
        let (_tmp, mut ws) = fixture(Access::ReadOnly);
        let out = ws.dispatch(&call("search", serde_json::json!({"query": "run"})), 10);
        assert!(out.content.contains("src/main.rs:2:"), "{}", out.content);
        assert!(out.content.contains("src/lib.rs:1:"), "{}", out.content);
    }

    #[test]
    fn tool_budget_stops_dispatch() {
        let (_tmp, mut ws) = fixture(Access::ReadOnly);
        let c = call("list_files", serde_json::json!({}));
        assert!(!ws.dispatch(&c, 1).is_error);
        let out = ws.dispatch(&c, 1);
        assert!(out.is_error);
        assert!(out.content.contains("budget"));
    }

    /// A symlink inside the repository must not be a way out of it.
    ///
    /// The dangerous case is a path that does not exist yet: canonicalize
    /// fails on it, so a check that only looks at fully-resolved paths never
    /// runs, and the write follows the link.
    #[test]
    fn a_symlink_is_not_a_way_out_of_the_repository() {
        let outside = tempfile::tempdir().unwrap();
        let (tmp, mut ws) = fixture(Access::ReadWrite);
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("escape")).unwrap();

        // Writing *through* the link to a file that does not exist yet.
        let out = ws.dispatch(
            &call(
                "write_file",
                serde_json::json!({"path": "escape/pwned.txt", "content": "owned"}),
            ),
            10,
        );
        assert!(out.is_error, "a write through a symlink escaped: {}", out.content);
        assert!(
            !outside.path().join("pwned.txt").exists(),
            "the write landed outside the repository"
        );

        // And reading through it, for a file that does exist out there.
        std::fs::write(outside.path().join("secret.txt"), "s3cret").unwrap();
        let out = ws.dispatch(
            &call("read_file", serde_json::json!({"path": "escape/secret.txt"})),
            10,
        );
        assert!(out.is_error, "a read escaped the repository: {}", out.content);
        assert!(!out.content.contains("s3cret"));
    }

    /// A repository reached through a symlinked path is still that
    /// repository. `/tmp` on macOS and a symlinked home directory are both
    /// this case, and a containment check that compares a resolved path
    /// against an unresolved root refuses every write in them.
    #[test]
    #[cfg(unix)]
    fn a_symlinked_repository_root_still_accepts_its_own_files() {
        let outer = tempfile::tempdir().unwrap();
        let real = outer.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("a.txt"), "hello\n").unwrap();
        let link = outer.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        // Callers hand us whatever path they were given, symlink and all.
        let resolved = safe_join(&link, "a.txt").expect("a file in the repository");
        assert!(resolved.ends_with("a.txt"));
        // A file that does not exist yet is fine too — that is how a new
        // file gets written.
        assert!(safe_join(&link, "sub/new.txt").is_ok());

        // Escaping is still refused, symlinked root or not.
        assert!(safe_relative(&link, "../outside.txt").is_err());
        assert!(safe_relative(&link, "/etc/passwd").is_err());
        assert!(safe_relative(&link, ".git/config").is_err());
    }

    #[test]
    fn a_live_run_writes_to_disk_and_can_be_reverted() {
        let (tmp, ws) = fixture(Access::ReadWrite);
        let mut ws = ws.with_write_mode(WriteMode::Live);
        let out = ws.dispatch(
            &call("write_file", serde_json::json!({"path": "src/lib.rs", "content": "pub fn run() { work() }\n"})),
            10,
        );
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("on disk"), "{}", out.content);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("src/lib.rs")).unwrap(),
            "pub fn run() { work() }\n"
        );

        // A new file the run created is removed by a revert, not left behind.
        ws.dispatch(
            &call("write_file", serde_json::json!({"path": "src/new.rs", "content": "fn x() {}\n"})),
            10,
        );
        assert!(tmp.path().join("src/new.rs").exists());

        ws.revert_all().unwrap();
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("src/lib.rs")).unwrap(),
            "pub fn run() {}\n"
        );
        assert!(!tmp.path().join("src/new.rs").exists());
    }

    #[test]
    fn checks_are_offered_only_when_the_repository_declares_them() {
        let (_tmp, ws) = fixture(Access::ReadWrite);
        assert!(!ws.tools().iter().any(|t| t.name == "run_check"));

        let job = crate::local_ci::Job {
            name: "tests".into(),
            commands: vec!["true".into()],
            ..Default::default()
        };
        let ws = ws.with_checks(vec![job]);
        let spec = ws.tools().into_iter().find(|t| t.name == "run_check").unwrap();
        // The available names go in the description, so the model does not
        // have to guess what it may run.
        assert!(spec.description.contains("tests"), "{}", spec.description);
    }

    #[test]
    fn a_check_is_refused_when_the_edits_are_not_on_disk() {
        let (_tmp, ws) = fixture(Access::ReadWrite);
        let job = crate::local_ci::Job {
            name: "tests".into(),
            commands: vec!["true".into()],
            ..Default::default()
        };
        let mut ws = ws.with_checks(vec![job]);
        let out = ws.dispatch(&call("run_check", serde_json::json!({"name": "tests"})), 10);
        assert!(out.is_error);
        assert!(out.content.contains("without writing them"), "{}", out.content);
    }

    #[test]
    fn only_declared_checks_can_run() {
        let (tmp, ws) = fixture(Access::ReadWrite);
        let job = crate::local_ci::Job {
            name: "tests".into(),
            commands: vec!["echo ran-the-real-check".into()],
            ..Default::default()
        };
        let mut ws = ws.with_checks(vec![job]).with_write_mode(WriteMode::Live);

        let out = ws.dispatch(
            &call("run_check", serde_json::json!({"name": "rm -rf /"})),
            10,
        );
        assert!(out.is_error);
        assert!(out.content.contains("No check named"), "{}", out.content);

        let out = ws.dispatch(&call("run_check", serde_json::json!({"name": "tests"})), 10);
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("PASSED"), "{}", out.content);
        assert!(out.content.contains("ran-the-real-check"), "{}", out.content);
        let _ = tmp;
    }

    #[test]
    fn language_tools_appear_only_with_a_manager() {
        let (tmp, ws) = fixture(Access::ReadOnly);
        assert!(!ws.tools().iter().any(|t| t.name == "diagnostics"));

        let manager = std::sync::Arc::new(crate::lsp::Manager::new(tmp.path(), None));
        let ws = ws.with_language_support(manager);
        let names: Vec<&str> = ws.tools().iter().map(|t| t.name).collect();
        for expected in ["diagnostics", "definition", "references", "find_symbol"] {
            assert!(names.contains(&expected), "{expected} missing from {names:?}");
        }
    }

    #[test]
    fn the_plan_is_recorded_and_ticked_off() {
        let (_tmp, mut ws) = fixture(Access::ReadOnly);
        assert!(ws.plan().is_empty());

        let out = ws.dispatch(
            &call(
                "update_plan",
                serde_json::json!({"steps": [
                    {"text": "read the parser", "done": true},
                    {"text": "fix the off-by-one", "done": false}
                ]}),
            ),
            10,
        );
        assert!(!out.is_error, "{}", out.content);
        assert!(out.content.contains("1/2"), "{}", out.content);

        let plan = ws.plan();
        assert_eq!(plan.len(), 2);
        assert!(plan[0].done && !plan[1].done);
        assert_eq!(plan[1].text, "fix the off-by-one");

        // Sending the list again replaces it, so ticking a box is one call.
        ws.dispatch(
            &call(
                "update_plan",
                serde_json::json!({"steps": [
                    {"text": "read the parser", "done": true},
                    {"text": "fix the off-by-one", "done": true}
                ]}),
            ),
            10,
        );
        assert!(ws.plan().iter().all(|s| s.done));

        // An empty plan is a mistake, not a plan.
        let out = ws.dispatch(&call("update_plan", serde_json::json!({"steps": []})), 10);
        assert!(out.is_error);
        assert_eq!(ws.plan().len(), 2, "the previous plan survives a bad update");
    }

    #[test]
    fn glob_matches_paths() {
        assert!(glob_match("src/*.rs", "src/main.rs"));
        assert!(glob_match("*.rs", "src/main.rs"));
        assert!(!glob_match("src/*.rs", "docs/cli.md"));
        assert!(glob_match("src/?ib.rs", "src/lib.rs"));
    }

    #[test]
    fn line_delta_counts_changed_lines() {
        let edit = PendingEdit {
            path: "a".into(),
            before: Some("a\nb\nc\n".into()),
            after: "a\nB\nB2\nc\n".into(),
        };
        assert_eq!(edit.line_delta(), (2, 1));
    }
}
