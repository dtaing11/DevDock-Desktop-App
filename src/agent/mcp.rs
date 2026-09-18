//! A client for MCP servers over stdio, so the built-in harness can offer
//! their tools next to its own — a Dart `pub` tool, a database inspector,
//! whatever the repository's `.mcp.json` declares.
//!
//! The protocol is JSON-RPC 2.0, one message per line, over the server's
//! stdin and stdout: `initialize`, the `notifications/initialized` note,
//! `tools/list`, and `tools/call`. That is all the harness needs, and all
//! this speaks. A server is started when the workspace is built and killed
//! when the workspace is dropped; in a sandboxed run the server runs inside
//! the sandbox, so what it touches is what the checks touch.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// One server from `.mcp.json`: how to start it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSpec {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

/// The servers a repository declares in its `.mcp.json`, in file order.
/// Servers reached by URL rather than a command are skipped: the harness
/// speaks stdio only.
pub fn declared(root: &Path) -> Vec<ServerSpec> {
    let Ok(text) = std::fs::read_to_string(root.join(".mcp.json")) else { return Vec::new() };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else { return Vec::new() };
    let Some(servers) = value.get("mcpServers").and_then(|s| s.as_object()) else { return Vec::new() };
    servers
        .iter()
        .filter_map(|(name, spec)| {
            let command = spec.get("command")?.as_str()?.to_string();
            let args = spec
                .get("args")
                .and_then(|a| a.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            let env = spec
                .get("env")
                .and_then(|e| e.as_object())
                .map(|e| e.iter().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect())
                .unwrap_or_default();
            Some(ServerSpec { name: name.clone(), command, args, env })
        })
        .collect()
}

/// A tool a server offers.
#[derive(Debug, Clone)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub schema: serde_json::Value,
}

/// How to start a server's process: on the host, or through a sandbox
/// that wraps the command so it runs inside.
pub trait Launcher: Send + Sync {
    /// A command that runs `program args…` in `workdir` with `env`, with
    /// stdin and stdout to be piped by the caller.
    fn command(&self, program: &str, args: &[String], env: &BTreeMap<String, String>, workdir: &Path) -> Command;
}

/// Runs servers on this machine.
pub struct HostLauncher;

impl Launcher for HostLauncher {
    fn command(&self, program: &str, args: &[String], env: &BTreeMap<String, String>, workdir: &Path) -> Command {
        let mut cmd = Command::new(program);
        cmd.args(args).current_dir(workdir);
        cmd.env("PATH", crate::local_ci::runner::login_path());
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd
    }
}

/// A running server: its process and the conversation with it.
pub struct Server {
    pub spec: ServerSpec,
    child: Child,
    io: Mutex<(ChildStdin, BufReader<ChildStdout>, u64)>,
    tools: Vec<Tool>,
}

/// How long a server has to answer one request.
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a server has to come up.
const INIT_TIMEOUT: Duration = Duration::from_secs(60);
/// The MCP revision this client speaks.
const PROTOCOL: &str = "2024-11-05";

