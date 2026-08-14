//! Does the env on SelfDevBuildCommand actually REACH the spawned child?
//!
//! Reading the two `for (key, value) in &build.env { cmd.env(...) }` loops shows
//! intent. This runs the real spawn path and reads what the child SAW, which is
//! the only thing that proves the variable arrives.

use std::path::Path;
use std::process::Command;

fn main() {
    let repo = std::env::args().nth(1).expect("usage: <repo_dir>");
    let repo = Path::new(&repo);

    let build = jcode_build_support::selfdev_build_command(repo);

    println!("program : {}", build.program);
    println!("env     : {:?}", build.env);

    let expected: Vec<_> = build
        .env
        .iter()
        .filter(|(k, _)| k == "JCODE_BUILD_GIT_HASH")
        .collect();
    assert!(
        !expected.is_empty(),
        "FAIL: no JCODE_BUILD_GIT_HASH on the command"
    );
    let want = expected[0].1.clone();

    // Spawn a child the SAME way run_selfdev_build does, but have it print the
    // variable rather than run cargo.
    let mut cmd = Command::new("cmd");
    cmd.args(["/c", "echo %JCODE_BUILD_GIT_HASH%"])
        .current_dir(repo);
    for (key, value) in &build.env {
        cmd.env(key, value);
    }
    let out = cmd.output().expect("spawn child");
    let saw = String::from_utf8_lossy(&out.stdout).trim().to_string();

    println!("child saw: {saw}");
    println!("expected : {want}");
    assert_eq!(saw, want, "FAIL: child did not receive the hash");

    // And the value must be the repo's real HEAD.
    let head = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .current_dir(repo)
        .output()
        .expect("git");
    let head = String::from_utf8_lossy(&head.stdout).trim().to_string();
    println!("real HEAD: {head}");
    assert_eq!(want, head, "FAIL: hash is not HEAD");

    println!("\nPASS: the env reaches the child and equals HEAD");
}
