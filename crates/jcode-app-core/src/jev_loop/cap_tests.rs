//! A read-only loop session that runs out of tool calls still answers.
//!
//! The scripted model below behaves like the capped judges seen in real runs:
//! it keeps reading files and only gives its answer once a tool result tells
//! it to stop. Before the answer grace, the call over the cap cancelled the
//! turn, so the judge returned nothing and the loop recorded "judge gave no
//! usable decision". Each test drives the real agent and the real guarded
//! registry through `JcodeClaude`, under a throwaway `JCODE_HOME`.

use super::StepSpec;
use super::claude::{ClaudeCalls, JcodeClaude, StepLogEntry};
use super::config::{self, LoopConfig};
use super::report::HelperReport;
use crate::message::{ContentBlock, Message, StreamEvent, ToolDefinition};
use crate::provider::{EventStream, Provider};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::{Arc, Mutex};

/// Each response reads `notes.txt` `batch` times (parallel tool calls),
/// until the latest tool results ask for an answer; then it replies with
/// `answer`. A `stubborn` model never answers.
#[derive(Clone)]
struct ReadsUntilToldProvider {
    model: Arc<Mutex<String>>,
    requests: Arc<Mutex<u32>>,
    batch: u32,
    answer: &'static str,
    stubborn: bool,
}

impl ReadsUntilToldProvider {
    fn new(batch: u32, answer: &'static str, stubborn: bool) -> Self {
        Self {
            model: Arc::new(Mutex::new("claude-opus-5-5".into())),
            requests: Arc::new(Mutex::new(0)),
            batch,
            answer,
            stubborn,
        }
    }

    fn requests(&self) -> u32 {
        *self.requests.lock().unwrap()
    }
}

/// True when any tool result since the model's last response carries the
/// guard's "reply now" refusal.
fn told_to_answer(messages: &[Message]) -> bool {
    messages
        .iter()
        .rev()
        .take_while(|message| message.role != crate::message::Role::Assistant)
        .flat_map(|message| message.content.iter())
        .any(|block| {
            matches!(block, ContentBlock::ToolResult { content, .. }
                if content.contains("reply now with your answer"))
        })
}

#[async_trait]
impl Provider for ReadsUntilToldProvider {
    async fn complete(
        &self,
        messages: &[Message],
        _tools: &[ToolDefinition],
        _system: &str,
        _resume_session_id: Option<&str>,
    ) -> Result<EventStream> {
        let call = {
            let mut requests = self.requests.lock().unwrap();
            *requests += 1;
            *requests
        };
        let mut events = Vec::new();
        if told_to_answer(messages) && !self.stubborn {
            events.push(StreamEvent::TextDelta(self.answer.into()));
            events.push(StreamEvent::MessageEnd {
                stop_reason: Some("end_turn".into()),
            });
        } else {
            for n in 0..self.batch {
                events.extend([
                    StreamEvent::ToolUseStart {
                        id: format!("read-{call}-{n}"),
                        name: "read".into(),
                    },
                    StreamEvent::ToolInputDelta("{\"file_path\": \"notes.txt\"}".into()),
                    StreamEvent::ToolUseEnd,
                ]);
            }
            events.push(StreamEvent::MessageEnd {
                stop_reason: Some("tool_use".into()),
            });
        }
        Ok(Box::pin(futures::stream::iter(events.into_iter().map(Ok))))
    }

    fn name(&self) -> &str {
        "jev-loop-cap-test"
    }

    fn model(&self) -> String {
        self.model.lock().unwrap().clone()
    }

    fn set_model(&self, model: &str) -> Result<()> {
        *self.model.lock().unwrap() = model.to_string();
        Ok(())
    }

    fn set_reasoning_effort(&self, _effort: &str) -> Result<()> {
        Ok(())
    }

    fn fork(&self) -> Arc<dyn Provider> {
        Arc::new(self.clone())
    }
}

/// Points `JCODE_HOME` at a temporary directory for the test's lifetime, so
/// the sessions these tests create never reach the user's `~/.jcode`.
struct IsolatedHome {
    _home: tempfile::TempDir,
    previous: Option<std::ffi::OsString>,
}

impl IsolatedHome {
    fn new() -> Self {
        let home = tempfile::tempdir().expect("temp JCODE_HOME");
        let previous = std::env::var_os("JCODE_HOME");
        crate::env::set_var("JCODE_HOME", home.path());
        Self {
            _home: home,
            previous,
        }
    }
}

impl Drop for IsolatedHome {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(value) => crate::env::set_var("JCODE_HOME", value),
            None => crate::env::remove_var("JCODE_HOME"),
        }
    }
}

const DECISION: &str = "{\"decision\": \"done\", \"reason\": \"read enough\"}";

fn claude_for(provider: &ReadsUntilToldProvider, repo: &std::path::Path) -> JcodeClaude {
    std::fs::write(repo.join("notes.txt"), "a note\n").expect("write notes");
    JcodeClaude::new(
        Arc::new(provider.clone()),
        repo.to_path_buf(),
        LoopConfig::default(),
    )
}

