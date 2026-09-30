//! Structured results from the loop's Claude sessions.
//!
//! The Python version used the Agent SDK's `output_format` (JSON schema).
//! Here each session ends by printing one JSON object; this module extracts
//! and validates it. A missing or malformed object never crashes the loop:
//! each caller has the same safe fallback the original used (a failed helper
//! report, an `escalate` judgment, a `stop` revision).

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Outcome, StepSpec};

/// What a helper reports after one attempt at one step.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HelperReport {
    pub summary: String,
    #[serde(default)]
    pub files_changed: Vec<String>,
    #[serde(default)]
    pub check_command: String,
    #[serde(default)]
    pub check_passed: bool,
    #[serde(default)]
    pub check_output_tail: String,
    #[serde(default)]
    pub blocker: String,
}

impl HelperReport {
    /// The original's fallback when a helper ends without a valid report.
    pub fn stopped(output_tail: &str, blocker: impl Into<String>) -> Self {
        Self {
            summary: "Helper stopped before reporting.".into(),
            files_changed: Vec::new(),
            check_command: String::new(),
            check_passed: false,
            check_output_tail: super::fork::tail(output_tail, 2000),
            blocker: blocker.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Judgment {
    pub decision: Outcome,
    #[serde(default)]
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RevisionAction {
    Revise,
    Stop,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Revision {
    pub action: RevisionAction,
    #[serde(default)]
    pub task: String,
    #[serde(default)]
    pub done_when: String,
    #[serde(default)]
    pub reason: String,
}

#[derive(Deserialize)]
struct PlanEnvelope {
    steps: Vec<StepSpec>,
}

pub fn parse_plan(text: &str) -> Result<Vec<StepSpec>> {
    let value = last_json_object(text).context("The planner returned no JSON plan")?;
    let plan: PlanEnvelope =
        serde_json::from_value(value).context("The planner's plan has the wrong shape")?;
    ensure!(!plan.steps.is_empty(), "The planner returned an empty plan");
    ensure!(
        plan.steps.len() <= 12,
        "The planner returned {} steps; at most 12 are allowed",
        plan.steps.len()
    );
    for step in &plan.steps {
        ensure!(
            !step.task.trim().is_empty() && !step.done_when.trim().is_empty(),
            "Every plan step needs a task and a done_when check"
        );
    }
    Ok(plan.steps)
}

pub fn parse_helper_report(text: &str) -> Result<HelperReport> {
    let value = last_json_object(text).context("The helper returned no JSON report")?;
    let report: HelperReport =
        serde_json::from_value(value).context("The helper's report has the wrong shape")?;
    ensure!(
        !report.summary.trim().is_empty(),
        "The helper's report has no summary"
    );
    Ok(report)
}

pub fn parse_judgment(text: &str) -> Result<Judgment> {
    let value = last_json_object(text).context("The judge returned no JSON decision")?;
    serde_json::from_value(value).context("The judge's decision has the wrong shape")
}

pub fn parse_revision(text: &str) -> Result<Revision> {
    let value = last_json_object(text).context("The reviser returned no JSON")?;
    let revision: Revision =
        serde_json::from_value(value).context("The revision has the wrong shape")?;
    if revision.action == RevisionAction::Revise
        && (revision.task.trim().is_empty() || revision.done_when.trim().is_empty())
    {
        bail!("A revise action needs a new task and done_when");
    }
    Ok(revision)
}

/// Return the last top-level JSON object in `text`. Fenced blocks and bare
/// objects both work; prose around them is ignored. The last object wins
/// because sessions often quote earlier JSON (plans, reports) while working.
pub fn last_json_object(text: &str) -> Option<Value> {
    let bytes = text.as_bytes();
    let mut found = None;
    let mut start = 0;
    while let Some(offset) = text[start..].find('{') {
        let open = start + offset;
        match matching_brace(bytes, open) {
            Some(close) => {
                if let Ok(value @ Value::Object(_)) =
                    serde_json::from_str::<Value>(&text[open..=close])
                {
                    found = Some(value);
                    start = close + 1;
                } else {
                    start = open + 1;
                }
            }
            None => start = open + 1,
        }
    }
    found
}

/// Index of the `}` that closes the `{` at `open`, honoring JSON strings.
fn matching_brace(bytes: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (index, &byte) in bytes.iter().enumerate().skip(open) {
        if in_string {
            match byte {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}
