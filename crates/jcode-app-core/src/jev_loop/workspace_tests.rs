//! Tests for what the loop runs and reads in a real repository: its own run
//! of a helper's check, the step diff, and the keyword search behind Jev's
//! file picks. Each test uses a throwaway git repository.

use super::config;
use super::pick::{self, Candidate, Terms};
use super::report::CheckResult;
use super::workspace::{RepoWorkspace, Workspace, cut, fit_sections, parse_status};
use std::path::Path;
use std::time::Duration;

fn git(repo: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?}");
}

fn repo_with(files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temp repo");
    git(dir.path(), &["init", "-q"]);
    for (path, text) in files {
        let full = dir.path().join(path);
        std::fs::create_dir_all(full.parent().expect("parent")).expect("dirs");
        std::fs::write(full, text).expect("write");
    }
    git(dir.path(), &["add", "-A"]);
    git(dir.path(), &["commit", "-q", "-m", "init"]);
    dir
}

fn workspace(repo: &Path) -> RepoWorkspace {
    RepoWorkspace::new(repo.to_path_buf(), Duration::from_secs(30))
}

// ------------------------------------------------------- the loop's check

#[cfg(unix)]
#[tokio::test]
async fn the_check_runs_in_the_repo_and_reports_its_exit_and_output() {
    let repo = repo_with(&[("marker.txt", "here\n")]);
    let ws = workspace(repo.path());

    let ok = ws.run_check("test -f marker.txt && echo found it").await;
    assert_eq!(ok.result, CheckResult::Passed, "{ok:?}");
    assert_eq!(ok.exit_code, Some(0));
    assert_eq!(ok.output_tail, "found it");

    // Errors land in the captured output, in order with stdout.
    let bad = ws
        .run_check("echo first; echo 'E: second' >&2; exit 3")
        .await;
    assert_eq!(bad.result, CheckResult::Failed);
    assert_eq!(bad.exit_code, Some(3));
    assert_eq!(bad.output_tail, "first\nE: second");
    assert_eq!(bad.describe(), "FAILED (exit 3)");
}

// The env lock is held across the check's await on purpose: the check's
// shell inherits the process environment, so no other test may change it
// while the check runs.
#[cfg(unix)]
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn the_check_runs_without_cloud_or_git_logins() {
    let repo = repo_with(&[("a.txt", "a\n")]);
    let ws = workspace(repo.path());
    let _env = crate::storage::lock_test_env();
    crate::env::set_var("GH_TOKEN", "secret-gh-token");
    crate::env::set_var("AWS_SECRET_ACCESS_KEY", "secret-aws-key");
    let check = ws
        .run_check(
            "test -z \"$GH_TOKEN\" && test -z \"$AWS_SECRET_ACCESS_KEY\" \
             && test \"$AWS_CONFIG_FILE\" = /dev/null \
             && test \"$(git config --get credential.helper)\" = '' \
             && test \"$GIT_TERMINAL_PROMPT\" = 0",
        )
        .await;
    crate::env::remove_var("GH_TOKEN");
    crate::env::remove_var("AWS_SECRET_ACCESS_KEY");
    assert_eq!(check.result, CheckResult::Passed, "{check:?}");
}

#[tokio::test]
async fn blocked_and_empty_checks_are_never_run() {
    let repo = repo_with(&[("a.txt", "a\n")]);
    let ws = workspace(repo.path());
    let witness = repo.path().join("ran.txt");

    for command in [
        "git push origin main",
        "touch ran.txt && gh pr create",
        "cd . && aws s3 ls",
        "env FOO=1 bash -c 'sudo touch ran.txt'",
    ] {
        let check = ws.run_check(command).await;
        assert_eq!(check.result, CheckResult::NotRun, "{command}");
        assert!(check.why_not_run.contains("never allowed"), "{check:?}");
    }
    let empty = ws.run_check("   ").await;
    assert_eq!(empty.result, CheckResult::NotRun);
    assert_eq!(empty.why_not_run, "the helper named no check command");
    assert!(!witness.exists(), "no blocked command ran any part");
}