fn step() -> StepSpec {
    StepSpec {
        id: 1,
        task: "Add a test".into(),
        done_when: "the test passes".into(),
    }
}

fn passing_report() -> HelperReport {
    HelperReport {
        summary: "added the test".into(),
        files_changed: vec!["test_x.py".into()],
        check_command: "pytest -q".into(),
        check_passed: true,
        check_output_tail: "1 passed".into(),
        blocker: String::new(),
        ..HelperReport::default()
    }
}

async fn judge_with(provider: &ReadsUntilToldProvider) -> super::report::Judgment {
    let repo = tempfile::tempdir().expect("repo dir");
    let claude = claude_for(provider, repo.path());
    let (judgment, _cost) = claude.judge(&step(), &passing_report(), 1, "").await;
    judgment
}

// The env lock is held across the sessions' awaits on purpose: it keeps
// other tests from changing JCODE_HOME while a session is being saved.

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn capped_judge_still_returns_its_decision() {
    let _env = crate::storage::lock_test_env();
    let _home = IsolatedHome::new();
    let provider = ReadsUntilToldProvider::new(1, DECISION, false);

    let judgment = judge_with(&provider).await;

    assert_eq!(
        judgment.decision,
        super::Outcome::Done,
        "the judge must answer after its cap: {}",
        judgment.reason
    );
    assert_eq!(judgment.reason, "read enough");
    // 8 reads, 1 refused call carrying "reply now", then the answer.
    assert_eq!(provider.requests(), config::JUDGE_MAX_TOOL_CALLS + 2);
}

/// A model that sends many reads in one response must not use up the grace
/// before it has seen a single refusal. With 6 parallel reads per response,
/// the second response crosses the cap of 8 with 4 calls to spare; all of
/// them are refused with "reply now", and the third response answers.
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn judge_batching_reads_past_the_cap_still_answers() {
    let _env = crate::storage::lock_test_env();
    let _home = IsolatedHome::new();
    let provider = ReadsUntilToldProvider::new(6, DECISION, false);

    let judgment = judge_with(&provider).await;

    assert_eq!(
        judgment.decision,
        super::Outcome::Done,
        "a batching judge must still answer: {}",
        judgment.reason
    );
    assert_eq!(provider.requests(), 3);
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn judge_that_never_stops_reading_is_still_stopped() {
    let _env = crate::storage::lock_test_env();
    let _home = IsolatedHome::new();
    let provider = ReadsUntilToldProvider::new(1, DECISION, true);

    let judgment = judge_with(&provider).await;

    assert_eq!(judgment.decision, super::Outcome::Escalate);
    assert!(
        judgment.reason.contains("judge gave no usable decision"),
        "{}",
        judgment.reason
    );
    // 8 reads, then one refused response per grace slot, then the call that
    // stops the session.
    assert_eq!(
        provider.requests(),
        config::JUDGE_MAX_TOOL_CALLS + config::ANSWER_GRACE_RESPONSES + 1
    );
}

/// The final reviewer gets the same grace: a review that ran out of tool
/// calls still reaches the user instead of "(no review text)".
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn capped_reviewer_still_returns_its_review() {
    let _env = crate::storage::lock_test_env();
    let _home = IsolatedHome::new();
    let provider = ReadsUntilToldProvider::new(5, "Review: the change looks right.", false);
    let repo = tempfile::tempdir().expect("repo dir");
    let claude = claude_for(&provider, repo.path());
    let log = vec![StepLogEntry {
        step: step(),
        report: passing_report(),
        stopped: false,
        reason: String::new(),
    }];

    let (review, _cost) = claude.review("Add a test", &log).await;

    assert_eq!(review, "Review: the change looks right.");
}

/// A helper keeps the hard stop: its attempt ends at the cap and is reported
/// as stopped, with no grace for more calls.
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn helper_keeps_the_hard_stop_at_its_cap() {
    let _env = crate::storage::lock_test_env();
    let _home = IsolatedHome::new();
    let provider = ReadsUntilToldProvider::new(1, "{}", false);
    let repo = tempfile::tempdir().expect("repo dir");
    std::fs::write(repo.path().join("notes.txt"), "a note\n").expect("write notes");
    let claude = JcodeClaude::new(
        Arc::new(provider.clone()),
        repo.path().to_path_buf(),
        LoopConfig {
            helper_max_tool_calls: 3,
            ..LoopConfig::default()
        },
    );

    let (report, _cost) = claude.run_helper(&step(), "", "").await;

    assert!(!report.check_passed);
    assert_eq!(report.stopped, Some(super::report::Stop::Limit));
    assert!(
        report.blocker.contains("tool-call limit reached (3 calls)"),
        "{}",
        report.blocker
    );
    // 3 reads, then the 4th call stops the session: no grace response.
    assert_eq!(provider.requests(), 4);
}
