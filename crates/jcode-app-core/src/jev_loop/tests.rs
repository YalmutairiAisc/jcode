//! Unit tests for the jev-loop port. No network: Claude, Jev, and the
//! repository are mocked.

use super::claude::{AttemptRecord, ClaudeCalls, StepLogEntry};
use super::config::LoopConfig;
use super::engine::{Mode, Parts, RunOptions, run};
use super::fork::{DecidedBy, DecisionTransport, JevLayer, Route};
use super::pick::Candidate;
use super::report::{
    HelperReport, Judgment, LoopCheck, Revision, RevisionAction, Stop, last_json_object,
    parse_helper_report, parse_plan, parse_revision,
};
use super::workspace::{Snapshot, Workspace};
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

/// What the helper claims. The loop's own check is scripted separately.
fn report(passed: bool) -> HelperReport {
    HelperReport {
        summary: if passed { "did it" } else { "broke" }.into(),
        files_changed: vec!["src/lib.rs".into()],
        check_command: "cargo test".into(),
        check_passed: passed,
        check_output_tail: if passed { "" } else { "error: boom" }.into(),
        blocker: String::new(),
        ..HelperReport::default()
    }
}

fn passed() -> LoopCheck {
    LoopCheck::finished(true, Some(0), "test result: ok".into(), 1.0)
}

fn failed(output: &str) -> LoopCheck {
    LoopCheck::finished(false, Some(1), output.into(), 1.0)
}