#[cfg(unix)]
#[tokio::test]
async fn the_destructive_gate_holds_a_check_that_could_delete_files() {
    let repo = repo_with(&[("a.txt", "a\n")]);
    let ws = workspace(repo.path());
    let outside = tempfile::tempdir().expect("outside dir");
    let victim = outside.path().join("keep.txt");
    std::fs::write(&victim, "keep").expect("write");

    let command = format!(
        "pytest -q; rm -r \"$HOME_DIR_TARGET\" {}/*",
        outside.path().display()
    );
    let check = ws.run_check(&command).await;
    assert_eq!(check.result, CheckResult::NotRun, "{check:?}");
    assert!(
        check.why_not_run.contains("destructive-command gate"),
        "{check:?}"
    );
    assert!(victim.exists());

    // Deleting inside the repository is routine and still runs.
    std::fs::write(repo.path().join("build.tmp"), "x").expect("write");
    let routine = ws.run_check("rm -f build.tmp && echo cleaned").await;
    assert_eq!(routine.result, CheckResult::Passed, "{routine:?}");
}

#[cfg(unix)]
#[tokio::test]
async fn a_check_that_runs_too_long_is_killed_with_its_children() {
    let repo = repo_with(&[("a.txt", "a\n")]);
    let ws = RepoWorkspace::new(repo.path().to_path_buf(), Duration::from_millis(700));
    let pid_file = repo.path().join("child.pid");

    let started = std::time::Instant::now();
    let check = ws
        .run_check("echo started; sleep 30 & echo $! > child.pid; wait")
        .await;
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the timeout stops the wait"
    );
    assert_eq!(check.result, CheckResult::Failed);
    assert!(check.timed_out);
    assert!(check.describe().starts_with("FAILED (timed out after"));
    assert_eq!(check.output_tail, "started");

    let pid: i32 = std::fs::read_to_string(&pid_file)
        .expect("pid written")
        .trim()
        .parse()
        .expect("pid");
    let mut alive = true;
    for _ in 0..50 {
        // Signal 0 checks existence; a zombie reaped by init also reads gone.
        alive = unsafe { libc::kill(pid, 0) } == 0 && !is_zombie(pid);
        if !alive {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!alive, "the background child was killed too");
}

#[cfg(unix)]
fn is_zombie(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .map(|stat| stat.split_whitespace().nth(2) == Some("Z"))
        .unwrap_or(true)
}

#[cfg(unix)]
#[tokio::test]
async fn long_output_keeps_its_end() {
    let repo = repo_with(&[("a.txt", "a\n")]);
    let ws = workspace(repo.path());
    let check = ws
        .run_check("for i in $(seq 1 20000); do echo line $i; done; echo LAST; exit 1")
        .await;
    assert_eq!(check.result, CheckResult::Failed);
    assert!(check.output_tail.ends_with("line 20000\nLAST"));
    assert!(check.output_tail.starts_with("..."));
    assert!(check.output_tail.chars().count() <= super::workspace::CHECK_OUTPUT_TAIL_CHARS + 3);
}

// ------------------------------------------------------------ step diffs

#[tokio::test]
async fn the_diff_shows_only_what_changed_since_the_step_started() {
    let repo = repo_with(&[("src/a.py", "x = 1\n"), ("src/b.py", "y = 1\n")]);
    // Changed before the step started: part of an earlier step.
    std::fs::write(repo.path().join("src/b.py"), "y = 2\n").expect("write");
    let ws = workspace(repo.path());
    let start = ws.snapshot().await;

    std::fs::write(repo.path().join("src/a.py"), "x = 1\nz = 3\n").expect("write");
    std::fs::create_dir_all(repo.path().join("tests")).expect("dir");
    std::fs::write(
        repo.path().join("tests/test_new.py"),
        "def test_z():\n    assert True\n",
    )
    .expect("write");
    let diff = ws.diff_since(&start, 48 * 1024).await;

    assert!(diff.contains("=== changed src/a.py ==="), "{diff}");
    assert!(diff.contains("+z = 3"));
    assert!(diff.contains("=== new file tests/test_new.py ==="));
    assert!(diff.contains("def test_z():"));
    assert!(
        !diff.contains("src/b.py"),
        "earlier steps are not shown: {diff}"
    );

    // Nothing more changed: an empty diff.
    let later = ws.snapshot().await;
    assert_eq!(ws.diff_since(&later, 48 * 1024).await, "");
}

