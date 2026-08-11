fn main() {
    let repo = git_manage::git::Repo::open("/tmp/merge-test").unwrap();
    let out = repo.merge("feat");
    println!("ok={} conflict={} msg={:?}", out.ok, out.conflict, out.message);
    println!("state={:?}", repo.state().unwrap());
    let log = repo.log(5, None).unwrap();
    for c in log { println!("log: {}", c.subject); }
}
