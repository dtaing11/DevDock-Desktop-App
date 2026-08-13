//! Terminal styling for the CLI: ANSI colors with automatic detection.
//!
//! Colors are enabled only when stdout is a TTY and `NO_COLOR` is unset,
//! so piped/scripted output stays plain.

use std::io::IsTerminal;
use std::sync::OnceLock;

fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
    })
}

fn wrap(code: &str, text: &str) -> String {
    if enabled() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

pub fn bold(text: &str) -> String {
    wrap("1", text)
}

pub fn dim(text: &str) -> String {
    wrap("2", text)
}

/// Ember/orange: branch names, highlights.
pub fn ember(text: &str) -> String {
    wrap("38;5;215", text)
}

/// Teal/cyan: informational accents (remotes, shas).
pub fn teal(text: &str) -> String {
    wrap("38;5;80", text)
}

pub fn green(text: &str) -> String {
    wrap("32", text)
}

pub fn red(text: &str) -> String {
    wrap("31", text)
}

pub fn yellow(text: &str) -> String {
    wrap("33", text)
}

/// `ok` / `fail` status pill.
pub fn status(ok: bool) -> String {
    if ok {
        green("PASS")
    } else {
        red("FAIL")
    }
}

/// Section header line: `── title ──…` padded to a fixed width.
pub fn header(title: &str) -> String {
    let line = format!("── {title} {}", "─".repeat(44_usize.saturating_sub(title.len())));
    dim(&line)
}