#[tokio::test]
async fn a_restored_file_and_a_deleted_new_file_are_named() {
    let repo = repo_with(&[("a.txt", "one\n")]);
    std::fs::write(repo.path().join("a.txt"), "two\n").expect("write");
    std::fs::write(repo.path().join("scratch.txt"), "tmp\n").expect("write");
    let ws = workspace(repo.path());
    let start = ws.snapshot().await;

    std::fs::write(repo.path().join("a.txt"), "one\n").expect("write");
    std::fs::remove_file(repo.path().join("scratch.txt")).expect("remove");
    let diff = ws.diff_since(&start, 48 * 1024).await;
    assert!(diff.contains("=== restored a.txt ==="), "{diff}");
    assert!(diff.contains("=== removed scratch.txt ==="), "{diff}");
}

#[test]
fn status_parsing_handles_renames_and_spaces() {
    let raw = b" M src/a.rs\0?? new file.txt\0R  dst.rs\0src.rs\0A  added.rs\0";
    assert_eq!(
        parse_status(raw),
        [
            ("src/a.rs".to_string(), false),
            ("new file.txt".to_string(), true),
            ("dst.rs".to_string(), false),
            ("added.rs".to_string(), false),
        ]
    );
}

#[test]
fn big_diffs_are_shared_fairly_and_cut_on_character_boundaries() {
    let sections: Vec<(String, String)> = (0..4)
        .map(|i| (format!("changed f{i}.rs"), "é".repeat(5000)))
        .collect();
    let text = fit_sections(&sections, 12_000);
    assert!(text.len() <= 12_000 + 200, "{}", text.len());
    for i in 0..4 {
        assert!(
            text.contains(&format!("=== changed f{i}.rs ===")),
            "every file gets a share"
        );
    }
    assert!(text.contains("... (cut:"));
    assert_eq!(cut("aé", 2), "a", "never splits a character");

    let many: Vec<(String, String)> = (0..40)
        .map(|i| (format!("changed f{i}.rs"), "x".repeat(4000)))
        .collect();
    let text = fit_sections(&many, 10_000);
    assert!(text.contains("more changed files not shown"), "{text}");
}

// ----------------------------------------------------------- file picks

#[test]
fn terms_split_paths_identifiers_and_words() {
    let terms = pick::terms(
        "Fix _rollup_element_verdict in apps/worker/src/deterministic.py so NOT_APPLICABLE \
         never becomes PASS; see check_ast.py and the evaluator, e.g. version 1.2.",
    );
    assert_eq!(
        terms.paths,
        [
            "deterministic.py",
            "apps/worker/src/deterministic.py",
            "check_ast.py"
        ]
    );
    // `deterministic` is a plain word, so the file is found by its path
    // terms rather than by searching every file's contents for it.
    assert_eq!(
        terms.idents,
        ["rollup_element_verdict", "not_applicable", "check_ast"]
    );
    assert!(terms.words.contains(&"evaluator".to_string()));
    assert!(
        !terms.words.contains(&"never".to_string()),
        "stopwords are dropped"
    );
    assert!(!terms.paths.iter().any(|path| path.contains("e.g")));
}

#[test]
fn ranking_prefers_named_files_and_rare_identifiers() {
    let files: Vec<String> = [
        "apps/worker/src/deterministic.py",
        "apps/worker/tests/test_deterministic.py",
        "docs/notes.md",
        "src/worker_common.py",
        "README.md",
    ]
    .iter()
    .map(|path| path.to_string())
    .collect();
    let terms = Terms {
        paths: vec!["apps/worker/src/deterministic.py".into()],
        idents: vec!["rollup_element_verdict".into(), "needs_review".into()],
        words: vec!["worker".into()],
    };
    // A rare identifier (2 files) and a common one (501 files). The file
    // with only the rare identifier outranks the one with the common
    // identifier and a path word.
    let rare = vec![
        "apps/worker/tests/test_deterministic.py".into(),
        "docs/notes.md".into(),
    ];
    let common: Vec<String> = (0..500)
        .map(|i| format!("other/{i}.py"))
        .chain(["src/worker_common.py".to_string()])
        .collect();
    let ranked = pick::rank(&files, &terms, &[rare, common], 3);
    let paths: Vec<&str> = ranked.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(
        paths,
        [
            "apps/worker/src/deterministic.py",
            "apps/worker/tests/test_deterministic.py",
            "docs/notes.md"
        ]
    );
    assert_eq!(ranked[1].words, ["rollup_element_verdict"]);
    assert!(!paths.contains(&"README.md"));
}

