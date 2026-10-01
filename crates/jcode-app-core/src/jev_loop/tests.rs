//! Unit tests for the jev-loop port. No network: Claude and Jev are mocked.

use super::claude::{AttemptRecord, ClaudeCalls, StepLogEntry};
use super::config::LoopConfig;
use super::engine::{RunOptions, run};
use super::fork::{DecidedBy, DecisionTransport, JevLayer, Route, parse_choice};
use super::report::{
    HelperReport, Judgment, Revision, RevisionAction, last_json_object, parse_helper_report,
    parse_plan, parse_revision,
};
use super::{Outcome, StepSpec};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Map, Value, json};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------- helpers

fn step(id: u32) -> StepSpec {
    StepSpec {
        id,
        task: format!("task {id}"),
        done_when: format!("check {id} passes"),
    }
}

fn report(passed: bool) -> HelperReport {
    HelperReport {
        summary: if passed { "did it" } else { "broke" }.into(),
        files_changed: vec!["src/lib.rs".into()],
        check_command: "cargo test".into(),
        check_passed: passed,
        check_output_tail: if passed { "" } else { "error: boom" }.into(),
        blocker: String::new(),
    }
}

fn jev_answer(choice: &str, confidence: f64) -> Value {
    let mut probabilities = Map::new();
    for outcome in ["done", "retry", "escalate"] {
        let p = if outcome == choice {
            confidence
        } else {
            (1.0 - confidence) / 2.0
        };
        probabilities.insert(outcome.into(), json!(p));
    }
    json!({
        "answers": {"q": {"type": "choice", "choice": choice, "confidence": confidence,
                          "probabilities": probabilities}},
        "usage": {"input_tokens": 500, "output_tokens": 1}
    })
}

/// Jev mock: pops scripted responses, records every request.
type RecordedRequests = Arc<Mutex<Vec<(Value, Map<String, Value>)>>>;

struct MockJev {
    responses: Mutex<VecDeque<Result<Value>>>,
    requests: RecordedRequests,
}

impl MockJev {
    fn new(responses: Vec<Result<Value>>) -> (Self, RecordedRequests) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                responses: Mutex::new(responses.into()),
                requests: requests.clone(),
            },
            requests,
        )
    }
}

#[async_trait]
impl DecisionTransport for MockJev {
    async fn evaluate(&self, state: Value, questions: Map<String, Value>) -> Result<Value> {
        self.requests.lock().unwrap().push((state, questions));
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| Err(anyhow::anyhow!("no scripted jev response")))
    }
}

/// Claude mock: scripted helper reports, judgments, revisions.
#[derive(Default)]
struct MockClaude {
    plan: Vec<StepSpec>,
    helper_reports: Mutex<VecDeque<HelperReport>>,
    judgments: Mutex<VecDeque<Outcome>>,
    revisions: Mutex<VecDeque<Revision>>,
    calls: Mutex<Vec<String>>,
    helper_feedback: Mutex<Vec<String>>,
}

impl MockClaude {
    fn with(plan: Vec<StepSpec>, reports: Vec<HelperReport>) -> Self {
        Self {
            plan,
            helper_reports: Mutex::new(reports.into()),
            ..Self::default()
        }
    }
    fn judgments(self, judgments: Vec<Outcome>) -> Self {
        *self.judgments.lock().unwrap() = judgments.into();
        self
    }
    fn revisions(self, revisions: Vec<Revision>) -> Self {
        *self.revisions.lock().unwrap() = revisions.into();
        self
    }
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
    fn count(&self, name: &str) -> usize {
        self.calls()
            .iter()
            .filter(|call| call.as_str() == name)
            .count()
    }
}

