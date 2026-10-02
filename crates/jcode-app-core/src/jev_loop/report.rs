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
    /// What the loop saw when it ran `check_command` itself. Never read from
    /// the helper's reply, so a helper cannot claim it.
    #[serde(skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub loop_check: Option<LoopCheck>,
    /// The session ended before the helper reported (a cap, the budget, or
    /// an error). Set by the loop only.
    #[serde(skip)]
    pub stopped: Option<Stop>,
}

/// Why a helper session ended without a usable report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// A code limit (the tool-call cap or the spending cap) stopped it.
    Limit,
    /// The session failed (for example the provider was overloaded) or
    /// ended without a valid report.
    Error,
}

impl HelperReport {
    /// The original's fallback when a helper ends without a valid report.
    pub fn stopped(output_tail: &str, blocker: impl Into<String>, why: Stop) -> Self {
        Self {
            summary: "Helper stopped before reporting.".into(),
            files_changed: Vec::new(),
            check_command: String::new(),
            check_passed: false,
            check_output_tail: super::fork::tail(output_tail, 2000),
            blocker: blocker.into(),
            loop_check: None,
            stopped: Some(why),
        }
    }

    /// The check result the loop acts on. The loop's own run of the check
    /// outranks the helper's claim. When the loop could not run it, a claimed
    /// failure is believed (it can only lead to another attempt), but a
    /// claimed pass is not: nobody saw it pass. Where the loop cannot run
    /// checks at all (Windows, for now), the claim stands, as in the
    /// prototype.
    pub fn check_status(&self) -> CheckStatus {
        match self.loop_check.as_ref().map(|check| check.result) {
            Some(CheckResult::Passed) => CheckStatus::Passed,
            Some(CheckResult::Failed) => CheckStatus::Failed,
            Some(CheckResult::Unsupported) if self.check_passed => CheckStatus::Passed,
            _ if !self.check_passed => CheckStatus::Failed,
            _ => CheckStatus::Unverified,
        }
    }

    /// Whether the helper reported a real blocker. Short notes such as
    /// "none", "N/A", or "none - all good" do not count; "None of the tests
    /// can run without a database" does.
    pub fn has_blocker(&self) -> bool {
        let text = self.blocker.trim().to_lowercase();
        let words: Vec<&str> = text
            .split(|c: char| !(c.is_alphanumeric() || c == '/'))
            .filter(|word| !word.is_empty())
            .collect();
        let empty = match words.first() {
            None => true,
            Some(first) => {
                (matches!(*first, "none" | "n/a" | "na" | "null" | "nothing") && words.len() <= 3)
                    || words == ["no"]
                    || text.starts_with("no blocker")
            }
        };
        !empty
    }

    /// The failure a retry carries and the same-error rule compares: why the
    /// session stopped, else the loop's own check output, else what the
    /// helper said.
    pub fn error_text(&self) -> String {
        if self.stopped.is_some() {
            return self.blocker.clone();
        }
        match &self.loop_check {
            Some(check) if check.result == CheckResult::NotRun => {
                return format!("the loop could not run the check: {}", check.why_not_run);
            }
            Some(check) if check.result == CheckResult::Failed => {
                return if check.output_tail.trim().is_empty() {
                    format!("(no output; the check {})", check.describe())
                } else {
                    check.output_tail.clone()
                };
            }
            _ => {}
        }
        [&self.check_output_tail, &self.blocker]
            .into_iter()
            .find(|text| !text.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| "(no error output was reported)".into())
    }
}

/// What happened when the loop ran a helper's check command itself.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LoopCheck {
    pub result: CheckResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub timed_out: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub why_not_run: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub output_tail: String,
    pub seconds: f64,
}

impl LoopCheck {
    pub fn not_run(why: impl Into<String>) -> Self {
        Self {
            result: CheckResult::NotRun,
            exit_code: None,
            timed_out: false,
            why_not_run: why.into(),
            output_tail: String::new(),
            seconds: 0.0,
        }
    }

    pub fn finished(
        passed: bool,
        exit_code: Option<i32>,
        output_tail: String,
        seconds: f64,
    ) -> Self {
        Self {
            result: if passed {
                CheckResult::Passed
            } else {
                CheckResult::Failed
            },
            exit_code,
            timed_out: false,
            why_not_run: String::new(),
            output_tail,
            seconds,
        }
    }

    pub fn timed_out(output_tail: String, seconds: f64) -> Self {
        Self {
            timed_out: true,
            ..Self::finished(false, None, output_tail, seconds)
        }
    }

    /// The loop cannot run checks on this platform; the helper's claim is
    /// used instead.
    pub fn unsupported() -> Self {
        Self {
            result: CheckResult::Unsupported,
            why_not_run: "the loop runs checks itself only on Unix".into(),
            ..Self::not_run("")
        }
    }

    /// One line for progress output and the fork log.
    pub fn describe(&self) -> String {
        match self.result {
            CheckResult::Passed => format!("passed in {:.1}s", self.seconds),
            CheckResult::Failed if self.timed_out => {
                format!("FAILED (timed out after {:.0}s)", self.seconds)
            }
            CheckResult::Failed => match self.exit_code {
                Some(code) => format!("FAILED (exit {code})"),
                None => "FAILED (killed by a signal)".into(),
            },
            CheckResult::NotRun => format!("not run: {}", self.why_not_run),
            CheckResult::Unsupported => {
                format!("not run ({}; the helper's claim is used)", self.why_not_run)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckResult {
    Passed,
    Failed,
    NotRun,
    /// The platform cannot run checks (Windows): the claim is used.
    Unsupported,
}

/// The check result a decision is based on (see [`HelperReport::check_status`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Passed,
    Failed,
    Unverified,
}

impl CheckStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Unverified => "unverified",
        }
    }
}

/// Whether two failures are the same error. Standalone numbers (timings,
/// counts, line numbers, temp directory suffixes, addresses) are ignored,
/// so a test that fails the same way twice matches. Digits inside names
/// (`test_case_1`) still count, so a different failing test or a different
/// message does not match.
pub fn same_error(previous: &str, current: &str) -> bool {
    let (previous, current) = (normalize_error(previous), normalize_error(current));
    let tail = |text: &str| -> String {
        let count = text.chars().count();
        text.chars().skip(count.saturating_sub(1500)).collect()
    };
    tail(&previous) == tail(&current)
}

fn normalize_error(text: &str) -> String {
    let mut normalized = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut previous: Option<char> = None;
    while let Some(c) = chars.next() {
        let in_name = previous.is_some_and(|p| p.is_alphanumeric() || p == '_');
        if c.is_ascii_digit() && !in_name {
            let mut last = c;
            let hex = c == '0' && chars.peek().is_some_and(|next| matches!(next, 'x' | 'X'));
            if hex {
                chars.next();
            }
            while let Some(&next) = chars
                .peek()
                .filter(|next| next.is_ascii_digit() || (hex && next.is_ascii_hexdigit()))
            {
                chars.next();
                last = next;
            }
            normalized.push('#');
            previous = Some(last);
            continue;
        }
        if c.is_whitespace() {
            if !normalized.ends_with(' ') {
                normalized.push(' ');
            }
        } else {
            normalized.extend(c.to_lowercase());
        }
        previous = Some(c);
    }
    normalized.trim().to_string()
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
