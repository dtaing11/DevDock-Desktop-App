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
use std::collections::BTreeMap;
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
    write_mode: WriteMode,
    /// Language servers, when the task is allowed to consult them.
    lsp: Option<Arc<crate::lsp::Manager>>,
    /// Diagnostic epoch per file at the moment it was last written, so
    /// `diagnostics` can wait for results about the new text.
    epochs: BTreeMap<String, u64>,
    /// The project's own checks, the only commands that may be run.
    checks: Vec<crate::local_ci::Job>,
    check_runs: usize,
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
            write_mode: WriteMode::Overlay,
            lsp: None,
            epochs: BTreeMap::new(),
            checks: Vec::new(),
            check_runs: 0,
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

    /// The tools available at this access level.
    pub fn tools(&self) -> Vec<ToolSpec> {
        let mut tools = vec![
            ToolSpec {
                name: "list_files",
                description: "List the repository's tracked files. Optionally filter with a \
                              glob such as \"src/*.rs\" or \"*/tests/*\". Use this first to \
                              learn the layout before reading anything.",
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
                description: "Case-insensitive plain-text search across tracked files. \
                              Returns path:line: matching text. Use it to find definitions, \
                              callers, and other uses of a symbol.",
                schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Literal text to find (not a regex)."},
                        "glob": {"type": "string", "description": "Optional glob to restrict which files are searched."},
                        "max_results": {"type": "integer", "description": "Cap on hits returned."}
                    },
                    "required": ["query"]
                }),
            },
        ];

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
            "list_files" => self.list_files(&call.input),
            "read_file" => self.read_file(&call.input),
            "search" => self.search(&call.input),
            "write_file" if self.access == Access::ReadWrite => self.write_file(&call.input),
            "edit_file" if self.access == Access::ReadWrite => self.edit_file(&call.input),
            "diagnostics" if self.lsp.is_some() => self.diagnostics(&call.input),
            "definition" if self.lsp.is_some() => self.locate(&call.input, false),
            "references" if self.lsp.is_some() => self.locate(&call.input, true),
            "find_symbol" if self.lsp.is_some() => self.find_symbol(&call.input),
            "run_check" if !self.checks.is_empty() => self.run_check(&call.input),
            "write_file" | "edit_file" => {
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
        let needle = query.to_lowercase();
        let glob = input.get("glob").and_then(|g| g.as_str());
        let cap = input
            .get("max_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(MAX_SEARCH_HITS as u64)
            .min(MAX_SEARCH_HITS as u64) as usize;

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
            for (i, line) in content.lines().enumerate() {
                if !line.to_lowercase().contains(&needle) {
                    continue;
                }
                if hits.len() >= cap {
                    more += 1;
                    continue;
                }
                let text = line.trim_end();
                let text: String = if text.chars().count() > 200 {
                    text.chars().take(200).collect::<String>() + "…"
                } else {
                    text.to_string()
                };
                hits.push(format!("{rel}:{}: {text}", i + 1));
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
        let mut out = hits.join("\n");
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
        Ok(format!("Wrote {rel} ({lines} lines). {}", self.write_note()))
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

        let count = current.matches(old.as_str()).count();
        if count == 0 {
            return Err(format!(
                "old_text does not appear in {rel}. Read the file again and copy the text \
                 exactly, including indentation."
            ));
        }
        if count > 1 && !all {
            return Err(format!(
                "old_text appears {count} times in {rel}. Include more surrounding context \
                 to make it unique, or pass replace_all."
            ));
        }
        let updated = if all {
            current.replace(old.as_str(), &new)
        } else {
            current.replacen(old.as_str(), &new, 1)
        };
        self.remember_original(&rel);
        self.overlay.insert(rel.clone(), updated);
        self.persist(&rel)?;
        Ok(format!(
            "Edited {rel} ({} replacement{}). {}",
            if all { count } else { 1 },
            if all && count != 1 { "s" } else { "" },
            self.write_note()
        ))
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

    // -- language server ----------------------------------------------------

    /// Diagnostics for one file (or all of them), waiting for results that
    /// reflect the most recent edit.
    fn diagnostics(&mut self, input: &serde_json::Value) -> Result<String, String> {
        let lsp = self.lsp.clone().ok_or("no language server support in this run")?;

        let Some(path) = input.get("path").and_then(|p| p.as_str()) else {
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
        /// A hard stop on a model that would otherwise run the test suite
        /// after every edit.
        const MAX_CHECK_RUNS: usize = 8;
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
                Component::Normal(part) => {
                    parts.push(part.to_string_lossy().to_string());
                }
                Component::CurDir => {}
                _ => return Err(format!("{path}: resolves outside the repository.")),
            }
        }
        let rel = parts.join("/");
        if parts.first().is_some_and(|p| p == ".git") {
            return Err(format!("{path}: the .git directory is off limits."));
        }
        // Existing paths get the symlink check too: a tracked symlink could
        // otherwise point anywhere on the machine.
        let joined = self.root.join(&rel);
        if let Ok(canonical) = joined.canonicalize() {
            if !canonical.starts_with(&self.root) {
                return Err(format!("{path}: resolves outside the repository."));
            }
        }
        Ok(rel)
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

/// A one-line description of a tool call, for the progress log.
pub fn summarize(call: &ToolCall) -> String {
    let arg = |key: &str| call.input.get(key).and_then(|v| v.as_str()).unwrap_or("?");
    match call.name.as_str() {
        "list_files" => match call.input.get("glob").and_then(|v| v.as_str()) {
            Some(glob) => format!("list {glob}"),
            None => "list files".into(),
        },
        "read_file" => format!("read {}", arg("path")),
        "search" => format!("search \"{}\"", arg("query")),
        "write_file" => format!("propose new content for {}", arg("path")),
        "edit_file" => format!("propose an edit to {}", arg("path")),
        other => other.to_string(),
    }
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
        assert_eq!(names, vec!["list_files", "read_file", "search"]);
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