#[async_trait]
impl ClaudeCalls for MockClaude {
    async fn make_plan(&self, _task: &str) -> Result<(Vec<StepSpec>, f64)> {
        self.calls.lock().unwrap().push("plan".into());
        Ok((self.plan.clone(), 0.10))
    }
    async fn run_helper(&self, _step: &StepSpec, feedback: &str) -> (HelperReport, f64) {
        self.calls.lock().unwrap().push("helper".into());
        self.helper_feedback.lock().unwrap().push(feedback.into());
        let report = self
            .helper_reports
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| report(false));
        (report, 0.25)
    }
    async fn judge(&self, _step: &StepSpec, _r: &HelperReport, _a: u32) -> (Judgment, f64) {
        self.calls.lock().unwrap().push("judge".into());
        let decision = self
            .judgments
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Outcome::Escalate);
        (
            Judgment {
                decision,
                reason: "opus says so".into(),
            },
            0.05,
        )
    }
    async fn revise(&self, _step: &StepSpec, _h: &[AttemptRecord]) -> (Revision, f64) {
        self.calls.lock().unwrap().push("revise".into());
        let revision = self
            .revisions
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Revision {
                action: RevisionAction::Stop,
                task: String::new(),
                done_when: String::new(),
                reason: "human needed".into(),
            });
        (revision, 0.05)
    }
    async fn review(&self, _task: &str, step_log: &[StepLogEntry]) -> (String, f64) {
        self.calls.lock().unwrap().push("review".into());
        (format!("reviewed {} steps", step_log.len()), 0.05)
    }
}

struct Run {
    summary: super::RunSummary,
    output: String,
    forks: Vec<Value>,
}

async fn run_loop(claude: &MockClaude, jev: JevLayer) -> Run {
    let dir = tempfile::tempdir().unwrap();
    let fork_log = dir.path().join("logs").join("forks.jsonl");
    let options = RunOptions {
        task: "do the thing".into(),
        config: LoopConfig {
            fork_log: fork_log.clone(),
            ..LoopConfig::default()
        },
    };
    let mut output = Vec::<u8>::new();
    let summary = run(&options, claude, &jev, &mut output).await.unwrap();
    let forks = std::fs::read_to_string(&fork_log)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    Run {
        summary,
        output: String::from_utf8(output).unwrap(),
        forks,
    }
}

fn jev_with(responses: Vec<Result<Value>>) -> JevLayer {
    JevLayer::new(Some(Box::new(MockJev::new(responses).0)), 0.80)
}

// ------------------------------------------------------------ fork routing

#[tokio::test]
async fn sharp_done_with_passing_check_skips_the_planner() {
    let claude = MockClaude::with(vec![step(1)], vec![report(true)]);
    let run = run_loop(&claude, jev_with(vec![Ok(jev_answer("done", 0.95))])).await;

    assert!(run.summary.finished);
    assert_eq!(claude.count("judge"), 0, "sharp forks never call Opus");
    assert_eq!((run.summary.stats.sharp, run.summary.stats.split), (1, 0));
    assert_eq!(run.forks.len(), 1);
    assert_eq!(run.forks[0]["route"], "sharp");
    assert_eq!(run.forks[0]["decided_by"], "jev");
    assert_eq!(run.forks[0]["final_decision"], "done");
    assert_eq!(run.forks[0]["answer"], "done");
    assert_eq!(run.summary.stats.jev_tokens, 500);
}

#[tokio::test]
async fn low_confidence_splits_to_the_planner() {
    let claude = MockClaude::with(vec![step(1)], vec![report(true)]).judgments(vec![Outcome::Done]);
    let run = run_loop(&claude, jev_with(vec![Ok(jev_answer("done", 0.6))])).await;

    assert!(run.summary.finished);
    assert_eq!(claude.count("judge"), 1);
    assert_eq!(run.forks[0]["route"], "split");
    assert_eq!(run.forks[0]["decided_by"], "opus");
    assert_eq!(
        run.forks[0]["answer"], "done",
        "jev's unsure answer is still logged"
    );
    assert_eq!(run.forks[0]["opus_reason"], "opus says so");
}

