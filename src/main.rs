//! DevDock: native desktop git client. No arguments launches the GUI; a
//! directory launches it on that repository (how a second window on a
//! worktree is opened); subcommands run the CLI (see `devdock help`).

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // `devdock <dir>`: the GUI on that repository. A directory is never a
    // subcommand name, so there is nothing to disambiguate.
    let start = match args.as_slice() {
        [only] if PathBuf::from(only).is_dir() => Some(PathBuf::from(only)),
        _ => None,
    };
    if start.is_none() {
        if let Some(code) = git_manage::cli::run(&args) {
            return code;
        }
    }
    match git_manage::app::run_with(start) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("devdock: {e}");
            ExitCode::FAILURE
        }
    }
}
