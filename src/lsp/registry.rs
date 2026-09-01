//! Which language server to run for a file.
//!
//! A built-in table covers the servers people actually have installed, keyed
//! by file extension and resolved against `PATH`. Anything else — a server
//! not listed, a wrapper script, a different toolchain — is declared in the
//! repository's own `.git-manage-ci.toml`:
//!
//! ```toml
//! [[lsp]]
//! extensions = ["rs"]
//! command = "rust-analyzer"
//! args = ["--log-file", "/tmp/ra.log"]
//! language_id = "rust"          # optional; defaults to the first extension
//! ```
//!
//! Overrides are matched before the built-ins, so declaring one for `rs`
//! replaces rust-analyzer rather than competing with it.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// A server the client knows how to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSpec {
    /// Display name, e.g. "rust-analyzer".
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    /// The `languageId` sent in `textDocument/didOpen`.
    pub language_id: String,
}

impl ServerSpec {
    /// Key that decides whether two files share one server process.
    pub fn key(&self) -> String {
        format!("{} {}", self.command, self.args.join(" "))
    }
}

/// One `[[lsp]]` entry from `.git-manage-ci.toml`.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ServerOverride {
    /// Extensions this server handles, without the dot.
    #[serde(default)]
    pub extensions: Vec<String>,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub language_id: Option<String>,
}

/// Only the `[[lsp]]` section; every other key in the file is ignored, so
/// this parses the same file local CI does without knowing anything about it.
#[derive(Deserialize, Default)]
struct LspSection {
    #[serde(default, rename = "lsp")]
    servers: Vec<ServerOverride>,
}

/// The built-in table: extensions, language id, command, default args.
const BUILT_IN: &[(&[&str], &str, &str, &[&str])] = &[
    (&["rs"], "rust", "rust-analyzer", &[]),
    (&["py", "pyi"], "python", "pyright-langserver", &["--stdio"]),
    (&["py", "pyi"], "python", "pylsp", &[]),
    (
        &["ts", "tsx", "js", "jsx", "mjs", "cjs"],
        "typescript",
        "typescript-language-server",
        &["--stdio"],
    ),
    (&["go"], "go", "gopls", &[]),
    (&["c", "h", "cc", "cpp", "hpp", "cxx"], "cpp", "clangd", &[]),
    (&["java"], "java", "jdtls", &[]),
    (&["dart"], "dart", "dart", &["language-server", "--protocol=lsp"]),
    (&["rb"], "ruby", "solargraph", &["stdio"]),
    (&["php"], "php", "intelephense", &["--stdio"]),
    (&["lua"], "lua", "lua-language-server", &[]),
    (&["json"], "json", "vscode-json-language-server", &["--stdio"]),
    (&["yaml", "yml"], "yaml", "yaml-language-server", &["--stdio"]),
    (&["html"], "html", "vscode-html-language-server", &["--stdio"]),
    (&["css", "scss", "less"], "css", "vscode-css-language-server", &["--stdio"]),
    (&["sh", "bash"], "shellscript", "bash-language-server", &["start"]),
    (&["zig"], "zig", "zls", &[]),
    (&["kt", "kts"], "kotlin", "kotlin-language-server", &[]),
];

/// Reads `[[lsp]]` from the repository root's config, if there is one.
///
/// A malformed file yields no overrides rather than an error: the same file
/// drives local CI, which reports its own parse failures, and the editor
/// should still open.
pub fn load_overrides(repo_root: &Path) -> Vec<ServerOverride> {
    let path = repo_root.join(crate::local_ci::CONFIG_FILE);
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    toml::from_str::<LspSection>(&text)
        .map(|s| s.servers)
        .unwrap_or_default()
        .into_iter()
        .filter(|s| !s.command.trim().is_empty())
        .collect()
}