#[tokio::test]
async fn threshold_is_inclusive() {
    let claude = MockClaude::with(vec![step(1)], vec![report(true)]);
    let run = run_loop(&claude, jev_with(vec![Ok(jev_answer("done", 0.80))])).await;
    assert_eq!(run.forks[0]["route"], "sharp");
    assert_eq!(claude.count("judge"), 0);
}

#[tokio::test]
async fn jev_saying_done_on_a_failed_check_is_overruled() {
    let claude = MockClaude::with(vec![step(1)], vec![report(false), report(true)])
        .judgments(vec![Outcome::Retry, Outcome::Done]);
    let run = run_loop(
        &claude,
        jev_with(vec![
            Ok(jev_answer("done", 0.99)),
            Ok(jev_answer("done", 0.3)),
        ]),
    )
    .await;

    assert_eq!(run.forks[0]["route"], "split");
    assert_eq!(
        run.forks[0]["note"],
        "overruled: jev said done but the check failed"
    );
    assert_eq!(run.forks[0]["decided_by"], "opus");
    assert_eq!(run.forks[0]["final_decision"], "retry");
    assert_eq!(run.summary.stats.rule_overrides, 1);
    assert!(run.summary.finished);
}

#[tokio::test]
async fn the_planner_cannot_accept_a_failed_check_either() {
    // Jev is unsure; Opus wrongly says done while the check failed. The
    // README's rule ("never accepted as done if its check failed") binds
    // everyone, so code turns it into a retry. The retry then passes.
    let claude = MockClaude::with(vec![step(1)], vec![report(false), report(true)])
        .judgments(vec![Outcome::Done]);
    let run = run_loop(
        &claude,
        jev_with(vec![
            Ok(jev_answer("done", 0.4)),
            Ok(jev_answer("done", 0.95)),
        ]),
    )
    .await;

    assert_eq!(claude.count("helper"), 2, "the failed attempt is retried");
    assert_eq!(run.forks[0]["route"], "split");
    assert_eq!(run.forks[0]["decided_by"], "rule");
    assert_eq!(run.forks[0]["final_decision"], "retry");
    assert_eq!(
        run.forks[0]["opus_reason"],
        "overruled: opus said done but the check failed (opus says so)"
    );
    assert_eq!(run.forks[1]["decided_by"], "jev");
    assert_eq!(run.summary.stats.rule_overrides, 1);
    assert!(run.summary.finished);
}

#[tokio::test]
async fn an_opus_done_on_a_failed_final_attempt_escalates() {
    // The rule override (done -> retry) still respects the attempt cap.
    let claude = MockClaude::with(vec![step(1)], vec![report(false); 3]).judgments(vec![
        Outcome::Retry,
        Outcome::Retry,
        Outcome::Done,
    ]);
    let run = run_loop(&claude, JevLayer::new(None, 0.8)).await;
    assert_eq!(run.forks[2]["final_decision"], "escalate");
    assert_eq!(run.forks[2]["decided_by"], "rule");
    assert_eq!(run.summary.stats.rule_overrides, 2);
    assert!(!run.summary.finished);
}

#[tokio::test]
async fn jev_errors_and_invalid_answers_fall_back_to_the_planner() {
    let bad_answer = json!({"answers": {"q": {"type": "choice", "choice": "ship_it",
        "confidence": 0.99, "probabilities": {"done": 0.01, "retry": 0.0, "ship_it": 0.99}}}});
    let claude = MockClaude::with(vec![step(1), step(2)], vec![report(true), report(true)])
        .judgments(vec![Outcome::Done, Outcome::Done]);
    let run = run_loop(
        &claude,
        jev_with(vec![Err(anyhow::anyhow!("HTTP 503")), Ok(bad_answer)]),
    )
    .await;

    assert!(run.summary.finished);
    assert_eq!(claude.count("judge"), 2);
    for fork in &run.forks {
        assert_eq!(fork["route"], "split");
        assert!(fork["answer"].is_null());
        assert!(fork["note"].as_str().unwrap().starts_with("jev error:"));
    }
}

