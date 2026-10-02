//! `jcode jev-loop`: a coding-agent loop with a Jev fork layer.
//!
//! The planner (Opus) breaks a task into steps. For each step a helper
//! (Sonnet) makes one attempt, the loop runs the helper's check command
//! itself, and a fork decides what happens next. Rules in code settle the
//! obvious cases (a failed check, a blocker, a stopped attempt). When the
//! check passed, Jev reviews the change against the step's `done_when`: code
//! accepts a confident yes ("sharp"), and the planner decides otherwise
//! ("split"). Jev also picks the files the planner and each helper should
//! start from. The planner reviews the whole change at the end.
//!
//! Policy lives in code: a check the loop did not see pass is never
//! accepted, attempts are capped, a stuck step may be rewritten once,
//! blocked commands are refused for every session, and any Jev failure falls
//! back to the planner.
//!
//! This is a port of the `jev-loop` Python prototype (slices 1 to 3).

pub mod claude;
pub mod config;
pub mod engine;
pub mod fork;
pub mod guard;
pub mod pick;
pub mod report;
pub mod stop;
pub mod workspace;

pub use claude::{ClaudeCalls, JcodeClaude};
pub use config::LoopConfig;
pub use engine::{Mode, Parts, RunOptions, RunSummary, run};
pub use fork::JevLayer;
pub use workspace::RepoWorkspace;

use serde::{Deserialize, Serialize};

/// One planned step.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StepSpec {
    pub id: u32,
    pub task: String,
    pub done_when: String,
}

/// What happens after a helper attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Done,
    Retry,
    Escalate,
}

impl Outcome {
    pub const ALL: [Outcome; 3] = [Outcome::Done, Outcome::Retry, Outcome::Escalate];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Retry => "retry",
            Self::Escalate => "escalate",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|outcome| outcome.as_str() == value)
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod guard_tests;

#[cfg(test)]
mod cap_tests;

#[cfg(test)]
mod workspace_tests;

#[cfg(test)]
mod stop_tests;