#[tokio::test]
async fn candidates_come_from_tracked_and_new_files_with_matching_lines() {
    let repo = repo_with(&[
        (
            "src/rules.py",
            "def rollup_element_verdict(x):\n    return x\n",
        ),
        ("src/other.py", "print('hello')\n"),
        ("docs/guide.md", "Nothing here.\n"),
    ]);
    std::fs::write(
        repo.path().join("src/new_tests.py"),
        "from rules import rollup_element_verdict\n",
    )
    .expect("write");
    std::fs::write(repo.path().join(".gitignore"), "ignored.py\n").expect("write");
    std::fs::write(repo.path().join("ignored.py"), "rollup_element_verdict\n").expect("write");
    let ws = workspace(repo.path());

    let found: Vec<Candidate> = ws
        .candidates(
            "Test rollup_element_verdict thoroughly",
            config::MAX_PICK_CANDIDATES,
        )
        .await;
    let paths: Vec<&str> = found.iter().map(|c| c.path.as_str()).collect();
    assert_eq!(paths, ["src/new_tests.py", "src/rules.py"]);
    assert_eq!(found[1].lines, ["def rollup_element_verdict(x):"]);
    assert!(ws.candidates("", 10).await.is_empty());
}

#[test]
fn pick_parsing_keeps_likely_files_most_likely_first() {
    let candidates: Vec<Candidate> = ["a.rs", "b.rs", "c.rs", "d.rs"]
        .iter()
        .map(|path| Candidate {
            path: (*path).into(),
            ..Candidate::default()
        })
        .collect();
    let value = serde_json::json!({"answers": {
        "c0": {"type": "noul", "noul": 0.55},
        "c1": {"type": "noul", "noul": 0.2},
        "c2": {"type": "noul", "noul": 0.97},
        "c3": {"type": "noul", "noul": 0.5}
    }});
    let (picked, scores) = pick::parse(&value, &candidates, 0.5, 2).unwrap();
    assert_eq!(picked, ["c.rs", "a.rs"], "best first, capped at 2");
    assert_eq!(scores["b.rs"], 0.2);
    let broken = serde_json::json!({"answers": {"c0": {"type": "noul", "noul": 2.0}}});
    assert!(pick::parse(&broken, &candidates, 0.5, 2).is_err());
    assert_eq!(pick::hint_block(&[]), "");
}

#[test]
fn every_pick_request_fits_jev_limits() {
    let candidates: Vec<Candidate> = (0..config::MAX_PICK_CANDIDATES)
        .map(|i| Candidate {
            path: format!("a/very/long/path/number/{i}/{}.py", "x".repeat(150)),
            lines: vec!["y".repeat(160); 3],
            ..Candidate::default()
        })
        .collect();
    let (state, questions) = pick::request(&"task ".repeat(5000), &candidates);
    // Every Jev route accepts at least 24 questions and 80 KiB per request.
    assert!(questions.len() <= 24);
    let body = serde_json::to_vec(&serde_json::json!({"state": state, "questions": questions}))
        .expect("encode");
    assert!(body.len() < 80 * 1024, "{} bytes", body.len());
}

#[test]
fn every_review_request_fits_jev_limits_even_with_a_huge_diff() {
    use super::fork::review_state;
    use super::report::{HelperReport, LoopCheck};
    let step = super::StepSpec {
        id: 1,
        task: "t".repeat(9000),
        done_when: "d".repeat(9000),
    };
    let report = HelperReport {
        summary: "s".repeat(9000),
        check_command: "c".repeat(9000),
        loop_check: Some(LoopCheck::finished(true, Some(0), "o".repeat(9000), 1.0)),
        ..HelperReport::default()
    };
    // Quotes and newlines double in size when OpenRouter's string state
    // escapes them, the worst case for code diffs.
    let diff = "\"\n".repeat(200_000);
    let state = review_state(&step, &report, &diff);
    let as_string = serde_json::to_string(&state).expect("encode");
    let escaped = serde_json::to_vec(&serde_json::Value::String(as_string)).expect("encode");
    assert!(escaped.len() < 78 * 1024, "{} bytes", escaped.len());
    assert!(state["changes"].as_str().unwrap().ends_with("(cut to fit)"));
    assert!(state["step"].as_str().unwrap().ends_with("..."));
}
