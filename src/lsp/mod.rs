//! Language server client: one child process per server, spoken to over
//! stdio with JSON-RPC.
//!
//! # Threads
//!
//! A [`Client`] owns the server process, a writer behind a mutex, and a
//! reader thread. The reader thread does three things: it completes pending
//! requests, it answers the requests servers make of *us* (rust-analyzer
//! stalls forever if `workspace/configuration` goes unanswered), and it
//! files notifications — diagnostics, log messages, progress — into shared
//! state the UI reads each frame.
//!
//! Everything in [`Client`] is `Send + Sync` and meant to be held in an
//! `Arc`: the UI thread reads diagnostics and status, worker threads make
//! requests. **Requests block**, so call them from a worker, never from a
//! render pass — `rust-analyzer` can take a minute to answer while it
//! indexes a large repository.
//!
//! # Lifetime
//!
//! Servers start lazily, on the first file that needs one ([`Manager`]), and
//! are shut down on [`Manager::shutdown_all`] or when the last `Arc` drops.
//! A server that dies is not restarted automatically: its diagnostics stop
//! updating and the status line says so, which is honest, where a silent
//! respawn loop would just burn CPU against whatever is crashing it.

pub mod protocol;
pub mod registry;

use protocol::{
    apply_edits, hover_text, path_to_uri, CompletionItem, Diagnostic, Location, Position,
    Range, Symbol, TextEdit,
};
use registry::ServerSpec;
use std::collections::HashMap;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long a request waits before giving up. Generous, because the first
/// request to a cold rust-analyzer queues behind indexing the whole project.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Cap on the server log kept in memory.
const MAX_LOG_LINES: usize = 400;

/// Cap on captured stderr. Only the tail matters: it is read to explain a
/// server that just died.
const MAX_STDERR_LINES: usize = 40;

/// In-flight requests, keyed by id, each waiting on its answer.
type Pending = Arc<Mutex<HashMap<u64, Sender<Result<serde_json::Value, String>>>>>;

/// Shared state the reader thread writes and the UI reads.
#[derive(Default)]
struct Shared {
    diagnostics: Mutex<HashMap<String, Vec<Diagnostic>>>,
    /// How many times each file's diagnostics have been republished.
    /// Diagnostics arrive whenever the server feels like it, so "are these
    /// about my latest edit?" can only be answered by watching this advance.
    epochs: Mutex<HashMap<String, u64>>,
    log: Mutex<Vec<String>>,
    /// The server's stderr, which is where a server that refuses to start
    /// says why. Without it the only symptom is a process that vanished.
    stderr: Mutex<Vec<String>>,
    /// Latest `$/progress` title, e.g. "rust-analyzer: indexing".
    status: Mutex<Option<String>>,
    alive: AtomicBool,
}

/// One running language server.
pub struct Client {
    spec: ServerSpec,
    root: PathBuf,
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    next_id: AtomicU64,
    pending: Pending,
    shared: Arc<Shared>,
    /// Server capabilities from `initialize`, to avoid asking for what it
    /// cannot do.
    capabilities: Mutex<serde_json::Value>,
    /// Open documents and their version numbers.
    docs: Mutex<HashMap<String, i64>>,
}

