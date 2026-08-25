//! Tests of the language server client against a real child process.
//!
//! `tests/fixtures/fake_lsp.py` is a genuine language server — framed
//! JSON-RPC over stdio — small enough to be deterministic. Testing against a
//! process rather than a mock is what catches the framing, handshake, and
//! server-to-client request bugs that only appear once something is actually
//! on the other end of a pipe.

use git_manage::lsp::protocol::{Position, Severity};
use git_manage::lsp::registry::ServerSpec;
use git_manage::lsp::{Client, Manager};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake_lsp.py")
}

fn spec() -> ServerSpec {
    ServerSpec {
        name: "fake".into(),
        command: "python3".into(),
        args: vec![fixture().to_string_lossy().to_string()],
        language_id: "rust".into(),
    }
}

/// A workspace with one file, and a started client for it.
fn started() -> (tempfile::TempDir, Arc<Client>, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("main.rs");
    std::fs::write(&file, "fn greet() -> String { String::new() }\n").unwrap();
    let client = Client::start(spec(), tmp.path(), None).expect("server should start");
    (tmp, client, file)
}

/// Waits for `check` to hold, so tests never race the reader thread.
fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if check() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("timed out waiting for {what}");
}

#[test]
fn starts_completes_the_handshake_and_reports_capabilities() {
    let (_tmp, client, _file) = started();
    assert!(client.alive());
    assert!(client.supports("hoverProvider"));
    assert!(client.supports("renameProvider"), "an options object counts as support");
    assert!(!client.supports("codeActionProvider"));
    client.shutdown();
    assert!(!client.alive());
}

#[test]
fn answers_the_requests_the_server_makes_of_us() {
    // The fake server sends workspace/configuration right after
    // `initialized`. A client that ignores it leaves a real server (this is
    // exactly what rust-analyzer does) waiting forever, so the proof it was
    // answered is that everything afterwards still works.
    let (_tmp, client, file) = started();
    client.did_open(&file, "fn greet() {}\n").unwrap();
    let hover = client.hover(&file, Position::new(0, 4)).unwrap();
    assert_eq!(hover.as_deref(), Some("fn greet() -> String"));
    client.shutdown();
}

#[test]
fn diagnostics_arrive_on_open_and_clear_on_change() {
    let (_tmp, client, file) = started();
    client.did_open(&file, "fn greet() {}\n").unwrap();
    eventually("diagnostics to arrive", || !client.diagnostics(&file).is_empty());

    let diagnostics = client.diagnostics(&file);
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].severity, Severity::Error);
    assert_eq!(diagnostics[0].code.as_deref(), Some("E0001"));
    assert!(diagnostics[0].line().contains("1:4: error: [E0001] something is wrong"));
    assert_eq!(client.all_diagnostics().len(), 1);

    client.did_change(&file, "fn greet() { ok() }\n").unwrap();
    eventually("diagnostics to clear", || client.diagnostics(&file).is_empty());
    client.shutdown();
}

#[test]
fn every_feature_request_round_trips() {
    let (_tmp, client, file) = started();
    let text = "let x = greet();\n\n\nfn greet() {}\n";
    client.did_open(&file, text).unwrap();

    let definitions = client.definition(&file, Position::new(0, 9)).unwrap();
    assert_eq!(definitions.len(), 1);
    assert_eq!(definitions[0].range.start.line, 4);

    let references = client.references(&file, Position::new(0, 9)).unwrap();
    assert_eq!(references.len(), 2);

    let symbols = client.document_symbols(&file).unwrap();
    assert_eq!(symbols.len(), 2);
    assert_eq!(symbols[1].name, "run");
    assert_eq!(symbols[1].depth, 1);

    let completions = client.completion(&file, Position::new(0, 10)).unwrap();
    assert_eq!(completions[0].label, "greet");
    // The second item carries a textEdit, whose newText is what gets typed.
    assert_eq!(completions[1].insert, "grumble()");
    assert!(completions[1].range.is_some());

    // Formatting replaces the first four characters.
    let formatted = client.format(&file, text, 4).unwrap().unwrap();
    assert!(formatted.starts_with("FMT"), "{formatted}");

    let range = client.prepare_rename(&file, Position::new(0, 4)).unwrap().unwrap();
    assert_eq!(range.start.character, 3);

    let renames = client.rename(&file, Position::new(0, 4), "hello").unwrap();
    assert_eq!(renames.len(), 1);
    assert_eq!(renames[0].0, file);
    assert_eq!(renames[0].1[0].new_text, "hello");

    client.shutdown();
}