#[tokio::test]
async fn no_jev_baseline_sends_every_fork_to_the_planner() {
    let claude = MockClaude::with(vec![step(1)], vec![report(true)]).judgments(vec![Outcome::Done]);
    let run = run_loop(&claude, JevLayer::new(None, 0.8)).await;

    assert!(run.summary.finished);
    assert_eq!(run.forks[0]["note"], "jev turned off (--no-jev)");
    assert_eq!(run.summary.stats.opus_fork_calls, 1);
    assert_eq!(run.summary.stats.jev_tokens, 0);
}

// ------------------------------------------------------------ rules in code

#[tokio::test]
async fn retries_are_capped_then_escalated_by_rule() {
    // Jev confidently says retry forever; the cap must win at attempt 3.
    let claude = MockClaude::with(vec![step(1)], vec![report(false); 3]);
    let run = run_loop(
        &claude,
        jev_with(vec![
            Ok(jev_answer("retry", 0.9)),
            Ok(jev_answer("retry", 0.9)),
            Ok(jev_answer("retry", 0.9)),
        ]),
    )
    .await;

    assert_eq!(claude.count("helper"), 3);
    assert_eq!(run.forks[2]["decided_by"], "rule");
    assert_eq!(run.forks[2]["final_decision"], "escalate");
    assert_eq!(run.forks[2]["opus_reason"], "hit 3 attempts");
    // Escalation asks Opus for a revision; the mock stops the run.
    assert_eq!(claude.count("revise"), 1);
    assert!(!run.summary.finished);
    assert_eq!(run.summary.exit_code(), 2);
}

#[tokio::test]
async fn retry_feedback_carries_the_failing_check_output() {
    let claude = MockClaude::with(vec![step(1)], vec![report(false), report(true)]);
    run_loop(
        &claude,
        jev_with(vec![
            Ok(jev_answer("retry", 0.9)),
            Ok(jev_answer("done", 0.9)),
        ]),
    )
    .await;
    let feedback = claude.helper_feedback.lock().unwrap().clone();
    assert_eq!(feedback[0], "", "first attempt has no feedback");
    assert!(feedback[1].contains("Check `cargo test` failed."));
    assert!(feedback[1].contains("error: boom"));
}

#[tokio::test]
async fn a_stuck_step_is_revised_once_then_the_run_stops() {
    let revised = Revision {
        action: RevisionAction::Revise,
        task: "smaller task".into(),
        done_when: "unit test passes".into(),
        reason: "split it".into(),
    };
    let claude = MockClaude::with(vec![step(1), step(2)], vec![report(false), report(false)])
        .revisions(vec![revised]);
    let run = run_loop(
        &claude,
        jev_with(vec![
            Ok(jev_answer("escalate", 0.9)),
            Ok(jev_answer("escalate", 0.9)),
        ]),
    )
    .await;

    assert_eq!(claude.count("revise"), 1, "only one revision is allowed");
    assert_eq!(
        claude.count("helper"),
        2,
        "the revised step gets a fresh attempt"
    );
    assert!(!run.summary.finished);
    assert!(run.output.contains("Opus rewrote the step: split it"));
    assert!(
        run.output
            .contains("stopping: step 1 is still stuck after a revision")
    );
    // The review still runs, and step 2 never started.
    assert_eq!(claude.count("review"), 1);
    assert!(run.summary.review.contains("reviewed 1 steps"));
}

#[tokio::test]
async fn planner_stop_ends_the_run_with_its_reason() {
    let claude = MockClaude::with(vec![step(1)], vec![report(false)]);
    let run = run_loop(&claude, jev_with(vec![Ok(jev_answer("escalate", 0.95))])).await;
    assert!(!run.summary.finished);
    assert!(run.output.contains("Opus stopped the run: human needed"));
}