/// The server for `path`: a matching override first, then the first built-in
/// whose command is on `PATH`.
pub fn for_path(path: &Path, overrides: &[ServerOverride]) -> Option<ServerSpec> {
    let ext = path.extension()?.to_string_lossy().to_lowercase();

    for entry in overrides {
        if entry.extensions.iter().any(|e| e.trim_start_matches('.').eq_ignore_ascii_case(&ext))
        {
            let language_id = entry.language_id.clone().unwrap_or_else(|| ext.clone());
            return Some(ServerSpec {
                name: entry.command.clone(),
                command: entry.command.clone(),
                args: entry.args.clone(),
                language_id,
            });
        }
    }

    for (extensions, language_id, command, args) in BUILT_IN {
        if !extensions.iter().any(|e| *e == ext) {
            continue;
        }
        if !on_path(command) {
            continue;
        }
        return Some(ServerSpec {
            name: (*command).to_string(),
            command: (*command).to_string(),
            args: args.iter().map(|a| (*a).to_string()).collect(),
            language_id: (*language_id).to_string(),
        });
    }
    None
}

/// Every built-in server for `ext`, installed or not, for the "how do I get
/// language support here?" message in the editor.
pub fn candidates_for_extension(ext: &str) -> Vec<&'static str> {
    let ext = ext.to_lowercase();
    BUILT_IN
        .iter()
        .filter(|(extensions, ..)| extensions.iter().any(|e| *e == ext))
        .map(|(_, _, command, _)| *command)
        .collect()
}

/// Whether `command` resolves to an executable, the way a shell would.
pub fn on_path(command: &str) -> bool {
    if command.contains('/') {
        return Path::new(command).is_file();
    }
    let Ok(path) = std::env::var("PATH") else { return false };
    std::env::split_paths(&path).any(|dir| {
        let candidate = dir.join(command);
        candidate.is_file() || candidate.with_extension("exe").is_file()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_override_wins_over_the_built_in_table() {
        let overrides = vec![ServerOverride {
            extensions: vec!["rs".into()],
            command: "my-analyzer".into(),
            args: vec!["--stdio".into()],
            language_id: None,
        }];
        let spec = for_path(Path::new("/repo/src/main.rs"), &overrides).unwrap();
        assert_eq!(spec.command, "my-analyzer");
        assert_eq!(spec.args, vec!["--stdio"]);
        assert_eq!(spec.language_id, "rs");
    }

    #[test]
    fn a_leading_dot_in_an_override_extension_still_matches() {
        let overrides = vec![ServerOverride {
            extensions: vec![".toml".into()],
            command: "taplo".into(),
            args: vec!["lsp".into()],
            language_id: Some("toml".into()),
        }];
        assert!(for_path(Path::new("Cargo.toml"), &overrides).is_some());
    }

    #[test]
    fn files_without_an_extension_have_no_server() {
        assert!(for_path(Path::new("/repo/Makefile"), &[]).is_none());
    }

    #[test]
    fn overrides_load_from_the_ci_config_and_ignore_the_rest_of_it() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(crate::local_ci::CONFIG_FILE),
            "[[job]]\nname = \"tests\"\ncommands = [\"cargo test\"]\n\n\
             [[lsp]]\nextensions = [\"rs\"]\ncommand = \"rust-analyzer\"\n",
        )
        .unwrap();
        let overrides = load_overrides(tmp.path());
        assert_eq!(overrides.len(), 1);
        assert_eq!(overrides[0].command, "rust-analyzer");
    }

    #[test]
    fn a_broken_config_yields_no_overrides_rather_than_failing() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(crate::local_ci::CONFIG_FILE), "not = [valid").unwrap();
        assert!(load_overrides(tmp.path()).is_empty());
        assert!(load_overrides(Path::new("/nonexistent")).is_empty());
    }

    #[test]
    fn candidates_are_listed_for_a_known_extension() {
        assert!(candidates_for_extension("py").contains(&"pyright-langserver"));
        assert!(candidates_for_extension("zzz").is_empty());
    }

    #[test]
    fn path_lookup_finds_a_real_binary() {
        assert!(on_path("sh") || on_path("cmd"));
        assert!(!on_path("definitely-not-a-real-binary-xyz"));
    }
}