#[test]
fn a_server_error_becomes_an_error_not_a_hang() {
    let (_tmp, client, _file) = started();
    let err = client
        .request("unsupported/method", serde_json::json!({}), Duration::from_secs(5))
        .unwrap_err();
    assert!(err.contains("method not found"), "{err}");
    client.shutdown();
}

#[test]
fn requests_after_the_server_dies_fail_immediately() {
    let (_tmp, client, file) = started();
    client.shutdown();
    let started = Instant::now();
    let err = client.hover(&file, Position::new(0, 0)).unwrap_err();
    assert!(err.contains("not running"), "{err}");
    assert!(started.elapsed() < Duration::from_secs(2), "it must not wait for a timeout");
}

#[test]
fn events_fire_so_a_gui_can_repaint() {
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("main.rs");
    std::fs::write(&file, "fn main() {}\n").unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let counter = count.clone();
    let client =
        Client::start(spec(), tmp.path(), Some(Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        })))
        .unwrap();
    client.did_open(&file, "fn main() {}\n").unwrap();
    eventually("a repaint request", || count.load(Ordering::SeqCst) > 0);
    client.shutdown();
}

#[test]
fn documents_are_tracked_across_open_and_close() {
    let (_tmp, client, file) = started();
    assert!(!client.is_open(&file));
    client.did_open(&file, "one\n").unwrap();
    assert!(client.is_open(&file));
    // Opening twice is a change, not a protocol error.
    client.did_open(&file, "two\n").unwrap();
    assert!(client.is_open(&file));
    client.did_close(&file).unwrap();
    assert!(!client.is_open(&file));
    assert!(client.diagnostics(&file).is_empty(), "closing drops its diagnostics");
    client.shutdown();
}

#[test]
fn the_manager_shares_one_server_and_reports_why_it_cannot_start_one() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join(".git-manage-ci.toml"),
        format!(
            "[[lsp]]\nextensions = [\"rs\"]\ncommand = \"python3\"\nargs = [\"{}\"]\n\
             language_id = \"rust\"\n",
            fixture().display()
        ),
    )
    .unwrap();
    let a = tmp.path().join("a.rs");
    let b = tmp.path().join("b.rs");
    std::fs::write(&a, "fn a() {}\n").unwrap();
    std::fs::write(&b, "fn b() {}\n").unwrap();

    let manager = Manager::new(tmp.path(), None);
    assert!(manager.running_for(&a).is_none(), "nothing runs until it is needed");

    let first = manager.ensure_for(&a).unwrap();
    let second = manager.ensure_for(&b).unwrap();
    assert!(Arc::ptr_eq(&first, &second), "both .rs files share one server");
    assert_eq!(manager.running().len(), 1);
    assert!(manager.running_for(&a).is_some());

    // A file no server handles explains itself instead of silently failing.
    let unknown = tmp.path().join("notes.qqq");
    std::fs::write(&unknown, "").unwrap();
    let err = manager.ensure_for(&unknown).unwrap_err();
    assert!(err.contains("no language server"), "{err}");

    manager.shutdown_all();
    assert!(manager.running_for(&a).is_none());
}

#[test]
fn a_missing_server_binary_fails_once_and_stays_failed() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(
        tmp.path().join(".git-manage-ci.toml"),
        "[[lsp]]\nextensions = [\"rs\"]\ncommand = \"definitely-not-installed-xyz\"\n",
    )
    .unwrap();
    let file = tmp.path().join("a.rs");
    std::fs::write(&file, "").unwrap();

    let manager = Manager::new(tmp.path(), None);
    let first = manager.ensure_for(&file).unwrap_err();
    assert!(first.contains("cannot start"), "{first}");
    assert!(first.contains("[[lsp]]"), "the error should say how to fix it: {first}");
    assert_eq!(manager.ensure_for(&file).unwrap_err(), first, "cached, not retried");
    manager.forget_failures();
    assert!(manager.ensure_for(&file).is_err());
}

