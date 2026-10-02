//! Tests for stopping what a run started: the session sweep against a real
//! detached process in its own session (the way jcode's bash tool starts a
//! helper command), and the bookkeeping for the loop's check groups.

use super::stop;
use std::time::{Duration, Instant};

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    crate::platform::is_process_running(pid)
}

/// Start `sleep 300` in a session of its own, as `spawn_detached` does for a
/// helper command, and register it as a running detached background task of
/// `session_id`.
#[cfg(unix)]
async fn detached_sleeper(
    manager: &crate::background::BackgroundTaskManager,
    session_id: &str,
) -> u32 {
    let mut cmd = std::process::Command::new("sleep");
    cmd.arg("300");
    let child = crate::platform::spawn_detached(&mut cmd).expect("spawn sleep");
    let pid = child.id();
    let info = manager.reserve_task_info();
    manager
        .register_detached_task(
            &info,
            "bash",
            Some("sleep".into()),
            session_id,
            pid,
            &chrono::Utc::now().to_rfc3339(),
            false,
            false,
        )
        .await;
    // Like the bash tool, keep no handle: the background manager owns the
    // process from here and reaps it (waitpid) when it finalizes the task.
    drop(child);
    pid
}

#[cfg(unix)]
fn alive_not_zombie(pid: u32) -> bool {
    // A killed child we never reaped is a zombie: dead, but `kill(pid, 0)`
    // still succeeds. Reap it here if it is ours, then ask again.
    let _ = crate::platform::try_reap_child_process(pid);
    alive(pid)
}

#[cfg(unix)]
async fn wait_gone(pid: u32) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if !alive_not_zombie(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

#[cfg(unix)]
#[tokio::test]
async fn a_sessions_leftover_command_is_stopped_and_other_sessions_are_not() {
    let dir = tempfile::tempdir().expect("task dir");
    let manager = crate::background::BackgroundTaskManager::with_output_dir(dir.path().into());
    let mine = detached_sleeper(&manager, "session_loop_helper").await;
    let other = detached_sleeper(&manager, "session_somebody_else").await;
    assert!(alive(mine) && alive(other), "both sleepers start");

    let stopped = stop::stop_session_tasks_in(&manager, "session_loop_helper").await;

    assert_eq!(stopped, 1, "exactly the helper's task is stopped");
    assert!(
        wait_gone(mine).await,
        "the helper's leftover command is gone"
    );
    assert!(
        alive_not_zombie(other),
        "another session's command keeps running"
    );
    assert_eq!(
        stop::stop_session_tasks_in(&manager, "session_loop_helper").await,
        0,
        "a second sweep finds nothing left"
    );

    // Clean up the other session's sleeper.
    assert_eq!(
        stop::stop_session_tasks_in(&manager, "session_somebody_else").await,
        1
    );
    assert!(wait_gone(other).await);
}

#[cfg(unix)]
#[tokio::test]
async fn a_running_check_is_tracked_and_forgotten_when_it_ends() {
    let dir = tempfile::tempdir().expect("repo");
    let repo = dir.path();
    let status = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(repo)
        .status()
        .expect("git init");
    assert!(status.success());
    let pid_file = repo.join("check.pid");
    let workspace =
        super::workspace::RepoWorkspace::new(repo.to_path_buf(), Duration::from_secs(30));
    // The check records its own process group, then lingers long enough to be
    // seen while it runs. Other tests may run checks at the same time, so
    // only this check's group is looked at.
    let command = "ps -o pgid= -p $$ | tr -d ' ' > check.pid && sleep 1";

    let run = super::workspace::Workspace::run_check(&workspace, command);
    tokio::pin!(run);
    let mut seen_while_running = false;
    let check = loop {
        tokio::select! {
            check = &mut run => break check,
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                let pgid = std::fs::read_to_string(&pid_file)
                    .ok()
                    .and_then(|text| text.trim().parse::<u32>().ok());
                if let Some(pgid) = pgid {
                    seen_while_running |= stop::tracked_check_groups().contains(&pgid);
                }
            }
        }
    };

    assert_eq!(
        check.result,
        super::report::CheckResult::Passed,
        "the check ran: {check:?}"
    );
    let pgid: u32 = std::fs::read_to_string(&pid_file)
        .expect("the check wrote its group")
        .trim()
        .parse()
        .expect("a process group id");
    assert!(
        seen_while_running,
        "the running check's group {pgid} was tracked"
    );
    assert!(
        !stop::tracked_check_groups().contains(&pgid),
        "the finished check's group {pgid} is no longer tracked"
    );
}

#[test]
fn signals_exit_with_the_codes_shells_use() {
    assert_eq!(stop::signal_exit_code("Ctrl+C"), 130);
    assert_eq!(stop::signal_exit_code("SIGTERM"), 143);
    assert_eq!(stop::signal_exit_code("SIGHUP"), 129);
    assert_eq!(stop::signal_exit_code("SIGQUIT"), 131);
}