/// Jev's answer to the review question: P(the change meets done_when).
fn meets(probability: f64) -> Value {
    json!({
        "answers": {"meets": {"type": "noul", "noul": probability}},
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
    hints: Mutex<Vec<String>>,
    judge_diffs: Mutex<Vec<String>>,
    revise_histories: Mutex<Vec<Vec<AttemptRecord>>>,
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
    async fn make_plan(&self, _task: &str, hint: &str) -> Result<(Vec<StepSpec>, f64)> {
        self.calls.lock().unwrap().push("plan".into());
        self.hints.lock().unwrap().push(format!("plan:{hint}"));
        Ok((self.plan.clone(), 0.10))
    }
    async fn run_helper(&self, step: &StepSpec, feedback: &str, hint: &str) -> (HelperReport, f64) {
        self.calls.lock().unwrap().push("helper".into());
        self.helper_feedback.lock().unwrap().push(feedback.into());
        self.hints
            .lock()
            .unwrap()
            .push(format!("step{}:{hint}", step.id));
        let report = self
            .helper_reports
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| report(false));
        (report, 0.25)
    }
    async fn judge(
        &self,
        _s: &StepSpec,
        _r: &HelperReport,
        _a: u32,
        diff: &str,
    ) -> (Judgment, f64) {
        self.calls.lock().unwrap().push("judge".into());
        self.judge_diffs.lock().unwrap().push(diff.into());
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
    async fn revise(&self, _step: &StepSpec, history: &[AttemptRecord]) -> (Revision, f64) {
        self.calls.lock().unwrap().push("revise".into());
        self.revise_histories.lock().unwrap().push(history.to_vec());
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

/// Repository mock: scripted results for the loop's own check runs, a
/// fixed diff, and fixed file candidates.
struct MockWorkspace {
    checks: Mutex<VecDeque<LoopCheck>>,
    commands: Mutex<Vec<String>>,
    diff: String,
    candidates: Vec<Candidate>,
}

impl MockWorkspace {
    fn checks(checks: Vec<LoopCheck>) -> Self {
        Self {
            checks: Mutex::new(checks.into()),
            commands: Mutex::new(Vec::new()),
            diff: "=== changed src/lib.rs ===\n+fn added() {}\n".into(),
            candidates: Vec::new(),
        }
    }
    fn with_candidates(mut self, paths: &[&str]) -> Self {
        self.candidates = paths
            .iter()
            .map(|path| Candidate {
                path: (*path).into(),
                score: 1,
                ..Candidate::default()
            })
            .collect();
        self
    }
    fn commands(&self) -> Vec<String> {
        self.commands.lock().unwrap().clone()
    }
}

#[async_trait]
impl Workspace for MockWorkspace {
    async fn run_check(&self, command: &str) -> LoopCheck {
        self.commands.lock().unwrap().push(command.into());
        self.checks
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| LoopCheck::not_run("no scripted check"))
    }
    async fn snapshot(&self) -> Snapshot {
        Snapshot::default()
    }
    async fn diff_since(&self, _start: &Snapshot, _max_bytes: usize) -> String {
        self.diff.clone()
    }
    async fn candidates(&self, _text: &str, _limit: usize) -> Vec<Candidate> {
        self.candidates.clone()
    }
}

struct Run {
    summary: super::RunSummary,
    output: String,
    forks: Vec<Value>,
    picks: Vec<Value>,
}

async fn run_mode(
    claude: &MockClaude,
    jev: JevLayer,
    workspace: &MockWorkspace,
    mode: Mode,
) -> Run {
    let dir = tempfile::tempdir().unwrap();
    let fork_log = dir.path().join("logs").join("forks.jsonl");
    let options = RunOptions {
        task: "do the thing".into(),
        config: LoopConfig {
            fork_log: fork_log.clone(),
            ..LoopConfig::default()
        },
        mode,
    };
    let mut output = Vec::<u8>::new();
    let parts = Parts {
        claude,
        jev: &jev,
        workspace,
    };
    let summary = run(&options, parts, &mut output).await.unwrap();
    let records: Vec<Value> = std::fs::read_to_string(&fork_log)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let (picks, forks) = records
        .into_iter()
        .partition(|record| record["name"] == "file_pick");
    Run {
        summary,
        output: String::from_utf8(output).unwrap(),
        forks,
        picks,
    }
}

async fn run_loop(claude: &MockClaude, jev: JevLayer, workspace: &MockWorkspace) -> Run {
    let mode = if jev.enabled() {
        Mode::Jev
    } else {
        Mode::NoJev
    };
    run_mode(claude, jev, workspace, mode).await
}

fn jev_with(responses: Vec<Result<Value>>) -> JevLayer {
    JevLayer::new(Some(Box::new(MockJev::new(responses).0)), 0.80)
}

fn no_jev() -> JevLayer {
    JevLayer::new(None, 0.80)
}

// ------------------------------------------------- fix 1: the loop's check

#[tokio::test]
async fn the_loop_runs_the_helpers_check_and_its_result_wins() {
    // The helper claims a pass; the loop's own run fails. The claim loses:
    // a rule retries with the loop's output, and Jev is never asked.
    let claude = MockClaude::with(vec![step(1)], vec![report(true), report(true)]);
    let workspace = MockWorkspace::checks(vec![failed("FAILED test_x - assert 1 == 2"), passed()]);
    let run = run_loop(&claude, jev_with(vec![Ok(meets(0.95))]), &workspace).await;

    assert_eq!(workspace.commands(), ["cargo test", "cargo test"]);
    assert_eq!(run.forks[0]["decided_by"], "rule");
    assert_eq!(run.forks[0]["route"], "rule");
    assert_eq!(run.forks[0]["final_decision"], "retry");
    assert_eq!(run.forks[0]["check"], "failed");
    assert_eq!(run.forks[0]["helper_said_passed"], true);
    assert_eq!(run.forks[0]["loop_check"]["result"], "failed");
    assert_eq!(run.summary.stats.check_mismatches, 1);
    assert!(
        run.output
            .contains("the helper said its check passed, but the loop's run FAILED (exit 1)")
    );
    let feedback = claude.helper_feedback.lock().unwrap().clone();
    assert!(feedback[1].contains("The loop ran your check `cargo test` itself"));
    assert!(feedback[1].contains("assert 1 == 2"), "{}", feedback[1]);
    // The retry passed in the loop's run and Jev accepted it.
    assert_eq!(run.forks[1]["decided_by"], "jev");
    assert!(run.summary.finished);
}

#[tokio::test]
async fn a_claimed_pass_the_loop_could_not_run_is_never_accepted() {
    // Opus would say done, but nobody saw the check pass: a rule retries,
    // and the judge is never asked about it.
    let claude = MockClaude::with(vec![step(1)], vec![report(true), report(true)])
        .judgments(vec![Outcome::Done]);
    let workspace = MockWorkspace::checks(vec![LoopCheck::not_run("held by the gate"), passed()]);
    let run = run_loop(&claude, no_jev(), &workspace).await;

    assert_eq!(run.forks[0]["check"], "unverified");
    assert_eq!(run.forks[0]["decided_by"], "rule");
    assert_eq!(run.forks[0]["final_decision"], "retry");
    assert_eq!(run.summary.stats.checks_not_run, 1);
    let feedback = claude.helper_feedback.lock().unwrap().clone();
    assert!(
        feedback[1].contains("could not run your check"),
        "{}",
        feedback[1]
    );
    assert_eq!(claude.count("judge"), 1, "only the verified pass is judged");
    assert!(run.summary.finished);
}

#[tokio::test]
async fn a_claimed_failure_that_the_loop_sees_pass_is_reviewed() {
    let claude = MockClaude::with(vec![step(1)], vec![report(false)]);
    let workspace = MockWorkspace::checks(vec![passed()]);
    let run = run_loop(&claude, jev_with(vec![Ok(meets(0.9))]), &workspace).await;

    assert_eq!(run.forks[0]["check"], "passed");
    assert_eq!(run.forks[0]["decided_by"], "jev");
    assert_eq!(run.summary.stats.check_mismatches, 1);
    assert!(run.summary.finished);
}

// ------------------------------------------- fix 2: rules for obvious cases

#[tokio::test]
async fn a_new_error_retries_and_the_same_error_escalates_without_a_model() {
    let claude = MockClaude::with(vec![step(1)], vec![report(false); 3]);
    let workspace = MockWorkspace::checks(vec![
        failed("ImportError: no module named foo"),
        failed("test_a FAILED at line 12 in 0.31s"),
        failed("test_a FAILED at line 14 in 0.29s"),
    ]);
    let (mock, requests) = MockJev::new(vec![]);
    let jev = JevLayer::new(Some(Box::new(mock)), 0.8);
    let run = run_loop(&claude, jev, &workspace).await;

    let decisions: Vec<&str> = run
        .forks
        .iter()
        .map(|fork| fork["final_decision"].as_str().unwrap())
        .collect();
    assert_eq!(decisions, ["retry", "retry", "escalate"]);
    assert!(run.forks.iter().all(|fork| fork["decided_by"] == "rule"));
    assert!(
        run.forks[2]["reason"]
            .as_str()
            .unwrap()
            .contains("same error as the previous attempt"),
        "numbers are ignored: {}",
        run.forks[2]["reason"]
    );
    assert_eq!(claude.count("judge"), 0, "no model settles a failed check");
    assert_eq!(requests.lock().unwrap().len(), 0, "jev only reviews passes");
    assert_eq!(run.summary.stats.rule_forks, 3);
}

#[tokio::test]
async fn failed_checks_escalate_at_the_attempt_cap() {
    let claude = MockClaude::with(vec![step(1)], vec![report(false); 3]);
    let workspace = MockWorkspace::checks(vec![
        failed("error one"),
        failed("error two"),
        failed("error three"),
    ]);
    let run = run_loop(&claude, no_jev(), &workspace).await;

    assert_eq!(claude.count("helper"), 3);
    assert_eq!(run.forks[2]["final_decision"], "escalate");
    assert_eq!(run.forks[2]["reason"], "the check failed on all 3 attempts");
    // Escalation asks Opus for a revision; the mock stops the run.
    assert_eq!(claude.count("revise"), 1);
    assert!(!run.summary.finished);
    assert_eq!(run.summary.exit_code(), 2);
}

#[tokio::test]
async fn a_blocker_escalates_at_once() {
    let blocked = HelperReport {
        blocker: "the database schema is missing the column".into(),
        ..report(false)
    };
    let claude = MockClaude::with(vec![step(1)], vec![blocked]);
    let workspace = MockWorkspace::checks(vec![failed("error")]);
    let run = run_loop(&claude, no_jev(), &workspace).await;

    assert_eq!(run.forks[0]["final_decision"], "escalate");
    assert!(run.forks[0]["reason"].as_str().unwrap().contains("blocker"));
    assert_eq!(claude.count("helper"), 1);
    assert_eq!(claude.count("revise"), 1);
}

#[tokio::test]
async fn a_blocker_of_none_is_not_a_blocker() {
    for text in [
        "",
        "-",
        "none",
        "None.",
        "N/A",
        "no",
        "no blockers",
        "none - all good",
    ] {
        let report = HelperReport {
            blocker: text.into(),
            ..report(true)
        };
        assert!(!report.has_blocker(), "{text}");
    }
    for text in [
        "need a database URL",
        "No database is reachable from here",
        "None of the tests can run without a database",
    ] {
        let report = HelperReport {
            blocker: text.into(),
            ..report(false)
        };
        assert!(report.has_blocker(), "{text}");
    }
}

#[tokio::test]
async fn a_limit_stop_escalates_but_a_session_error_retries() {
    let limit = HelperReport::stopped(
        "",
        "Session ended with: tool-call limit reached",
        Stop::Limit,
    );
    let overloaded = HelperReport::stopped("", "Session ended with: Overloaded", Stop::Error);

    let claude = MockClaude::with(vec![step(1)], vec![limit]);
    let run = run_loop(&claude, no_jev(), &MockWorkspace::checks(vec![])).await;
    assert_eq!(run.forks[0]["final_decision"], "escalate");
    assert!(
        run.forks[0]["reason"]
            .as_str()
            .unwrap()
            .starts_with("a limit stopped")
    );
    assert!(run.forks[0].get("loop_check").is_none(), "nothing to check");

    let claude = MockClaude::with(vec![step(1)], vec![overloaded, report(true)])
        .judgments(vec![Outcome::Done]);
    let workspace = MockWorkspace::checks(vec![passed()]);
    let run = run_loop(&claude, no_jev(), &workspace).await;
    assert_eq!(run.forks[0]["final_decision"], "retry");
    assert_eq!(
        workspace.commands().len(),
        1,
        "a stopped attempt is not checked"
    );
    assert!(run.summary.finished, "the retry finished the step");
    let feedback = claude.helper_feedback.lock().unwrap().clone();
    assert!(feedback[1].contains("Overloaded"));
}

#[test]
fn same_error_ignores_numbers_but_not_messages() {
    use super::report::same_error;
    assert!(same_error(
        "FAILED test_a - assert 3 == 4 (0.12s) at 0x7f3a",
        "FAILED test_a - assert 5 == 6 (0.98s) at 0x7f9c"
    ));
    assert!(same_error("Error:   boom\n", "error: boom"));
    assert!(same_error(
        "/tmp/pytest-of-me/pytest-12/x: 3 failed in 1.20s",
        "/tmp/pytest-of-me/pytest-13/x: 3 failed in 0.98s"
    ));
    assert!(!same_error("FAILED test_a", "FAILED test_b"));
    assert!(!same_error("FAILED test_case_1", "FAILED test_case_2"));
    assert!(!same_error("ImportError: foo", "NameError: foo"));
}

// --------------------------------------- fix 3: jev reviews verified passes

#[tokio::test]
async fn jev_accepts_a_verified_pass_it_is_sure_meets_done_when() {
    let claude = MockClaude::with(vec![step(1)], vec![report(true)]);
    let (mock, requests) = MockJev::new(vec![Ok(meets(0.95))]);
    let jev = JevLayer::new(Some(Box::new(mock)), 0.8);
    let run = run_loop(&claude, jev, &MockWorkspace::checks(vec![passed()])).await;

    assert!(run.summary.finished);
    assert_eq!(claude.count("judge"), 0, "sharp forks never call Opus");
    assert_eq!(run.forks[0]["name"], "step_review");
    assert_eq!(run.forks[0]["route"], "sharp");
    assert_eq!(run.forks[0]["decided_by"], "jev");
    assert_eq!(run.forks[0]["answer"], "meets");
    assert_eq!(run.forks[0]["confidence"], 0.95);
    assert_eq!(run.summary.stats.jev_tokens, 500);

    // Jev saw the diff and the loop's own check output, not the claim.
    let requests = requests.lock().unwrap();
    let (state, questions) = &requests[0];
    assert_eq!(questions["meets"]["type"], "noul");
    assert!(
        state["changes"]
            .as_str()
            .unwrap()
            .contains("+fn added() {}")
    );
    assert_eq!(state["check_result"], "passed in 1.0s");
    assert_eq!(state["check_output_end"], "test result: ok");
    assert_eq!(state["done_when"], "check 1 passes");
    assert!(
        state.get("check_passed").is_none(),
        "the helper's claim is not shown"
    );
}

#[tokio::test]
async fn an_unsure_or_doubtful_jev_hands_the_pass_to_opus() {
    for (probability, answer, note) in [
        (0.6, "meets", "jev unsure"),
        (0.3, "falls_short", "jev unsure"),
        (0.05, "falls_short", "jev doubts the change meets done_when"),
    ] {
        let claude =
            MockClaude::with(vec![step(1)], vec![report(true)]).judgments(vec![Outcome::Done]);
        let workspace = MockWorkspace::checks(vec![passed()]);
        let run = run_loop(&claude, jev_with(vec![Ok(meets(probability))]), &workspace).await;

        assert_eq!(run.forks[0]["route"], "split", "{probability}");
        assert_eq!(run.forks[0]["answer"], answer, "{probability}");
        assert_eq!(run.forks[0]["note"], note, "{probability}");
        assert_eq!(run.forks[0]["decided_by"], "opus");
        assert_eq!(run.forks[0]["reason"], "opus says so");
        assert_eq!(claude.count("judge"), 1);
        let diffs = claude.judge_diffs.lock().unwrap().clone();
        assert!(
            diffs[0].contains("+fn added() {}"),
            "the judge sees the diff"
        );
        assert!(run.summary.finished);
    }
}

#[tokio::test]
async fn threshold_is_inclusive() {
    let claude = MockClaude::with(vec![step(1)], vec![report(true)]);
    let workspace = MockWorkspace::checks(vec![passed()]);
    let run = run_loop(&claude, jev_with(vec![Ok(meets(0.80))]), &workspace).await;
    assert_eq!(run.forks[0]["route"], "sharp");
    assert_eq!(claude.count("judge"), 0);
}

#[tokio::test]
async fn jev_errors_and_invalid_answers_fall_back_to_opus() {
    let bad = json!({"answers": {"meets": {"type": "noul", "noul": 1.7}}});
    let wrong_type = json!({"answers": {"meets": {"type": "choice", "choice": "done"}}});
    let claude = MockClaude::with(vec![step(1), step(2), step(3)], vec![report(true); 3])
        .judgments(vec![Outcome::Done; 3]);
    let workspace = MockWorkspace::checks(vec![passed(), passed(), passed()]);
    let jev = jev_with(vec![
        Err(anyhow::anyhow!("HTTP 503")),
        Ok(bad),
        Ok(wrong_type),
    ]);
    let run = run_loop(&claude, jev, &workspace).await;

    assert!(run.summary.finished);
    assert_eq!(claude.count("judge"), 3);
    for fork in &run.forks {
        assert_eq!(fork["route"], "split");
        assert!(fork["answer"].is_null());
        assert!(
            fork["note"].as_str().unwrap().starts_with("jev error:"),
            "{fork}"
        );
    }
}

#[tokio::test]
async fn a_review_retry_carries_the_reason_not_an_error() {
    let claude = MockClaude::with(vec![step(1)], vec![report(true), report(true)])
        .judgments(vec![Outcome::Retry, Outcome::Done]);
    let workspace = MockWorkspace::checks(vec![passed(), passed()]);
    let run = run_loop(&claude, no_jev(), &workspace).await;

    let feedback = claude.helper_feedback.lock().unwrap().clone();
    assert!(feedback[1].contains("it passed, but the review found the step is not done yet"));
    assert!(feedback[1].contains("opus says so"));
    assert!(run.summary.finished);
}

#[tokio::test]
async fn an_opus_retry_on_the_final_attempt_escalates() {
    let claude = MockClaude::with(vec![step(1)], vec![report(true); 3]).judgments(vec![
        Outcome::Retry,
        Outcome::Retry,
        Outcome::Retry,
    ]);
    let workspace = MockWorkspace::checks(vec![passed(), passed(), passed()]);
    let run = run_loop(&claude, no_jev(), &workspace).await;

    assert_eq!(run.forks[2]["final_decision"], "escalate");
    assert_eq!(run.forks[2]["decided_by"], "rule");
    assert_eq!(
        run.forks[2]["reason"],
        "hit 3 attempts (opus said retry: opus says so)"
    );
    assert_eq!(run.summary.stats.rule_overrides, 1);
    assert!(!run.summary.finished);
}

#[tokio::test]
async fn no_jev_sends_every_verified_pass_to_opus() {
    let claude = MockClaude::with(vec![step(1)], vec![report(true)]).judgments(vec![Outcome::Done]);
    let run = run_loop(&claude, no_jev(), &MockWorkspace::checks(vec![passed()])).await;

    assert!(run.summary.finished);
    assert_eq!(run.forks[0]["note"], "jev turned off (--no-jev)");
    assert_eq!(run.summary.stats.opus_fork_calls, 1);
    assert_eq!(run.summary.stats.jev_tokens, 0);
    assert!(run.picks.is_empty(), "no file picks without jev");
}

#[tokio::test]
async fn rules_only_accepts_a_verified_pass_without_any_review() {
    let claude = MockClaude::with(vec![step(1)], vec![report(true)]);
    let (mock, requests) = MockJev::new(vec![]);
    let jev = JevLayer::new(Some(Box::new(mock)), 0.8);
    let workspace = MockWorkspace::checks(vec![passed()]).with_candidates(&["src/lib.rs"]);
    let run = run_mode(&claude, jev, &workspace, Mode::RulesOnly).await;

    assert!(run.summary.finished);
    assert_eq!(run.forks[0]["decided_by"], "rule");
    assert_eq!(run.forks[0]["final_decision"], "done");
    assert_eq!(claude.count("judge"), 0);
    assert_eq!(
        requests.lock().unwrap().len(),
        0,
        "rules-only never calls jev"
    );
    assert!(run.picks.is_empty());
    assert!(run.output.contains("mode:                rules-only"));
}

// ---------------------------------------------------- fix 5: file picks

fn pick_answer(probabilities: &[f64]) -> Value {
    let mut answers = Map::new();
    for (index, p) in probabilities.iter().enumerate() {
        answers.insert(format!("c{index}"), json!({"type": "noul", "noul": p}));
    }
    json!({"answers": answers, "usage": {"input_tokens": 300, "output_tokens": 3}})
}

#[tokio::test]
async fn jev_file_picks_reach_the_planner_and_each_helper() {
    let claude = MockClaude::with(vec![step(1)], vec![report(true)]);
    let workspace = MockWorkspace::checks(vec![passed()]).with_candidates(&[
        "src/rules.rs",
        "docs/old.md",
        "tests/rules_test.rs",
    ]);
    let (mock, requests) = MockJev::new(vec![
        Ok(pick_answer(&[0.9, 0.1, 0.7])),
        Ok(pick_answer(&[0.2, 0.1, 0.95])),
        Ok(meets(0.9)),
    ]);
    let run = run_loop(
        &claude,
        JevLayer::new(Some(Box::new(mock)), 0.8),
        &workspace,
    )
    .await;

    let hints = claude.hints.lock().unwrap().clone();
    assert!(hints[0].starts_with("plan:Files that look relevant"));
    assert!(
        hints[0].contains("- src/rules.rs\n- tests/rules_test.rs"),
        "most likely first"
    );
    assert!(!hints[0].contains("docs/old.md"));
    assert_eq!(
        hints[1],
        "step1:Files that look relevant (from a quick relevance check; open them to confirm, and look further when needed):\n- tests/rules_test.rs"
    );
    assert_eq!(run.picks.len(), 2);
    assert_eq!(run.picks[0]["step"], 0);
    assert_eq!(
        run.picks[0]["files"],
        json!(["src/rules.rs", "tests/rules_test.rs"])
    );
    assert_eq!(run.summary.stats.files_picked, 3);
    assert_eq!(run.summary.stats.jev_tokens, 300 + 300 + 500);
    // Candidates go by position; paths are data, never question ids.
    let requests = requests.lock().unwrap();
    let (state, questions) = &requests[0];
    assert_eq!(questions.keys().collect::<Vec<_>>(), ["c0", "c1", "c2"]);
    assert_eq!(state["candidates"]["c1"]["path"], "docs/old.md");
    assert_eq!(questions["c0"]["type"], "noul");
}

#[tokio::test]
async fn a_failed_pick_leaves_the_prompts_unchanged() {
    let claude = MockClaude::with(vec![step(1)], vec![report(true)]);
    let workspace = MockWorkspace::checks(vec![passed()]).with_candidates(&["a.rs", "b.rs"]);
    let jev = jev_with(vec![
        Err(anyhow::anyhow!("HTTP 500")),
        Ok(json!({"answers": {"c0": {"type": "noul", "noul": 0.9}}})),
        Ok(meets(0.9)),
    ]);
    let run = run_loop(&claude, jev, &workspace).await;

    assert_eq!(*claude.hints.lock().unwrap(), ["plan:", "step1:"]);
    assert!(run.picks[0]["note"].as_str().unwrap().contains("HTTP 500"));
    assert!(
        run.picks[1]["note"]
            .as_str()
            .unwrap()
            .contains("no usable answer for b.rs")
    );
    assert!(run.summary.finished, "picks never block the run");
}

#[tokio::test]
async fn no_candidates_means_no_jev_call() {
    let claude = MockClaude::with(vec![step(1)], vec![report(true)]);
    let (mock, requests) = MockJev::new(vec![Ok(meets(0.9))]);
    let jev = JevLayer::new(Some(Box::new(mock)), 0.8);
    let run = run_loop(&claude, jev, &MockWorkspace::checks(vec![passed()])).await;

    assert_eq!(requests.lock().unwrap().len(), 1, "only the review");
    assert!(
        run.picks[0]["note"]
            .as_str()
            .unwrap()
            .contains("no candidate files")
    );
}

// ------------------------------------------------------- rest of the loop

#[tokio::test]
async fn a_stuck_step_is_revised_once_then_the_run_stops() {
    let revised = Revision {
        action: RevisionAction::Revise,
        task: "smaller task".into(),
        done_when: "unit test passes".into(),
        reason: "split it".into(),
    };
    let blocked = || HelperReport {
        blocker: "needs a decision".into(),
        ..report(false)
    };
    let claude = MockClaude::with(vec![step(1), step(2)], vec![blocked(), blocked()])
        .revisions(vec![revised]);
    let workspace = MockWorkspace::checks(vec![failed("x"), failed("y")]);
    let run = run_loop(&claude, no_jev(), &workspace).await;

    assert_eq!(claude.count("revise"), 1, "only one revision is allowed");
    assert_eq!(
        claude.count("helper"),
        2,
        "the revised step gets a fresh attempt"
    );
    // The reviser saw what the loop decided about the attempt, and why.
    let histories = claude.revise_histories.lock().unwrap().clone();
    let first = serde_json::to_value(&histories[0][0]).unwrap();
    assert_eq!(first["decision"], "escalate");
    assert!(
        first["reason"]
            .as_str()
            .unwrap()
            .contains("needs a decision")
    );
    assert_eq!(first["report"]["loop_check"]["result"], "failed");
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
    let blocked = HelperReport {
        blocker: "needs a product decision".into(),
        ..report(false)
    };
    let claude = MockClaude::with(vec![step(1)], vec![blocked]);
    let run = run_loop(&claude, no_jev(), &MockWorkspace::checks(vec![failed("x")])).await;
    assert!(!run.summary.finished);
    assert!(run.output.contains("Opus stopped the run: human needed"));
}

#[tokio::test]
async fn summary_shows_every_kind_of_fork_and_the_costs() {
    let claude = MockClaude::with(vec![step(1)], vec![report(false), report(true)]);
    let workspace = MockWorkspace::checks(vec![failed("boom"), passed()]);
    let run = run_loop(&claude, jev_with(vec![Ok(meets(0.95))]), &workspace).await;
    for label in [
        "--- run summary ---",
        "mode:                jev",
        "finished all steps:  true",
        "forks:               2  (rule 1, jev 1, opus 0)",
        "Opus fork calls:     0",
        "rule overrides:      0",
        "loop checks:         0 disagreed with the helper, 0 not run",
        "jev file picks:      2 (0 files suggested)",
        "Claude cost (est.):  $0.65",
        "Jev cost (est.):     $0.000021",
        "fork log:",
    ] {
        assert!(
            run.output.contains(label),
            "missing {label:?} in:\n{}",
            run.output
        );
    }
    assert_eq!(claude.calls(), ["plan", "helper", "helper", "review"]);
    assert!(
        run.output
            .contains("fork: rule -> retry (the check failed; retrying with the error)")
    );
    assert!(
        run.output
            .contains("fork: jev=meets @ 0.95 -> sharp -> done (by jev)")
    );
}

// --------------------------------------------------------------- reports

#[test]
fn the_helper_cannot_claim_a_loop_check() {
    let text = "{\"summary\": \"ok\", \"check_passed\": true, \
                \"loop_check\": {\"result\": \"passed\", \"seconds\": 1.0}}";
    let parsed = parse_helper_report(text).unwrap();
    assert!(parsed.loop_check.is_none());
    assert_eq!(parsed.stopped, None);
    assert_eq!(
        parsed.check_status(),
        super::report::CheckStatus::Unverified
    );
}

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
        Stop::Limit,
    );
    assert!(!report.check_passed);
    assert!(report.check_output_tail.ends_with("TAIL"));
    assert_eq!(report.summary, "Helper stopped before reporting.");
    assert_eq!(report.blocker, "Session ended with: cap");
    assert_eq!(report.error_text(), "Session ended with: cap");
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
    assert_eq!(defaults.plan_max_tool_calls, 40);
    assert_eq!(defaults.judge_max_tool_calls, 8);
    assert_eq!(defaults.revise_max_tool_calls, 15);
    assert_eq!(defaults.review_max_tool_calls, 25);
    assert_eq!(defaults.step_budget_usd, 2.0);
    assert_eq!(defaults.check_timeout_secs, 600);

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
            ("JCODE_JEV_LOOP_PLAN_MAX_TOOL_CALLS", "11"),
            ("JCODE_JEV_LOOP_JUDGE_MAX_TOOL_CALLS", "2"),
            ("JCODE_JEV_LOOP_REVISE_MAX_TOOL_CALLS", "3"),
            ("JCODE_JEV_LOOP_REVIEW_MAX_TOOL_CALLS", " 4 "),
            ("JCODE_JEV_LOOP_CHECK_TIMEOUT_SECS", "90"),
        ]),
        "/tmp/f.jsonl".into(),
    )
    .unwrap();
    assert_eq!(tuned.jev_confidence_threshold, 0.7);
    assert_eq!(tuned.max_attempts_per_step, 5);
    assert_eq!(
        (
            tuned.plan_max_tool_calls,
            tuned.judge_max_tool_calls,
            tuned.revise_max_tool_calls,
            tuned.review_max_tool_calls
        ),
        (11, 2, 3, 4)
    );
    assert_eq!(tuned.check_timeout_secs, 90);
    assert_eq!(tuned.fork_log, std::path::PathBuf::from("/tmp/f.jsonl"));

    for bad in [
        &[("JCODE_JEV_LOOP_THRESHOLD", "1.5")][..],
        &[("JCODE_JEV_LOOP_MAX_ATTEMPTS", "0")][..],
        &[("JCODE_JEV_LOOP_STEP_BUDGET_USD", "-1")][..],
        &[("JCODE_JEV_LOOP_JUDGE_MAX_TOOL_CALLS", "0")][..],
        &[("JCODE_JEV_LOOP_REVIEW_MAX_TOOL_CALLS", "many")][..],
        &[("JCODE_JEV_LOOP_PLAN_MAX_TOOL_CALLS", "-3")][..],
        &[("JCODE_JEV_LOOP_REVISE_MAX_TOOL_CALLS", "2.5")][..],
        &[("JCODE_JEV_LOOP_CHECK_TIMEOUT_SECS", "0")][..],
    ] {
        assert!(
            LoopConfig::from_lookup(env(bad), "f".into()).is_err(),
            "{bad:?}"
        );
    }
}

#[test]
fn decided_by_and_routes_serialize_for_the_fork_log() {
    assert_eq!(serde_json::to_value(DecidedBy::Jev).unwrap(), "jev");
    assert_eq!(serde_json::to_value(DecidedBy::Opus).unwrap(), "opus");
    assert_eq!(serde_json::to_value(DecidedBy::Rule).unwrap(), "rule");
    assert_eq!(serde_json::to_value(Route::Rule).unwrap(), "rule");
    assert_eq!(
        serde_json::to_value(Judgment {
            decision: Outcome::Retry,
            reason: String::new()
        })
        .unwrap()["decision"],
        "retry"
    );
}