/// The same client against the real rust-analyzer, which is where handshake
/// details that a fake server tolerates actually get tested.
///
/// Ignored by default: it needs rust-analyzer on PATH and takes as long as
/// indexing a crate takes. Run it with
/// `cargo test --test lsp -- --ignored --nocapture`.
#[test]
#[ignore]
fn live_rust_analyzer() {
    if !git_manage::lsp::registry::on_path("rust-analyzer") {
        eprintln!("rust-analyzer not installed; skipping");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("src")).unwrap();
    std::fs::write(
        tmp.path().join("Cargo.toml"),
        "[package]\nname = \"probe\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    let file = tmp.path().join("src/main.rs");
    std::fs::write(
        &file,
        "fn greet(name: &str) -> String {\n    format!(\"hi {name}\")\n}\n\n\
         fn main() {\n    println!(\"{}\", greet(\"world\"));\n}\n",
    )
    .unwrap();

    let spec = git_manage::lsp::registry::for_path(&file, &[]).expect("rust-analyzer spec");
    // A rustup shim with no component behind it is on PATH but cannot run;
    // that is an environment fact, not a client bug, so skip rather than fail.
    let client = match Client::start(spec, tmp.path(), None) {
        Ok(client) => client,
        Err(e) => {
            eprintln!("rust-analyzer will not start; skipping: {e}");
            return;
        }
    };
    client.did_open(&file, &std::fs::read_to_string(&file).unwrap()).unwrap();

    // Wait for indexing to finish before asking anything of it.
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut hover = None;
    while Instant::now() < deadline {
        if let Ok(Some(text)) = client.hover(&file, Position::new(5, 21)) {
            hover = Some(text);
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    let hover = hover.expect("hover over greet()");
    println!("HOVER: {hover}");
    assert!(hover.contains("greet"), "{hover}");

    let definitions = client.definition(&file, Position::new(5, 21)).unwrap();
    println!("DEFINITION: {definitions:?}");
    assert_eq!(definitions[0].range.start.line, 0, "greet is defined on line 1");

    let symbols = client.document_symbols(&file).unwrap();
    let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
    println!("SYMBOLS: {names:?}");
    assert!(names.contains(&"greet") && names.contains(&"main"));

    let completions = client.completion(&file, Position::new(1, 22)).unwrap();
    println!("COMPLETIONS: {}", completions.len());
    assert!(!completions.is_empty());

    client.shutdown();
}

/// The same client against clangd, which is a production server with a
/// different personality from rust-analyzer: it publishes diagnostics
/// without a build system and answers immediately.
///
/// Ignored by default; needs clangd on PATH.
#[test]
#[ignore]
fn live_clangd() {
    if !git_manage::lsp::registry::on_path("clangd") {
        eprintln!("clangd not installed; skipping");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("main.c");
    let source = "#include <stdio.h>\n\nint twice(int n) { return n * 2; }\n\n\
                  int main(void) {\n    int x = twice(21);\n    undeclared_call(x);\n\
                      return 0;\n}\n";
    std::fs::write(&file, source).unwrap();

    let spec = git_manage::lsp::registry::for_path(&file, &[]).expect("clangd spec");
    let client = match Client::start(spec, tmp.path(), None) {
        Ok(client) => client,
        Err(e) => {
            eprintln!("clangd will not start; skipping: {e}");
            return;
        }
    };
    client.did_open(&file, source).unwrap();

    // Diagnostics: the undeclared call is an error clangd finds unaided.
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline && client.diagnostics(&file).is_empty() {
        std::thread::sleep(Duration::from_millis(200));
    }
    let diagnostics = client.diagnostics(&file);
    println!("DIAGNOSTICS: {:#?}", diagnostics.iter().map(|d| d.line()).collect::<Vec<_>>());
    assert!(
        diagnostics.iter().any(|d| d.message.to_lowercase().contains("undeclared")),
        "{diagnostics:?}"
    );

    // Hover and go-to-definition over the call to twice() on line 6.
    let call = Position::new(5, 13);
    let mut hover = None;
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Ok(Some(text)) = client.hover(&file, call) {
            hover = Some(text);
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let hover = hover.expect("hover over twice()");
    println!("HOVER: {hover}");
    assert!(hover.contains("twice"), "{hover}");

    let definitions = client.definition(&file, call).unwrap();
    println!("DEFINITION: {definitions:?}");
    assert_eq!(definitions[0].range.start.line, 2, "twice is defined on line 3");

    let symbols = client.document_symbols(&file).unwrap();
    let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();
    println!("SYMBOLS: {names:?}");
    assert!(names.contains(&"twice") && names.contains(&"main"));

    let completions = client.completion(&file, Position::new(5, 17)).unwrap();
    println!("COMPLETIONS: {} items", completions.len());
    assert!(!completions.is_empty());

    client.shutdown();
}
