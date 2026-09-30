//! The Jev fork layer.
//!
//! Each fork is one multiple-choice question to Jev. If Jev is confident
//! enough ("sharp"), the loop acts on its answer in code. If not ("split"),
//! the planner decides. Every fork is appended to the fork log so the
//! threshold can be tuned later.
//!
//! One fork exists today: `step_outcome` (done / retry / escalate).

use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use serde::Serialize;
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use super::{Outcome, StepSpec, report::HelperReport};

/// Published Jev input price. Output tokens are free.
pub const JEV_USD_PER_MILLION_INPUT_TOKENS: f64 =
    crate::voice_intent::JEV_USD_PER_MILLION_INPUT_TOKENS;

const QUESTION_ID: &str = "q";
const STEP_OUTCOME_INSTRUCTIONS: &str =
    "A helper just attempted one step of a coding task and reported back. What should happen next?";

fn step_outcome_criteria() -> Map<String, Value> {
    let mut criteria = Map::new();
    criteria.insert(
        "done".into(),
        json!("The step's goal is met and its check passed."),
    );
    criteria.insert(
        "retry".into(),
        json!(
            "The attempt failed for a small, fixable reason (a typo, a missing import, a wrong path, a flaky test) that another attempt with the error in hand is likely to fix."
        ),
    );
    criteria.insert(
        "escalate".into(),
        json!(
            "The attempt failed in a way that needs a different approach or a decision: the same error as the previous attempt, the step seems wrong or impossible as written, or the helper reported a blocker."
        ),
    );
    criteria
}

pub(crate) fn step_outcome_questions() -> Map<String, Value> {
    let mut questions = Map::new();
    questions.insert(
        QUESTION_ID.into(),
        json!({
            "type": "choice",
            "instructions": STEP_OUTCOME_INSTRUCTIONS,
            "criteria": step_outcome_criteria(),
        }),
    );
    questions
}

/// Where a fork was settled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Route {
    /// Jev was confident: code acts on its answer.
    Sharp,
    /// Jev was unsure, off, or unreachable: the planner decides.
    Split,
}

/// One fork's Jev result, before policy and the planner weigh in.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Fork {
    pub name: String,
    pub answer: Option<Outcome>,
    pub confidence: f64,
    pub probabilities: BTreeMap<String, f64>,
    pub route: Route,
    pub latency_ms: u64,
    pub input_tokens: u64,
    pub note: String,
}

impl Fork {
    fn split(name: &str, note: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            answer: None,
            confidence: 0.0,
            probabilities: BTreeMap::new(),
            route: Route::Split,
            latency_ms: 0,
            input_tokens: 0,
            note: note.into(),
        }
    }
}

/// Sends one typed Decisions request. Mocked in tests.
#[async_trait]
pub trait DecisionTransport: Send + Sync {
    async fn evaluate(&self, state: Value, questions: Map<String, Value>) -> Result<Value>;
}

#[async_trait]
impl DecisionTransport for crate::jev::JevClient {
    async fn evaluate(&self, state: Value, questions: Map<String, Value>) -> Result<Value> {
        crate::jev::JevClient::evaluate(self, state, questions).await
    }
}

pub struct JevLayer {
    transport: Option<Box<dyn DecisionTransport>>,
    threshold: f64,
}

impl JevLayer {
    /// `transport: None` is the `--no-jev` baseline: every fork is split.
    pub fn new(transport: Option<Box<dyn DecisionTransport>>, threshold: f64) -> Self {
        Self {
            transport,
            threshold,
        }
    }

    pub fn enabled(&self) -> bool {
        self.transport.is_some()
    }

    pub async fn step_outcome(
        &self,
        step: &StepSpec,
        report: &HelperReport,
        attempt: u32,
        previous_error: &str,
    ) -> Fork {
        let state = json!({
            "step": step.task,
            "done_when": step.done_when,
            "attempt_number": attempt,
            "helper_summary": report.summary,
            "check_command": report.check_command,
            "check_passed": report.check_passed,
            "check_output_end": tail(&report.check_output_tail, 3000),
            "blocker": report.blocker,
            "previous_attempt_error_end": tail(previous_error, 1500),
        });
        self.ask("step_outcome", state).await
    }

