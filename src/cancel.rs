//! The kill switch: a run is stopped by the directory it works in.
//!
//! Everything a run does happens under one root — its worktree, or the
//! repository for a run in the tree — and everything that can take long
//! there already waits in a loop: an engine's stream, a check, a command
//! in the sandbox, the harness between turns. Each of those loops looks
//! at the token for its root, so stopping a run needs nothing passed down
//! through every layer in between: the UI sets the token for the root,
//! and whatever is running under it kills its process group and returns.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// How a run that was stopped says so, in its error and on its card.
pub const STOPPED: &str = "stopped by you";

/// Whether to stop, for everything running under one root.
#[derive(Debug, Clone, Default)]
pub struct Token(Arc<AtomicBool>);

impl Token {
    pub fn is_stopped(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// `Err(STOPPED)` once the run has been stopped, for `?` between steps.
    pub fn check(&self) -> Result<(), String> {
        if self.is_stopped() { Err(STOPPED.to_string()) } else { Ok(()) }
    }
}

fn tokens() -> &'static Mutex<HashMap<PathBuf, Token>> {
    static TOKENS: OnceLock<Mutex<HashMap<PathBuf, Token>>> = OnceLock::new();
    TOKENS.get_or_init(Default::default)
}

/// One spelling per directory: a worktree reached through a symlink
/// (`/tmp` on macOS) is the same run.
fn key(root: &Path) -> PathBuf {
    root.canonicalize().unwrap_or_else(|_| root.to_path_buf())
}

/// The token for `root`, made on first use.
pub fn token(root: &Path) -> Token {
    tokens().lock().unwrap_or_else(|e| e.into_inner()).entry(key(root)).or_default().clone()
}

/// Stops whatever is running under `root`, now and until [`reset`].
pub fn stop(root: &Path) {
    token(root).0.store(true, Ordering::SeqCst);
}

/// A new run under `root` starts unstopped, whatever happened to the last.
pub fn reset(root: &Path) {
    tokens().lock().unwrap_or_else(|e| e.into_inner()).insert(key(root), Token::default());
}

/// Whether an error is a run having been stopped, not having failed.
pub fn was_stopped(error: &str) -> bool {
    error.contains(STOPPED)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_root_is_stopped_until_the_next_run_under_it() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let running = token(dir.path());
        assert!(running.check().is_ok());
        stop(dir.path());
        assert!(running.is_stopped(), "a token taken before the stop sees it");
        assert_eq!(running.check().unwrap_err(), STOPPED);
        assert!(token(dir.path()).is_stopped() && !token(other.path()).is_stopped());

        reset(dir.path());
        assert!(!token(dir.path()).is_stopped(), "the next run starts clean");
        assert!(running.is_stopped(), "and the stopped one stays stopped");
        assert!(was_stopped("the check failed: stopped by you") && !was_stopped("tests FAILED"));
    }
}
