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
        })
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
        for rel in self.visible_paths() {
            if !glob.is_none_or(|g| glob_match(g, rel)) {
                continue;
            }
            let Ok(content) = self.current_content(rel) else { continue };
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
            return Ok(format!("No matches for {query}."));
        }
        let mut out = hits.join("\n");
        if more > 0 {
            out.push_str(&format!("\n\n[{more} more matches; narrow the query or glob]"));
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
        Ok(format!(
            "Proposed new content for {rel} ({lines} lines). Not written yet: the user \
             confirms every change at the end of the run."
        ))
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
        Ok(format!(
            "Proposed an edit to {rel} ({} replacement{}). Not written yet: the user \
             confirms every change at the end of the run.",
            if all { count } else { 1 },
            if all && count != 1 { "s" } else { "" }
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