#[tokio::test]
async fn summary_matches_the_prototype_layout_and_costs() {
    let claude = MockClaude::with(vec![step(1)], vec![report(true)]);
    let run = run_loop(&claude, jev_with(vec![Ok(jev_answer("done", 0.95))])).await;
    for label in [
        "--- run summary ---",
        "finished all steps:  true",
        "forks:               1  (sharp 1, split 0)",
        "Opus fork calls:     0",
        "rule overrides:      0",
        "Claude cost (est.):  $0.40",
        "Jev cost (est.):     $0.000021",
        "fork log:",
    ] {
        assert!(
            run.output.contains(label),
            "missing {label:?} in:\n{}",
            run.output
        );
    }
    assert_eq!(claude.calls(), ["plan", "helper", "review"]);
}

// --------------------------------------------------------- jev request shape

#[tokio::test]
async fn jev_request_uses_the_step_outcome_choice_contract() {
    let (mock, requests) = MockJev::new(vec![Ok(jev_answer("done", 0.95))]);
    let layer = JevLayer::new(Some(Box::new(mock)), 0.8);
    let long_output = format!("{}END", "x".repeat(5000));
    let helper = HelperReport {
        check_output_tail: long_output,
        ..report(true)
    };
    let fork = layer.step_outcome(&step(1), &helper, 2, "prev").await;
    assert_eq!(fork.route, Route::Sharp);

    let requests = requests.lock().unwrap();
    let (state, questions) = &requests[0];
    let question = &questions["q"];
    assert_eq!(question["type"], "choice");
    let criteria = question["criteria"].as_object().unwrap();
    assert_eq!(
        criteria.keys().collect::<Vec<_>>(),
        ["done", "escalate", "retry"]
    );
    assert_eq!(state["attempt_number"], 2);
    assert_eq!(state["previous_attempt_error_end"], "prev");
    let tail = state["check_output_end"].as_str().unwrap();
    assert!(tail.starts_with("...") && tail.ends_with("END"));
    assert_eq!(
        tail.chars().count(),
        3003,
        "errors live at the end: keep the tail"
    );
}

#[test]
fn choice_parsing_rejects_inconsistent_answers() {
    assert!(parse_choice(&jev_answer("retry", 0.7)).is_ok());
    let cases = [
        json!({"answers": {}}),
        json!({"answers": {"q": {"type": "noul", "noul": 0.9}}}),
        json!({"answers": {"q": {"type": "choice", "choice": "done", "confidence": 1.5,
            "probabilities": {"done": 1.0, "retry": 0.0, "escalate": 0.0}}}}),
        // Missing an option.
        json!({"answers": {"q": {"type": "choice", "choice": "done", "confidence": 0.9,
            "probabilities": {"done": 0.9, "retry": 0.1}}}}),
        // Does not sum to one.
        json!({"answers": {"q": {"type": "choice", "choice": "done", "confidence": 0.9,
            "probabilities": {"done": 0.9, "retry": 0.9, "escalate": 0.9}}}}),
        // Choice disagrees with its own distribution.
        json!({"answers": {"q": {"type": "choice", "choice": "done", "confidence": 0.9,
            "probabilities": {"done": 0.1, "retry": 0.8, "escalate": 0.1}}}}),
    ];
    for case in cases {
        assert!(parse_choice(&case).is_err(), "should reject {case}");
    }
}

// --------------------------------------------------------------- reports

#[test]
fn every_session_is_told_to_leave_changes_uncommitted() {
    // The prototype ran every session on Claude Code's preset, which never
    // commits unless asked; the loop's review-and-undo model depends on it.
    let prompt = super::claude::system_prompt("RULES", std::path::Path::new("/repo"));
    assert!(prompt.contains("/repo"));
    assert!(prompt.contains("Never create commits"));
    assert!(prompt.contains("leave every change uncommitted"));
    assert!(prompt.trim_end().ends_with("RULES"));
}

