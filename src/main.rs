//! DevDock: native desktop git client. No arguments launches the GUI;
//! subcommands run the CLI (see `devdock help`).

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(code) = git_manage::cli::run(&args) {
        return code;
    }
    match git_manage::app::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("devdock: {e}");
            ExitCode::FAILURE
        }
    }
}