impl Client {
    /// Starts `spec` for `root` and completes the initialize handshake.
    ///
    /// Blocking, and slow for a cold server: call it from a worker thread.
    /// `on_event` is called whenever asynchronous state changes (new
    /// diagnostics, progress) so a GUI can repaint.
    pub fn start(
        spec: ServerSpec,
        root: &Path,
        on_event: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Result<Arc<Self>, String> {
        let mut child = Command::new(&spec.command)
            .args(&spec.args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                format!(
                    "cannot start {}: {e}. Install it, or declare a different \
                     server under [[lsp]] in {}.",
                    spec.command,
                    crate::local_ci::CONFIG_FILE
                )
            })?;

        let stdin = child.stdin.take().ok_or("language server has no stdin")?;
        let stdout = child.stdout.take().ok_or("language server has no stdout")?;
        let stderr = child.stderr.take();

        let shared = Arc::new(Shared::default());
        shared.alive.store(true, Ordering::SeqCst);
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));

        let client = Arc::new(Self {
            spec,
            root: root.to_path_buf(),
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            next_id: AtomicU64::new(1),
            pending: pending.clone(),
            shared: shared.clone(),
            capabilities: Mutex::new(serde_json::Value::Null),
            docs: Mutex::new(HashMap::new()),
        });

        // Drain stderr in the background: it is unbounded, and a server
        // whose stderr fills its pipe buffer deadlocks.
        if let Some(stderr) = stderr {
            let shared = shared.clone();
            std::thread::spawn(move || {
                use std::io::BufRead;
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    let mut log = shared.stderr.lock().unwrap();
                    log.push(line);
                    let overflow = log.len().saturating_sub(MAX_STDERR_LINES);
                    log.drain(..overflow);
                }
            });
        }

        // The reader owns a weak handle so a dropped client can still exit
        // the thread, and replies to server requests through it.
        let replier = Arc::downgrade(&client);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                match protocol::read_message(&mut reader) {
                    Ok(Some(message)) => {
                        handle_message(&message, &pending, &shared, &replier);
                        if let Some(notify) = &on_event {
                            notify();
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        shared.log.lock().unwrap().push(format!("read error: {e}"));
                        break;
                    }
                }
            }
            shared.alive.store(false, Ordering::SeqCst);
            // Nothing will ever answer the requests still in flight; drop
            // their senders so callers get an error instead of a timeout.
            pending.lock().unwrap().clear();
            if let Some(notify) = &on_event {
                notify();
            }
        });

        // A server that refuses to start usually says why on stderr and
        // exits, which otherwise surfaces as a bare "stopped responding".
        client.initialize().map_err(|e| match client.stderr_tail() {
            Some(tail) => format!("{e}\n{tail}"),
            None => e,
        })?;
        Ok(client)
    }

    fn initialize(&self) -> Result<(), String> {
        let params = serde_json::json!({
            "processId": std::process::id(),
            "rootUri": path_to_uri(&self.root),
            "workspaceFolders": [{
                "uri": path_to_uri(&self.root),
                "name": self.root.file_name().map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "workspace".into()),
            }],
            "clientInfo": {"name": "DevDock", "version": env!("CARGO_PKG_VERSION")},
            "capabilities": {
                "workspace": {
                    "workspaceFolders": true,
                    "configuration": true,
                    "applyEdit": false,
                    "symbol": {"dynamicRegistration": false},
                },
                "textDocument": {
                    "synchronization": {"didSave": true, "willSave": false},
                    "publishDiagnostics": {"relatedInformation": false},
                    "hover": {"contentFormat": ["markdown", "plaintext"]},
                    "definition": {"linkSupport": true},
                    "references": {"dynamicRegistration": false},
                    "documentSymbol": {"hierarchicalDocumentSymbolSupport": true},
                    "formatting": {"dynamicRegistration": false},
                    "rename": {"prepareSupport": true},
                    "completion": {
                        "completionItem": {
                            // No snippet support: the editor inserts plain
                            // text, so asking for $0 placeholders would only
                            // produce literal dollar signs in the buffer.
                            "snippetSupport": false,
                            "documentationFormat": ["plaintext"],
                            "labelDetailsSupport": true,
                        },
                        "contextSupport": true,
                    },
                },
            },
        });
        let result = self.request("initialize", params, DEFAULT_TIMEOUT)?;
        *self.capabilities.lock().unwrap() =
            result.get("capabilities").cloned().unwrap_or(serde_json::Value::Null);
        self.notify("initialized", serde_json::json!({}))?;
        Ok(())
    }

    pub fn spec(&self) -> &ServerSpec {
        &self.spec
    }

    /// Whether the server process is still running.
    pub fn alive(&self) -> bool {
        self.shared.alive.load(Ordering::SeqCst)
    }

    /// The latest progress title, for a status line.
    pub fn status(&self) -> Option<String> {
        self.shared.status.lock().unwrap().clone()
    }

    /// Recent server log lines (`window/logMessage`).
    pub fn log(&self) -> Vec<String> {
        self.shared.log.lock().unwrap().clone()
    }

    /// Anything the server wrote to stderr, newest last.
    pub fn stderr(&self) -> Vec<String> {
        self.shared.stderr.lock().unwrap().clone()
    }

    /// The last few stderr lines, formatted for an error message.
    fn stderr_tail(&self) -> Option<String> {
        // Give the drain thread a moment: a process that exits immediately
        // often loses the race with the error being formatted.
        for _ in 0..10 {
            if !self.shared.stderr.lock().unwrap().is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let lines = self.shared.stderr.lock().unwrap();
        let tail: Vec<&str> = lines.iter().rev().take(5).map(String::as_str).collect();
        if tail.is_empty() {
            return None;
        }
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        Some(format!("{} said: {}", self.spec.name, tail.join(" / ")))
    }

    /// Diagnostics currently published for `path`.
    pub fn diagnostics(&self, path: &Path) -> Vec<Diagnostic> {
        self.shared
            .diagnostics
            .lock()
            .unwrap()
            .get(&path_to_uri(path))
            .cloned()
            .unwrap_or_default()
    }

    /// How many times this file's diagnostics have been published.
    ///
    /// Take it before an edit, then pass it to [`Self::wait_for_diagnostics`]
    /// afterwards to get results that are actually about the new text.
    pub fn diagnostic_epoch(&self, path: &Path) -> u64 {
        self.shared
            .epochs
            .lock()
            .unwrap()
            .get(&path_to_uri(path))
            .copied()
            .unwrap_or(0)
    }

    /// Waits for diagnostics newer than `after`, then returns them.
    ///
    /// Returns whatever is current if the wait runs out — a server that
    /// stays quiet usually means "nothing to report", and blocking a caller
    /// forever to be sure is worse than saying so.
    pub fn wait_for_diagnostics(
        &self,
        path: &Path,
        after: u64,
        timeout: Duration,
    ) -> (Vec<Diagnostic>, bool) {
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            if self.diagnostic_epoch(path) > after {
                return (self.diagnostics(path), true);
            }
            if !self.alive() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        (self.diagnostics(path), false)
    }

    /// Symbols matching `query` across the workspace.
    pub fn workspace_symbols(&self, query: &str) -> Result<Vec<Location>, String> {
        if !self.supports("workspaceSymbolProvider") {
            return Err("this language server cannot search workspace symbols".into());
        }
        let value = self.request(
            "workspace/symbol",
            serde_json::json!({"query": query}),
            DEFAULT_TIMEOUT,
        )?;
        // The response is SymbolInformation[], whose locations parse the
        // same way go-to-definition's do.
        let mut out = Vec::new();
        if let Some(items) = value.as_array() {
            for item in items {
                if let Some(location) = item.get("location") {
                    out.extend(Location::parse_any(location));
                }
            }
        }
        Ok(out)
    }

    /// Symbols matching `query`, with their names, for an agent that needs
    /// to find something by name rather than by position.
    pub fn workspace_symbol_names(
        &self,
        query: &str,
    ) -> Result<Vec<(String, Location)>, String> {
        let value = self.request(
            "workspace/symbol",
            serde_json::json!({"query": query}),
            DEFAULT_TIMEOUT,
        )?;
        let mut out = Vec::new();
        if let Some(items) = value.as_array() {
            for item in items {
                let Some(name) = item.get("name").and_then(|n| n.as_str()) else { continue };
                let Some(location) = item.get("location") else { continue };
                for location in Location::parse_any(location) {
                    out.push((name.to_string(), location));
                }
            }
        }
        Ok(out)
    }

    /// Every file with diagnostics, as `(path, diagnostics)`.
    pub fn all_diagnostics(&self) -> Vec<(PathBuf, Vec<Diagnostic>)> {
        let map = self.shared.diagnostics.lock().unwrap();
        let mut out: Vec<(PathBuf, Vec<Diagnostic>)> = map
            .iter()
            .filter(|(_, list)| !list.is_empty())
            .filter_map(|(uri, list)| Some((protocol::uri_to_path(uri)?, list.clone())))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    // -- document sync ------------------------------------------------------

    /// Tells the server a file is open and hands it the current text. Safe
    /// to call again for an already-open file: it becomes a change.
    pub fn did_open(&self, path: &Path, text: &str) -> Result<(), String> {
        let uri = path_to_uri(path);
        let already = self.docs.lock().unwrap().contains_key(&uri);
        if already {
            return self.did_change(path, text);
        }
        self.docs.lock().unwrap().insert(uri.clone(), 1);
        self.notify(
            "textDocument/didOpen",
            serde_json::json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": self.spec.language_id,
                    "version": 1,
                    "text": text,
                }
            }),
        )
    }

    /// Sends the buffer's new content. Full-document sync: simple, and
    /// correct against an editor that can change many lines at once
    /// (formatting, an accepted AI edit) without tracking deltas.
    pub fn did_change(&self, path: &Path, text: &str) -> Result<(), String> {
        let uri = path_to_uri(path);
        let version = {
            let mut docs = self.docs.lock().unwrap();
            let version = docs.entry(uri.clone()).or_insert(1);
            *version += 1;
            *version
        };
        self.notify(
            "textDocument/didChange",
            serde_json::json!({
                "textDocument": {"uri": uri, "version": version},
                "contentChanges": [{"text": text}],
            }),
        )
    }

    pub fn did_save(&self, path: &Path, text: &str) -> Result<(), String> {
        self.notify(
            "textDocument/didSave",
            serde_json::json!({
                "textDocument": {"uri": path_to_uri(path)},
                "text": text,
            }),
        )
    }

    pub fn did_close(&self, path: &Path) -> Result<(), String> {
        let uri = path_to_uri(path);
        self.docs.lock().unwrap().remove(&uri);
        self.shared.diagnostics.lock().unwrap().remove(&uri);
        self.notify(
            "textDocument/didClose",
            serde_json::json!({"textDocument": {"uri": uri}}),
        )
    }

    /// Whether the server has this file open, which every position request
    /// requires.
    pub fn is_open(&self, path: &Path) -> bool {
        self.docs.lock().unwrap().contains_key(&path_to_uri(path))
    }

    // -- features -----------------------------------------------------------

    /// Type and documentation at a position.
    pub fn hover(&self, path: &Path, position: Position) -> Result<Option<String>, String> {
        let value = self.request(
            "textDocument/hover",
            self.position_params(path, position),
            DEFAULT_TIMEOUT,
        )?;
        Ok(hover_text(&value))
    }

    /// Where the symbol under the cursor is defined.
    pub fn definition(&self, path: &Path, position: Position) -> Result<Vec<Location>, String> {
        let value = self.request(
            "textDocument/definition",
            self.position_params(path, position),
            DEFAULT_TIMEOUT,
        )?;
        Ok(Location::parse_any(&value))
    }

    /// Everywhere the symbol under the cursor is used.
    pub fn references(&self, path: &Path, position: Position) -> Result<Vec<Location>, String> {
        let mut params = self.position_params(path, position);
        params["context"] = serde_json::json!({"includeDeclaration": true});
        let value = self.request("textDocument/references", params, DEFAULT_TIMEOUT)?;
        Ok(Location::parse_any(&value))
    }

    /// The file's symbol outline.
    pub fn document_symbols(&self, path: &Path) -> Result<Vec<Symbol>, String> {
        let value = self.request(
            "textDocument/documentSymbol",
            serde_json::json!({"textDocument": {"uri": path_to_uri(path)}}),
            DEFAULT_TIMEOUT,
        )?;
        Ok(Symbol::parse_all(&value))
    }

    /// Completions at a position, already sorted the way they should be
    /// shown.
    pub fn completion(
        &self,
        path: &Path,
        position: Position,
    ) -> Result<Vec<CompletionItem>, String> {
        let value = self.request(
            "textDocument/completion",
            self.position_params(path, position),
            DEFAULT_TIMEOUT,
        )?;
        let items = match value.get("items") {
            Some(items) => items.clone(),
            None => value.clone(),
        };
        let mut list: Vec<CompletionItem> = items
            .as_array()
            .map(|items| items.iter().filter_map(CompletionItem::parse).collect())
            .unwrap_or_default();
        list.sort_by(|a, b| {
            a.sort_text
                .as_deref()
                .unwrap_or(&a.label)
                .cmp(b.sort_text.as_deref().unwrap_or(&b.label))
        });
        Ok(list)
    }

    /// The formatted document, or `None` when the server declines to format.
    pub fn format(&self, path: &Path, text: &str, tab_size: u32) -> Result<Option<String>, String> {
        if !self.supports("documentFormattingProvider") {
            return Ok(None);
        }
        let value = self.request(
            "textDocument/formatting",
            serde_json::json!({
                "textDocument": {"uri": path_to_uri(path)},
                "options": {"tabSize": tab_size, "insertSpaces": true},
            }),
            DEFAULT_TIMEOUT,
        )?;
        let edits = TextEdit::parse_all(&value);
        if edits.is_empty() {
            return Ok(None);
        }
        Ok(Some(apply_edits(text, &edits)))
    }

    /// The symbol name at a position, when the server can tell us — used to
    /// prefill the rename box.
    pub fn prepare_rename(
        &self,
        path: &Path,
        position: Position,
    ) -> Result<Option<Range>, String> {
        if !self.supports("renameProvider") {
            return Ok(None);
        }
        let value = self.request(
            "textDocument/prepareRename",
            self.position_params(path, position),
            DEFAULT_TIMEOUT,
        )?;
        let range = value
            .get("range")
            .cloned()
            .or_else(|| value.get("start").map(|_| value.clone()));
        Ok(range.and_then(|r| serde_json::from_value(r).ok()))
    }

    /// A workspace-wide rename, as edits grouped by file. Nothing is
    /// written: the caller applies them, so a rename goes through the same
    /// confirmation as any other multi-file change.
    pub fn rename(
        &self,
        path: &Path,
        position: Position,
        new_name: &str,
    ) -> Result<Vec<(PathBuf, Vec<TextEdit>)>, String> {
        let mut params = self.position_params(path, position);
        params["newName"] = serde_json::json!(new_name);
        let value = self.request("textDocument/rename", params, DEFAULT_TIMEOUT)?;
        Ok(protocol::workspace_edits(&value)
            .into_iter()
            .filter_map(|(uri, edits)| Some((protocol::uri_to_path(&uri)?, edits)))
            .collect())
    }

    /// Whether the server advertised a capability, by its `initialize` key.
    pub fn supports(&self, capability: &str) -> bool {
        let caps = self.capabilities.lock().unwrap();
        match caps.get(capability) {
            Some(serde_json::Value::Bool(b)) => *b,
            Some(serde_json::Value::Null) | None => false,
            Some(_) => true, // an options object counts as support
        }
    }

    // -- transport ----------------------------------------------------------

    fn position_params(&self, path: &Path, position: Position) -> serde_json::Value {
        serde_json::json!({
            "textDocument": {"uri": path_to_uri(path)},
            "position": {"line": position.line, "character": position.character},
        })
    }

    /// Sends a request and waits for its response.
    pub fn request(
        &self,
        method: &str,
        params: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value, String> {
        if !self.alive() {
            return Err(match self.stderr_tail() {
                Some(tail) => format!("{} is not running. {tail}", self.spec.name),
                None => format!("{} is not running", self.spec.name),
            });
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = channel();
        self.pending.lock().unwrap().insert(id, tx);

        let message = serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        });
        if let Err(e) = self.send(&message) {
            self.pending.lock().unwrap().remove(&id);
            return Err(e);
        }

        match rx.recv_timeout(timeout) {
            Ok(result) => result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                self.pending.lock().unwrap().remove(&id);
                Err(format!("{} did not answer {method} in time", self.spec.name))
            }
            // The sender was dropped: the reader thread exited.
            Err(_) => Err(format!("{} stopped responding", self.spec.name)),
        }
    }

    /// Sends a notification, which has no reply.
    pub fn notify(&self, method: &str, params: serde_json::Value) -> Result<(), String> {
        self.send(&serde_json::json!({
            "jsonrpc": "2.0", "method": method, "params": params
        }))
    }

    fn send(&self, message: &serde_json::Value) -> Result<(), String> {
        let mut stdin = self.stdin.lock().unwrap();
        protocol::write_message(&mut *stdin, message)
    }

    /// Asks the server to exit, then makes sure it did.
    pub fn shutdown(&self) {
        let _ = self.request("shutdown", serde_json::Value::Null, Duration::from_secs(3));
        let _ = self.notify("exit", serde_json::Value::Null);
        self.shared.alive.store(false, Ordering::SeqCst);
        if let Ok(mut child) = self.child.lock() {
            // Give it a moment to leave on its own before killing it.
            for _ in 0..20 {
                match child.try_wait() {
                    Ok(Some(_)) => return,
                    Ok(None) => std::thread::sleep(Duration::from_millis(25)),
                    Err(_) => break,
                }
            }
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Client({} in {}, {})",
            self.spec.name,
            self.root.display(),
            if self.alive() { "running" } else { "stopped" }
        )
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        // A leaked rust-analyzer will happily keep a core busy, so never
        // rely on the app exiting to clean it up.
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Routes one incoming message: response, server request, or notification.
fn handle_message(
    message: &serde_json::Value,
    pending: &Pending,
    shared: &Shared,
    replier: &std::sync::Weak<Client>,
) {
    let id = message.get("id").and_then(|i| i.as_u64());
    let method = message.get("method").and_then(|m| m.as_str());

    match (id, method) {
        // A response to something we asked.
        (Some(id), None) => {
            let Some(tx) = pending.lock().unwrap().remove(&id) else { return };
            let result = match message.get("error") {
                Some(error) => Err(error
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("language server error")
                    .to_string()),
                None => Ok(message.get("result").cloned().unwrap_or(serde_json::Value::Null)),
            };
            let _ = tx.send(result);
        }
        // A request *from* the server. Every one of these must be answered:
        // rust-analyzer blocks its own startup waiting on configuration.
        (Some(id), Some(method)) => {
            let result = match method {
                "workspace/configuration" => {
                    let count = message
                        .pointer("/params/items")
                        .and_then(|i| i.as_array())
                        .map(|i| i.len())
                        .unwrap_or(1);
                    serde_json::Value::Array(vec![serde_json::Value::Null; count])
                }
                _ => serde_json::Value::Null,
            };
            if let Some(client) = replier.upgrade() {
                let _ = client.send(&serde_json::json!({
                    "jsonrpc": "2.0", "id": id, "result": result
                }));
            }
        }
        // A notification.
        (None, Some(method)) => match method {
            "textDocument/publishDiagnostics" => {
                let Some(uri) = message.pointer("/params/uri").and_then(|u| u.as_str()) else {
                    return;
                };
                let list: Vec<Diagnostic> = message
                    .pointer("/params/diagnostics")
                    .and_then(|d| d.as_array())
                    .map(|items| items.iter().filter_map(Diagnostic::parse).collect())
                    .unwrap_or_default();
                shared.diagnostics.lock().unwrap().insert(uri.to_string(), list);
                *shared.epochs.lock().unwrap().entry(uri.to_string()).or_insert(0) += 1;
            }
            "window/logMessage" | "window/showMessage" => {
                if let Some(text) = message.pointer("/params/message").and_then(|m| m.as_str())
                {
                    let mut log = shared.log.lock().unwrap();
                    log.push(text.to_string());
                    let overflow = log.len().saturating_sub(MAX_LOG_LINES);
                    log.drain(..overflow);
                }
            }
            "$/progress" => {
                let title = message
                    .pointer("/params/value/title")
                    .and_then(|t| t.as_str())
                    .map(String::from);
                let kind = message
                    .pointer("/params/value/kind")
                    .and_then(|k| k.as_str())
                    .unwrap_or("");
                let mut status = shared.status.lock().unwrap();
                match kind {
                    "end" => *status = None,
                    _ => {
                        if let Some(title) = title {
                            let message_text = message
                                .pointer("/params/value/message")
                                .and_then(|m| m.as_str())
                                .map(|m| format!(" {m}"))
                                .unwrap_or_default();
                            *status = Some(format!("{title}{message_text}"));
                        }
                    }
                }
            }
            _ => {}
        },
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Manager
// ---------------------------------------------------------------------------

/// The servers running for one repository, started on demand.
///
/// Files sharing a server command share one process, which is what language
/// servers expect: rust-analyzer wants the whole workspace, not a process
/// per file.
pub struct Manager {
    root: PathBuf,
    overrides: Vec<registry::ServerOverride>,
    servers: Mutex<HashMap<String, Arc<Client>>>,
    /// Paths whose server failed to start, and why. Kept so the editor can
    /// explain itself once instead of retrying the spawn on every keystroke.
    failed: Mutex<HashMap<String, String>>,
    on_event: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Manager {
    pub fn new(root: &Path, on_event: Option<Arc<dyn Fn() + Send + Sync>>) -> Self {
        Self {
            root: root.to_path_buf(),
            overrides: registry::load_overrides(root),
            servers: Mutex::new(HashMap::new()),
            failed: Mutex::new(HashMap::new()),
            on_event,
        }
    }

    /// Re-reads `[[lsp]]` overrides, for after the config file is edited.
    pub fn reload_config(&mut self) {
        self.overrides = registry::load_overrides(&self.root);
    }

    /// The server for `path` if one is already running. Never blocks, so the
    /// UI can call it every frame.
    pub fn running_for(&self, path: &Path) -> Option<Arc<Client>> {
        let spec = registry::for_path(path, &self.overrides)?;
        let servers = self.servers.lock().unwrap();
        servers.get(&spec.key()).filter(|c| c.alive()).cloned()
    }

    /// The server for `path`, starting it if needed. **Blocks** through the
    /// initialize handshake: worker threads only.
    pub fn ensure_for(&self, path: &Path) -> Result<Arc<Client>, String> {
        let Some(spec) = registry::for_path(path, &self.overrides) else {
            let ext = path.extension().map(|e| e.to_string_lossy().to_string());
            return Err(match ext {
                Some(ext) => match registry::candidates_for_extension(&ext) {
                    c if c.is_empty() => format!("no language server is known for .{ext} files"),
                    c => format!(
                        "no language server for .{ext} on PATH (tried {}). Install one, or \
                         declare it under [[lsp]] in {}.",
                        c.join(", "),
                        crate::local_ci::CONFIG_FILE
                    ),
                },
                None => "no language server for a file without an extension".into(),
            });
        };
        let key = spec.key();

        if let Some(client) = self.servers.lock().unwrap().get(&key) {
            if client.alive() {
                return Ok(client.clone());
            }
        }
        if let Some(why) = self.failed.lock().unwrap().get(&key) {
            return Err(why.clone());
        }

        match Client::start(spec, &self.root, self.on_event.clone()) {
            Ok(client) => {
                self.servers.lock().unwrap().insert(key, client.clone());
                Ok(client)
            }
            Err(e) => {
                self.failed.lock().unwrap().insert(key, e.clone());
                Err(e)
            }
        }
    }

    /// Clears a remembered start failure, so the next open retries.
    pub fn forget_failures(&self) {
        self.failed.lock().unwrap().clear();
    }

    /// Every running server, for a status panel.
    pub fn running(&self) -> Vec<Arc<Client>> {
        self.servers.lock().unwrap().values().cloned().collect()
    }

    /// Diagnostics for one file from whichever server owns it.
    pub fn diagnostics(&self, path: &Path) -> Vec<Diagnostic> {
        self.running_for(path).map(|c| c.diagnostics(path)).unwrap_or_default()
    }

    /// Diagnostics across every running server, worst-first per file.
    pub fn all_diagnostics(&self) -> Vec<(PathBuf, Vec<Diagnostic>)> {
        let mut out: Vec<(PathBuf, Vec<Diagnostic>)> = Vec::new();
        for client in self.running() {
            out.extend(client.all_diagnostics());
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Stops every server. Called when the repository changes or the app
    /// exits.
    pub fn shutdown_all(&self) {
        let servers: Vec<Arc<Client>> =
            self.servers.lock().unwrap().drain().map(|(_, c)| c).collect();
        for client in servers {
            client.shutdown();
        }
        self.failed.lock().unwrap().clear();
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        self.shutdown_all();
    }
}