#[test]
fn turn_meter_sums_usage_and_keeps_only_the_final_response_text() {
    use super::claude::{TurnMeter, UsageTotals, session_cost};
    use crate::protocol::ServerEvent;

    let mut meter = TurnMeter::default();
    let usage = |input, output, read, write| ServerEvent::TokenUsage {
        input,
        output,
        cache_read_input: Some(read),
        cache_creation_input: Some(write),
    };
    // First response: some text, then a tool call (a new response follows).
    assert!(!meter.observe(ServerEvent::TextDelta {
        text: "Let me look.".into()
    }));
    assert!(meter.observe(usage(1_000, 200, 0, 5_000)));
    meter.observe(ServerEvent::ToolStart {
        id: "t1".into(),
        name: "read".into(),
    });
    // Final response carries the report.
    meter.observe(ServerEvent::TextDelta {
        text: "{\"summary\": ".into(),
    });
    meter.observe(ServerEvent::TextDelta {
        text: "\"ok\"}".into(),
    });
    assert!(meter.observe(usage(300, 100, 5_000, 0)));

    assert_eq!(meter.text, "{\"summary\": \"ok\"}");
    assert_eq!(meter.declined, None);
    // A declined turn is recorded with the provider's explanation.
    let mut declined = TurnMeter::default();
    declined.observe(ServerEvent::ProviderGuardrail {
        stop_reason: Some("refusal".into()),
        message: "The model declined to answer this request.".into(),
    });
    assert_eq!(
        declined.declined.as_deref(),
        Some("The model declined to answer this request.")
    );
    assert!(declined.text.is_empty());
    assert_eq!(
        meter.usage,
        UsageTotals {
            input: 1_300,
            output: 300,
            cache_read: 5_000,
            cache_creation: 5_000,
        }
    );
    // Opus 5.5 is in jcode's static API price table ($4 in / $20 out / $0.20
    // read, cache writes at 1.25x or 2x input), so this needs no network.
    let cost = session_cost("claude:api-key", "claude-opus-5-5", &meter.usage);
    let fixed = 1_300.0 * 4e-6 + 300.0 * 20e-6 + 5_000.0 * 0.2e-6;
    let write = 5_000.0 * 4e-6;
    assert!(
        (cost - (fixed + write * 1.25)).abs() < 1e-9 || (cost - (fixed + write * 2.0)).abs() < 1e-9,
        "unexpected cost {cost}"
    );
    // Unknown routes are unpriced rather than a misleading guess.
    assert_eq!(session_cost("no-such-route", "mystery", &meter.usage), 0.0);
}

/// A session that ended without text reports its own error (for example the
/// model declining) instead of a JSON parse error, for every role.
#[test]
fn an_empty_session_reports_why_instead_of_a_parse_error() {
    use super::claude::{SessionResult, session_failure};

    let declined = SessionResult {
        error: Some("The model declined to answer this request.".into()),
        ..SessionResult::default()
    };
    let parse_error = || anyhow::anyhow!("no JSON object in the reply");
    assert_eq!(
        session_failure(&declined, parse_error()),
        "The model declined to answer this request."
    );
    // With text present, the parse error is the real problem.
    let chatty = SessionResult {
        text: "Sure, here is my answer without JSON.".into(),
        error: Some("unrelated".into()),
        ..SessionResult::default()
    };
    assert_eq!(
        session_failure(&chatty, parse_error()),
        "no JSON object in the reply"
    );
}

#[test]
fn reports_are_extracted_from_the_last_json_object() {
    let text = "I ran it.\n```json\n{\"summary\": \"old\"}\n```\nFinal:\n\
        {\"summary\": \"added } brace\", \"files_changed\": [], \"check_command\": \"make\",\
         \"check_passed\": true, \"check_output_tail\": \"ok {\", \"blocker\": \"\"}";
    let parsed = parse_helper_report(text).unwrap();
    assert_eq!(parsed.summary, "added } brace");
    assert!(parsed.check_passed);
    assert!(last_json_object("no json here").is_none());
    assert!(parse_helper_report("{\"files_changed\": []}").is_err());
}

