//! Prints what the stored GitHub token is allowed to do, and never the token.
//!
//! `cargo run --example gh_scopes`

fn main() {
    let Some(token) = std::env::var("GITHUB_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
        .or_else(git_manage::github::TokenStore::load)
    else {
        println!("no token: sign in through the app, or set GITHUB_TOKEN");
        return;
    };
    match git_manage::github::Client::new(token).scopes() {
        Ok((login, scopes)) => {
            println!("account: {login}");
            println!("scopes: {}", if scopes.is_empty() { "(none reported)" } else { &scopes });
        }
        Err(e) => println!("could not ask GitHub: {e}"),
    }
}
