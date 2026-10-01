//! A read-only loop session that runs out of tool calls still answers.
//!
//! The scripted model below behaves like the capped judges seen in real runs:
//! it keeps reading files and only gives its decision once a tool result tells
//! it to stop. Before the answer grace, the call over the cap cancelled the
//! turn, so the judge returned nothing and the loop recorded "judge gave no
//! usable decision". Each test runs the real agent and the real guarded
//! registry under a throwaway `JCODE_HOME`.

use super::StepSpec;
use super::claude::{ClaudeCalls, JcodeClaude};
use super::config::{self, LoopConfig};
use super::report::HelperReport;
use crate::message::{ContentBlock, Message, StreamEvent, ToolDefinition};
use crate::provider::{EventStream, Provider};
use anyhow::Result;
use async_trait::async_trait;
use std::sync::{Arc, Mutex};

/// Reads `notes.txt` on every response until the latest tool result asks for
/// an answer, then replies with a judge decision. `stubborn` never answers.
#[derive(Clone)]
struct ReadsUntilToldProvider {
    model: Arc<Mutex<String>>,
    requests: Arc<Mutex<u32>>,
    stubborn: bool,
}

impl ReadsUntilToldProvider {
    fn new(stubborn: bool) -> Self {
        Self {
            model: Arc::new(Mutex::new("claude-opus-5-5".into())),
            requests: Arc::new(Mutex::new(0)),
            stubborn,
        }
    }
}

fn told_to_answer(messages: &[Message]) -> bool {
    messages.last().is_some_and(|message| {
        message.content.iter().any(|block| {
            matches!(block, ContentBlock::ToolResult { content, .. }
                if content.contains("reply now with your answer"))
        })
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
        let events = if told_to_answer(messages) && !self.stubborn {
            vec![
                StreamEvent::TextDelta(
                    "{\"decision\": \"done\", \"reason\": \"read enough\"}".into(),
                ),
                StreamEvent::MessageEnd {
                    stop_reason: Some("end_turn".into()),
                },
            ]
        } else {
            vec![
                StreamEvent::ToolUseStart {
                    id: format!("read-{call}"),
                    name: "read".into(),
                },
                StreamEvent::ToolInputDelta("{\"file_path\": \"notes.txt\"}".into()),
                StreamEvent::ToolUseEnd,
                StreamEvent::MessageEnd {
                    stop_reason: Some("tool_use".into()),
                },
            ]
        };
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

async fn judge_with(provider: &ReadsUntilToldProvider) -> super::report::Judgment {
    let repo = tempfile::tempdir().expect("repo dir");
    std::fs::write(repo.path().join("notes.txt"), "a note\n").expect("write notes");
    let claude = JcodeClaude::new(
        Arc::new(provider.clone()),
        repo.path().to_path_buf(),
        LoopConfig::default(),
    );
    let step = StepSpec {
        id: 1,
        task: "Add a test".into(),
        done_when: "the test passes".into(),
    };
    let report = HelperReport {
        summary: "added the test".into(),
        files_changed: vec!["test_x.py".into()],
        check_command: "pytest -q".into(),
        check_passed: true,
        check_output_tail: "1 passed".into(),
        blocker: String::new(),
    };
    let (judgment, _cost) = claude.judge(&step, &report, 1).await;
    judgment
}

// The env lock is held across the judge's awaits on purpose: it keeps other
// tests from changing JCODE_HOME while a session is being saved.
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn capped_judge_still_returns_its_decision() {
    let _env = crate::storage::lock_test_env();
    let _home = IsolatedHome::new();
    let provider = ReadsUntilToldProvider::new(false);

    let judgment = judge_with(&provider).await;

    assert_eq!(
        judgment.decision,
        super::Outcome::Done,
        "the judge must answer after its cap: {}",
        judgment.reason
    );
    assert_eq!(judgment.reason, "read enough");
    // 8 reads, 1 refused call carrying "reply now", then the answer.
    assert_eq!(
        *provider.requests.lock().unwrap(),
        config::JUDGE_MAX_TOOL_CALLS + 2
    );
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn judge_that_never_stops_reading_is_still_stopped() {
    let _env = crate::storage::lock_test_env();
    let _home = IsolatedHome::new();
    let provider = ReadsUntilToldProvider::new(true);

    let judgment = judge_with(&provider).await;

    assert_eq!(judgment.decision, super::Outcome::Escalate);
    assert!(
        judgment.reason.contains("judge gave no usable decision"),
        "{}",
        judgment.reason
    );
    // The cap plus the grace, and then the turn is cut off.
    let requests = *provider.requests.lock().unwrap();
    assert!(
        requests <= config::JUDGE_MAX_TOOL_CALLS + config::ANSWER_GRACE_TOOL_CALLS + 1,
        "{requests} requests"
    );
}
