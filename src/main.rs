//! DevDock: native desktop git client with GitHub and AI integration.

fn main() -> eframe::Result<()> {
    // `git-manage ci` runs the local CI headlessly (used by the pre-push
    // hook); anything else launches the GUI.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("ci") {
        let cwd = std::env::current_dir().expect("cwd");
        let root = git_manage::git::Repo::open(&cwd)
            .map(|r| r.path().to_path_buf())
            .unwrap_or(cwd);
        match git_manage::local_ci::run_all_cli(&root) {
            Ok(true) => std::process::exit(0),
            Ok(false) => {
                eprintln!("devdock ci: checks failed; push aborted");
                std::process::exit(1);
            }
            Err(e) => {
                eprintln!("devdock ci: {e}");
                std::process::exit(1);
            }
        }
    }
    git_manage::app::run()
}