    async fn ask(&self, name: &str, state: Value) -> Fork {
        let Some(transport) = self.transport.as_deref() else {
            return Fork::split(name, "jev turned off (--no-jev)");
        };
        let started = std::time::Instant::now();
        // Any Jev problem is treated as "not sure": the planner decides.
        let value = match transport.evaluate(state, step_outcome_questions()).await {
            Ok(value) => value,
            Err(error) => return Fork::split(name, format!("jev error: {error:#}")),
        };
        let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let input_tokens = value
            .pointer("/usage/input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        match parse_choice(&value) {
            Ok(answer) => Fork {
                name: name.into(),
                answer: Some(answer.choice),
                confidence: round4(answer.confidence),
                probabilities: answer
                    .probabilities
                    .into_iter()
                    .map(|(key, value)| (key, round4(value)))
                    .collect(),
                route: if answer.confidence >= self.threshold {
                    Route::Sharp
                } else {
                    Route::Split
                },
                latency_ms,
                input_tokens,
                note: String::new(),
            },
            Err(error) => Fork {
                latency_ms,
                input_tokens,
                ..Fork::split(name, format!("jev error: {error:#}"))
            },
        }
    }
}

#[derive(Debug, PartialEq)]
pub(crate) struct ChoiceAnswer {
    pub choice: Outcome,
    pub confidence: f64,
    pub probabilities: BTreeMap<String, f64>,
}

/// Validate Jev's typed choice before any code acts on it. The chosen option
/// must be offered, the distribution complete and normalized, and the choice
/// must agree with it. Any violation is treated as a Jev error (split), never
/// as an answer. Routing uses Jev's stated confidence, as the prototype did,
/// so thresholds tuned against its fork logs keep their meaning.
pub(crate) fn parse_choice(value: &Value) -> Result<ChoiceAnswer> {
    let answer = value
        .pointer("/answers/q")
        .context("Jev returned no step_outcome answer")?;
    ensure!(
        answer["type"] == "choice",
        "Jev did not return a typed choice"
    );
    let choice_text = answer["choice"]
        .as_str()
        .context("Jev returned no choice")?;
    let choice = Outcome::parse(choice_text).context("Jev returned an unknown choice")?;
    let confidence = answer["confidence"]
        .as_f64()
        .context("Jev omitted its confidence")?;
    ensure!(
        confidence.is_finite() && (0.0..=1.0).contains(&confidence),
        "Jev returned an invalid confidence"
    );
    let raw = answer["probabilities"]
        .as_object()
        .context("Jev omitted choice probabilities")?;
    ensure!(
        raw.len() == Outcome::ALL.len(),
        "Jev returned an incomplete probability set"
    );
    let mut probabilities = BTreeMap::new();
    for (key, probability) in raw {
        ensure!(
            Outcome::parse(key).is_some(),
            "Jev returned a probability for an unknown choice"
        );
        let probability = probability
            .as_f64()
            .filter(|p| p.is_finite() && (0.0..=1.0).contains(p))
            .context("Jev returned an invalid probability")?;
        probabilities.insert(key.clone(), probability);
    }
    let sum: f64 = probabilities.values().sum();
    ensure!(
        (sum - 1.0).abs() <= 0.02,
        "Jev probabilities do not sum to one"
    );
    let selected = probabilities
        .get(choice.as_str())
        .copied()
        .context("Jev omitted the probability of its own choice")?;
    let highest = probabilities.values().copied().fold(0.0, f64::max);
    ensure!(
        selected + 1e-6 >= highest,
        "Jev's choice disagrees with its own probabilities"
    );
    Ok(ChoiceAnswer {
        choice,
        confidence,
        probabilities,
    })
}

/// One JSON line per fork: what Jev said, who decided, and what happened.
#[derive(Debug, Serialize)]
pub struct ForkRecord<'a> {
    pub time: String,
    pub step: u32,
    pub attempt: u32,
    #[serde(flatten)]
    pub fork: &'a Fork,
    pub final_decision: Outcome,
    pub decided_by: DecidedBy,
    pub opus_reason: &'a str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DecidedBy {
    Jev,
    Opus,
    Rule,
}

impl DecidedBy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Jev => "jev",
            Self::Opus => "opus",
            Self::Rule => "rule",
        }
    }
}

pub fn append_fork_record(path: &Path, record: &ForkRecord<'_>) -> Result<()> {
    let line = serde_json::to_string(record).context("Could not encode fork record")?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("Could not open fork log {}", path.display()))?;
    writeln!(file, "{line}").with_context(|| format!("Could not write {}", path.display()))
}

/// Keep the end of long outputs (errors live at the end) under Jev's limit.
pub(crate) fn tail(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let kept: String = text.chars().skip(count - max_chars).collect();
    format!("...{kept}")
}

fn round4(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}