impl Server {
    /// Starts the server, shakes hands, and lists its tools.
    pub fn start(spec: ServerSpec, launcher: &dyn Launcher, workdir: &Path) -> Result<Server, String> {
        let mut cmd = launcher.command(&spec.command, &spec.args, &spec.env, workdir);
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            cmd.process_group(0);
        }
        let mut child = cmd.spawn().map_err(|e| format!("MCP server {}: could not start `{}`: {e}", spec.name, spec.command))?;
        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = BufReader::new(child.stdout.take().ok_or("no stdout")?);
        let mut server = Server { spec, child, io: Mutex::new((stdin, stdout, 0)), tools: Vec::new() };
        let init = server.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": PROTOCOL,
                "capabilities": {},
                "clientInfo": {"name": "devdock", "version": env!("CARGO_PKG_VERSION")}
            }),
            INIT_TIMEOUT,
        )?;
        let _ = init;
        server.notify("notifications/initialized", serde_json::json!({}))?;
        let listed = server.request("tools/list", serde_json::json!({}), INIT_TIMEOUT)?;
        server.tools = listed
            .get("tools")
            .and_then(|t| t.as_array())
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(|t| {
                        Some(Tool {
                            name: t.get("name")?.as_str()?.to_string(),
                            description: t.get("description").and_then(|d| d.as_str()).unwrap_or("").to_string(),
                            schema: t.get("inputSchema").cloned().unwrap_or_else(|| serde_json::json!({"type": "object"})),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(server)
    }

    pub fn tools(&self) -> &[Tool] {
        &self.tools
    }

    /// Calls a tool and returns its text, or the error text it reported.
    pub fn call(&self, tool: &str, arguments: serde_json::Value) -> Result<String, String> {
        let result = self.request("tools/call", serde_json::json!({"name": tool, "arguments": arguments}), CALL_TIMEOUT)?;
        let text = result
            .get("content")
            .and_then(|c| c.as_array())
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|p| match p.get("type").and_then(|t| t.as_str()) {
                        Some("text") => p.get("text").and_then(|t| t.as_str()).map(str::to_string),
                        Some(other) => Some(format!("[{other} content omitted]")),
                        None => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_else(|| result.to_string());
        if result.get("isError").and_then(|e| e.as_bool()).unwrap_or(false) {
            Err(text)
        } else {
            Ok(text)
        }
    }

    fn notify(&self, method: &str, params: serde_json::Value) -> Result<(), String> {
        let mut io = self.io.lock().map_err(|_| "MCP io lock poisoned".to_string())?;
        let line = serde_json::json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string();
        writeln!(io.0, "{line}").and_then(|_| io.0.flush()).map_err(|e| format!("MCP server {}: {e}", self.spec.name))
    }

    /// One request, one response: lines that are not its response —
    /// notifications, the server's own requests — are skipped.
    fn request(&self, method: &str, params: serde_json::Value, timeout: Duration) -> Result<serde_json::Value, String> {
        let mut io = self.io.lock().map_err(|_| "MCP io lock poisoned".to_string())?;
        io.2 += 1;
        let id = io.2;
        let line = serde_json::json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
        writeln!(io.0, "{line}").and_then(|_| io.0.flush()).map_err(|e| format!("MCP server {}: {e}", self.spec.name))?;
        let deadline = Instant::now() + timeout;
        loop {
            if Instant::now() > deadline {
                return Err(format!("MCP server {}: no answer to {method} within {}s", self.spec.name, timeout.as_secs()));
            }
            let mut buf = String::new();
            let n = io.1.read_line(&mut buf).map_err(|e| format!("MCP server {}: {e}", self.spec.name))?;
            if n == 0 {
                return Err(format!("MCP server {} exited", self.spec.name));
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(buf.trim()) else { continue };
            if value.get("id").and_then(|i| i.as_u64()) != Some(id) {
                continue;
            }
            if let Some(err) = value.get("error") {
                let message = err.get("message").and_then(|m| m.as_str()).unwrap_or("error");
                return Err(format!("MCP server {}: {method}: {message}", self.spec.name));
            }
            return Ok(value.get("result").cloned().unwrap_or(serde_json::Value::Null));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The tool name the model sees: `mcp__<server>__<tool>`, as Claude Code
/// names them, so a developer reads both logs the same way.
pub fn tool_name(server: &str, tool: &str) -> String {
    format!("mcp__{server}__{tool}")
}

/// Splits a model-facing name back into server and tool.
pub fn split_name(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix("mcp__")?;
    rest.split_once("__")
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The smallest stdio MCP server: one tool, `add(a, b)`, in Python.
    pub const ADDER: &str = r#"
import json, sys
def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n"); sys.stdout.flush()
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    msg = json.loads(line); mid = msg.get("id"); method = msg.get("method")
    if method == "initialize":
        send({"jsonrpc":"2.0","id":mid,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"adder","version":"0.1"}}})
    elif method == "notifications/initialized":
        send({"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info","data":"ready"}})
    elif method == "tools/list":
        send({"jsonrpc":"2.0","id":mid,"result":{"tools":[{"name":"add","description":"Adds two integers.","inputSchema":{"type":"object","properties":{"a":{"type":"integer"},"b":{"type":"integer"}},"required":["a","b"]}}]}})
    elif method == "tools/call":
        args = msg["params"].get("arguments",{})
        if msg["params"].get("name") != "add":
            send({"jsonrpc":"2.0","id":mid,"result":{"content":[{"type":"text","text":"no such tool"}],"isError":True}})
        else:
            send({"jsonrpc":"2.0","id":mid,"result":{"content":[{"type":"text","text":str(int(args["a"])+int(args["b"]))}],"isError":False}})
    elif mid is not None:
        send({"jsonrpc":"2.0","id":mid,"error":{"code":-32601,"message":"method not found"}})
"#;

    /// A repository with `.mcp.json` pointing at the adder.
    pub fn adder_repo() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("adder_mcp.py"), ADDER).unwrap();
        std::fs::write(
            tmp.path().join(".mcp.json"),
            serde_json::json!({"mcpServers": {"adder": {"command": "python3", "args": ["adder_mcp.py"]}, "remote": {"url": "https://example.com/mcp"}}}).to_string(),
        )
        .unwrap();
        tmp
    }

    #[test]
    fn a_declared_stdio_server_is_started_listed_and_called() {
        let tmp = adder_repo();
        let specs = declared(tmp.path());
        assert_eq!(specs.len(), 1, "the URL server is skipped: {specs:?}");
        assert_eq!(specs[0].name, "adder");
        let server = Server::start(specs[0].clone(), &HostLauncher, tmp.path()).unwrap();
        assert_eq!(server.tools().len(), 1);
        assert_eq!(server.tools()[0].name, "add");
        assert_eq!(server.call("add", serde_json::json!({"a": 20, "b": 22})).unwrap(), "42");
        let err = server.call("nope", serde_json::json!({})).unwrap_err();
        assert_eq!(err, "no such tool");
        assert_eq!(tool_name("adder", "add"), "mcp__adder__add");
        assert_eq!(split_name("mcp__adder__add"), Some(("adder", "add")));
        assert_eq!(split_name("run_command"), None);
    }

    /// The adder started inside the sandbox — its process in the VM or
    /// container, over the worktree at its inner path — and called.
    /// `cargo test --lib mcp::tests::live -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn live_a_server_runs_inside_the_sandbox() {
        if crate::sandbox::installed().is_empty() {
            eprintln!("no sandbox runtime; skipping");
            return;
        }
        let repo = adder_repo();
        let sandbox = crate::sandbox::Sandbox::start(&crate::sandbox::Spec::default(), repo.path(), &mut |l| println!("  {l}")).unwrap();
        sandbox.provision(&["python3"], &mut |l| println!("  {l}")).unwrap();
        let launcher = crate::sandbox::SandboxRunner(std::sync::Arc::new(sandbox));
        let spec = declared(repo.path()).remove(0);
        let server = Server::start(spec, &launcher, repo.path()).unwrap();
        assert_eq!(server.tools()[0].name, "add");
        assert_eq!(server.call("add", serde_json::json!({"a": 20, "b": 22})).unwrap(), "42");
        println!("the adder answered from inside the sandbox");
    }

    #[test]
    fn a_server_that_cannot_start_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let spec = ServerSpec { name: "ghost".into(), command: "no-such-program-xyz".into(), args: vec![], env: Default::default() };
        let err = Server::start(spec, &HostLauncher, tmp.path()).err().expect("a missing program cannot start");
        assert!(err.contains("could not start"), "{err}");
    }
}
