//! Stopping everything a run started, not just the loop itself.
//!
//! A loop starts processes the terminal cannot reach: jcode's bash tool runs
//! each helper command in a session of its own (`setsid`), a command that
//! outlives its foreground time limit becomes a background task that outlives
//! the helper, and the loop's own check runs in a process group of its own.
//! Measured with the real binary, Ctrl+C stopped the loop while those kept
//! running and writing to disk. This module records what each run starts and
//! stops all of it on Ctrl+C or SIGTERM, when a session ends, and when the
//! run ends.

use std::collections::BTreeSet;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// How long processes get between SIGTERM and SIGKILL.
pub const GRACE: Duration = Duration::from_secs(3);
/// Longer than the agent's 750 ms bash hand-off window on shutdown.
const HANDOFF_WAIT: Duration = Duration::from_millis(1500);

/// Everything the current run has started and not yet stopped.
#[derive(Default)]
struct Registry {
    /// Agent sessions of this run; their background tasks belong to the run.
    sessions: BTreeSet<String>,
    /// Process groups of check commands still running.
    check_groups: BTreeSet<u32>,
    /// Every session's cancel signal, fired on Ctrl+C or SIGTERM.
    cancels: Vec<crate::agent::InterruptSignal>,
}

fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(Mutex::default)
}

fn with_registry<T>(f: impl FnOnce(&mut Registry) -> T) -> T {
    let mut guard = registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(&mut guard)
}

/// Record a session so its background tasks are stopped with the run, and
/// so Ctrl+C can cancel its turn.
pub fn track_session(session_id: &str, cancel: crate::agent::InterruptSignal) {
    with_registry(|reg| {
        reg.sessions.insert(session_id.to_string());
        reg.cancels.push(cancel);
    });
}

/// Record a running check's process group until [`check_finished`].
pub fn check_started(pgid: u32) {
    with_registry(|reg| {
        reg.check_groups.insert(pgid);
    });
}

pub fn check_finished(pgid: u32) {
    with_registry(|reg| {
        reg.check_groups.remove(&pgid);
    });
}

/// Stop the background tasks one session left running. A helper's command
/// that ran past its foreground time limit keeps running as a background
/// task after the helper's turn ends; nothing will ever wait for it.
/// Returns how many tasks were stopped.
pub async fn stop_session_tasks(session_id: &str) -> usize {
    stop_session_tasks_in(crate::background::global(), session_id).await
}

pub(crate) async fn stop_session_tasks_in(
    manager: &crate::background::BackgroundTaskManager,
    session_id: &str,
) -> usize {
    let mut stopped = 0;
    for task in manager.list().await {
        if task.session_id == session_id
            && task.status == crate::bus::BackgroundTaskStatus::Running
            && manager
                .cancel_with_grace(&task.task_id, GRACE)
                .await
                .unwrap_or(false)
        {
            stopped += 1;
        }
    }
    stopped
}

/// Stop everything the run started: cancel every session's turn, stop every
/// session's background tasks, and kill every running check's process group.
/// Safe to call more than once.
///
/// A helper command still in the foreground is not a background task yet.
/// Cancelling the turn makes the bash tool hand it to the background manager:
/// the tool polls the shutdown signal every 100 ms, and the agent gives a
/// bash call up to 750 ms to finish that hand-off. When the run was cut off
/// (`interrupted`), a turn may still be running, so the sweep runs again
/// after [`HANDOFF_WAIT`] to catch it. After a normal finish every turn has
/// ended and its leftovers were swept, so one sweep is enough.
pub async fn stop_all(interrupted: bool) -> usize {
    let (sessions, groups, cancels) = with_registry(|reg| {
        (
            reg.sessions.iter().cloned().collect::<Vec<_>>(),
            std::mem::take(&mut reg.check_groups),
            std::mem::take(&mut reg.cancels),
        )
    });
    for cancel in &cancels {
        cancel.fire();
    }
    #[cfg(unix)]
    for pgid in groups {
        let _ = crate::platform::signal_detached_process_group(pgid, libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = groups;
    let mut stopped = 0;
    let passes = if interrupted { 2 } else { 1 };
    for pass in 0..passes {
        if pass > 0 {
            tokio::time::sleep(HANDOFF_WAIT).await;
        }
        for session in &sessions {
            stopped += stop_session_tasks(session).await;
        }
    }
    stopped
}

/// Wait for Ctrl+C or SIGTERM. Never returns on platforms without them.
pub async fn interrupted() -> &'static str {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match (
            signal(SignalKind::interrupt()),
            signal(SignalKind::terminate()),
        ) {
            (Ok(mut interrupt), Ok(mut terminate)) => tokio::select! {
                _ = interrupt.recv() => "Ctrl+C",
                _ = terminate.recv() => "SIGTERM",
            },
            _ => std::future::pending().await,
        }
    }
    #[cfg(not(unix))]
    {
        match tokio::signal::ctrl_c().await {
            Ok(()) => "Ctrl+C",
            Err(_) => std::future::pending().await,
        }
    }
}

/// Exit code for a run stopped by a signal, as shells report it.
pub fn signal_exit_code(signal: &str) -> i32 {
    match signal {
        "SIGTERM" => 143,
        _ => 130,
    }
}

#[cfg(test)]
pub(crate) fn tracked_check_groups() -> Vec<u32> {
    with_registry(|reg| reg.check_groups.iter().copied().collect())
}