#[test]
fn plans_and_revisions_are_validated() {
    let plan = parse_plan(
        "{\"steps\": [{\"id\": 1, \"task\": \"a\", \"done_when\": \"b\"},\
          {\"id\": 2, \"task\": \"c\", \"done_when\": \"d\"}]}",
    )
    .unwrap();
    assert_eq!(plan.len(), 2);
    assert!(parse_plan("{\"steps\": []}").is_err());
    assert!(
        parse_plan("{\"steps\": [{\"id\": 1, \"task\": \"a\", \"done_when\": \"\"}]}").is_err()
    );
    assert!(
        parse_revision("{\"action\": \"revise\", \"task\": \"\", \"done_when\": \"x\"}").is_err()
    );
    let stop = parse_revision("{\"action\": \"stop\", \"reason\": \"why\"}").unwrap();
    assert_eq!(stop.action, RevisionAction::Stop);
}

#[test]
fn helper_fallback_keeps_the_end_of_the_output() {
    let report = HelperReport::stopped(
        &format!("{}TAIL", "y".repeat(3000)),
        "Session ended with: cap",
    );
    assert!(!report.check_passed);
    assert!(report.check_output_tail.ends_with("TAIL"));
    assert_eq!(report.summary, "Helper stopped before reporting.");
    assert_eq!(report.blocker, "Session ended with: cap");
}

// ---------------------------------------------------------------- config

#[test]
fn config_defaults_match_the_prototype_and_overrides_are_validated() {
    let defaults = LoopConfig::default();
    assert_eq!(defaults.planner_model, "claude-opus-5-5");
    assert_eq!(defaults.helper_model, "claude-sonnet-5-5");
    assert_eq!(defaults.jev_confidence_threshold, 0.80);
    assert_eq!(defaults.max_attempts_per_step, 3);
    assert_eq!(defaults.max_revisions_per_step, 1);
    assert_eq!(defaults.helper_max_tool_calls, 40);
    assert_eq!(defaults.step_budget_usd, 2.0);

    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |key: &str| -> Result<Option<String>> {
            Ok(pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| v.to_string()))
        }
    };
    let tuned = LoopConfig::from_lookup(
        env(&[
            ("JCODE_JEV_LOOP_THRESHOLD", "0.7"),
            ("JCODE_JEV_LOOP_MAX_ATTEMPTS", "5"),
        ]),
        "/tmp/f.jsonl".into(),
    )
    .unwrap();
    assert_eq!(tuned.jev_confidence_threshold, 0.7);
    assert_eq!(tuned.max_attempts_per_step, 5);
    assert_eq!(tuned.fork_log, std::path::PathBuf::from("/tmp/f.jsonl"));

    for bad in [
        &[("JCODE_JEV_LOOP_THRESHOLD", "1.5")][..],
        &[("JCODE_JEV_LOOP_MAX_ATTEMPTS", "0")][..],
        &[("JCODE_JEV_LOOP_STEP_BUDGET_USD", "-1")][..],
    ] {
        assert!(
            LoopConfig::from_lookup(env(bad), "f".into()).is_err(),
            "{bad:?}"
        );
    }
}

#[test]
fn decided_by_serializes_like_the_prototype() {
    assert_eq!(serde_json::to_value(DecidedBy::Jev).unwrap(), "jev");
    assert_eq!(serde_json::to_value(DecidedBy::Opus).unwrap(), "opus");
    assert_eq!(serde_json::to_value(DecidedBy::Rule).unwrap(), "rule");
    assert_eq!(
        serde_json::to_value(Judgment {
            decision: Outcome::Retry,
            reason: String::new()
        })
        .unwrap()["decision"],
        "retry"
    );
}
