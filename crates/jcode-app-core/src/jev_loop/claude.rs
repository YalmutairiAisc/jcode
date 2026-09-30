//! Every Claude session the loop runs, on jcode's own agent runtime.
//!
//! Planner (Opus): `make_plan`, `judge` (split forks only), `revise`, `review`.
//! Helper (Sonnet): `run_helper` (one attempt at one step).
//!
//! Each call is a fresh, isolated jcode session with its own model, effort,
//! system prompt, tool allowlist, and a guarded tool registry that enforces
//! blocked commands, a tool-call cap, and (for helpers) a spending cap.

use anyhow::{Context, Result};
use async_trait::async_trait;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::config::{self, LoopConfig};
use super::guard::{GuardedTool, SessionGuard};
use super::report::{self, HelperReport, Judgment, Revision, RevisionAction};
use super::{Outcome, StepSpec};
use crate::agent::Agent;
use crate::provider::Provider;
use crate::tool::Registry;

/// Tools a read-only session may use. Edits and the shell are not offered.
const READ_ONLY_TOOLS: &[&str] = &["read", "agentgrep", "ls"];
/// Tools a helper may use: read, edit, and run commands in the repo.
const HELPER_TOOLS: &[&str] = &[
    "read",
    "agentgrep",
    "ls",
    "edit",
    "write",
    "apply_patch",
    "replace",
    "bash",
];
/// The reviewer reads and runs tests, but never edits.
const REVIEW_TOOLS: &[&str] = &["read", "agentgrep", "ls", "bash"];

/// A session's final text and what it cost.
#[derive(Clone, Debug, Default)]
pub struct SessionResult {
    pub text: String,
    pub cost_usd: f64,
    /// Set when a code-level rule (tool cap, budget) stopped the session.
    pub stopped: Option<String>,
    pub error: Option<String>,
}

/// The Claude side of the loop. Mocked in tests.
#[async_trait]
pub trait ClaudeCalls: Send + Sync {
    async fn make_plan(&self, task: &str) -> Result<(Vec<StepSpec>, f64)>;
    async fn run_helper(&self, step: &StepSpec, feedback: &str) -> (HelperReport, f64);
    async fn judge(&self, step: &StepSpec, report: &HelperReport, attempt: u32) -> (Judgment, f64);
    async fn revise(&self, step: &StepSpec, history: &[AttemptRecord]) -> (Revision, f64);
    async fn review(&self, task: &str, step_log: &[StepLogEntry]) -> (String, f64);
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct AttemptRecord {
    pub attempt: u32,
    pub report: HelperReport,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct StepLogEntry {
    pub step: StepSpec,
    pub report: HelperReport,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub stopped: bool,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub reason: String,
}

/// Runs the loop's sessions as isolated jcode agents.
pub struct JcodeClaude {
    provider: Arc<dyn Provider>,
    repo: PathBuf,
    config: LoopConfig,
}

struct SessionSpec<'a> {
    role: &'static str,
    model: &'a str,
    effort: &'a str,
    rules: String,
    tools: &'static [&'static str],
    max_tool_calls: u32,
    budget_usd: Option<f64>,
}

impl JcodeClaude {
    pub fn new(provider: Arc<dyn Provider>, repo: PathBuf, config: LoopConfig) -> Self {
        Self {
            provider,
            repo,
            config,
        }
    }

    fn planner<'a>(
        &'a self,
        role: &'static str,
        rules: String,
        tools: &'static [&'static str],
        max_tool_calls: u32,
    ) -> SessionSpec<'a> {
        SessionSpec {
            role,
            model: &self.config.planner_model,
            effort: &self.config.planner_effort,
            rules,
            tools,
            max_tool_calls,
            budget_usd: None,
        }
    }

    async fn run_session(&self, spec: SessionSpec<'_>, prompt: &str) -> SessionResult {
        match self.run_session_inner(&spec, prompt).await {
            Ok(result) => result,
            Err(error) => SessionResult {
                error: Some(format!("{error:#}")),
                ..SessionResult::default()
            },
        }
    }

    async fn run_session_inner(
        &self,
        spec: &SessionSpec<'_>,
        prompt: &str,
    ) -> Result<SessionResult> {
        let provider = self.provider.fork();
        let guard = SessionGuard::new(
            spec.max_tool_calls,
            spec.budget_usd,
            config::BLOCKED_COMMANDS,
        );
        let registry = guarded_registry(provider.clone(), spec.tools, &guard).await;
        let allowed: HashSet<String> = spec.tools.iter().map(|tool| tool.to_string()).collect();
        let mut session =
            crate::session::Session::create(None, Some(format!("jev-loop {}", spec.role)));
        session.working_dir = Some(self.repo.display().to_string());
        let mut agent = Agent::new_with_session(provider, registry, session, Some(allowed));
        agent
            .set_model(spec.model)
            .with_context(|| format!("Could not select model {}", spec.model))?;
        if let Err(error) = agent.set_reasoning_effort(spec.effort) {
            crate::logging::warn(&format!(
                "jev-loop: {} effort '{}' not applied: {error:#}",
                spec.role, spec.effort
            ));
        }
        agent.set_memory_enabled(false);
        agent.set_system_prompt(&system_prompt(&spec.rules, &self.repo));
        guard.attach_cancel(agent.graceful_shutdown_signal());

        let model = agent.provider_model();
        let price_key = price_source_key(self.provider.name());
        let (run, meter) = run_metered(&mut agent, prompt, &guard, &price_key, &model).await;
        let TurnMeter {
            usage,
            text,
            declined,
        } = meter;
        let cost_usd = session_cost(&price_key, &model, &usage);
        let stopped = guard.stop_reason();
        if let Some(reason) = &stopped {
            // A guard stop cancels through the agent's shutdown signal, which
            // the runtime records as a "server reload" interruption. Close the
            // transcript with the real reason, so it reads correctly and is
            // never mistaken for a reload-interrupted session to resume.
            agent.add_message(
                crate::message::Role::Assistant,
                vec![crate::message::ContentBlock::Text {
                    text: format!("[jev-loop stopped this session: {reason}]"),
                    cache_control: None,
                }],
            );
        }
        agent.mark_closed();
        let error = match run {
            Ok(()) => None,
            Err(error) if stopped.is_some() => Some(format!("{error:#}")),
            Err(error) => return Err(error),
        };
        // A model that declines the turn ends it without text and without an
        // error. Report why, instead of letting callers fail to parse an
        // empty reply and blame the format.
        let error = error.or_else(|| declined.filter(|_| text.trim().is_empty()));
        Ok(SessionResult {
            text,
            cost_usd,
            stopped,
            error,
        })
    }
}

#[async_trait]
impl ClaudeCalls for JcodeClaude {
    async fn make_plan(&self, task: &str) -> Result<(Vec<StepSpec>, f64)> {
        let spec = self.planner(
            "planner",
            format!("{PLAN_RULES}\n{PLAN_FORMAT}"),
            READ_ONLY_TOOLS,
            config::PLAN_MAX_TOOL_CALLS,
        );
        let result = self
            .run_session(spec, &format!("Plan this request:\n\n{task}"))
            .await;
        if let Some(error) = result.error.as_deref().filter(|_| result.text.is_empty()) {
            anyhow::bail!("Planning failed: {error}");
        }
        let steps = report::parse_plan(&result.text).context("Planning failed")?;
        Ok((steps, result.cost_usd))
    }

    async fn run_helper(&self, step: &StepSpec, feedback: &str) -> (HelperReport, f64) {
        let spec = SessionSpec {
            role: "helper",
            model: &self.config.helper_model,
            effort: &self.config.helper_effort,
            rules: format!("{HELPER_RULES}\n{REPORT_FORMAT}"),
            tools: HELPER_TOOLS,
            max_tool_calls: self.config.helper_max_tool_calls,
            budget_usd: Some(self.config.step_budget_usd),
        };
        let mut prompt = format!(
            "Step {}: {}\n\nDone when: {}",
            step.id, step.task, step.done_when
        );
        if !feedback.is_empty() {
            prompt.push_str(&format!(
                "\n\nYour previous attempt did not finish this step:\n{feedback}"
            ));
        }
        let result = self.run_session(spec, &prompt).await;
        let report = match report::parse_helper_report(&result.text) {
            Ok(report) if result.stopped.is_none() => report,
            parsed => {
                let why = result
                    .stopped
                    .clone()
                    .or_else(|| result.error.clone())
                    .or_else(|| parsed.err().map(|error| format!("{error:#}")))
                    .unwrap_or_else(|| "no report".into());
                HelperReport::stopped(&result.text, format!("Session ended with: {why}"))
            }
        };
        (report, result.cost_usd)
    }

    async fn judge(&self, step: &StepSpec, helper: &HelperReport, attempt: u32) -> (Judgment, f64) {
        let spec = self.planner(
            "judge",
            JUDGE_FORMAT.to_string(),
            READ_ONLY_TOOLS,
            config::JUDGE_MAX_TOOL_CALLS,
        );
        let prompt = format!(
            "A helper just attempted one step. Decide what happens next.\n\
             done = the step's goal is met and its check passed.\n\
             retry = a fresh attempt with the error in hand will likely fix it.\n\
             escalate = it needs a different approach or a decision.\n\n\
             Step: {}\nAttempt: {attempt}\nHelper report: {}",
            to_json(step),
            to_json(helper)
        );
        let result = self.run_session(spec, &prompt).await;
        let judgment = report::parse_judgment(&result.text).unwrap_or_else(|error| Judgment {
            decision: Outcome::Escalate,
            reason: format!("judge gave no usable decision: {error:#}"),
        });
        (judgment, result.cost_usd)
    }

    async fn revise(&self, step: &StepSpec, history: &[AttemptRecord]) -> (Revision, f64) {
        let spec = self.planner(
            "reviser",
            REVISE_FORMAT.to_string(),
            READ_ONLY_TOOLS,
            config::REVISE_MAX_TOOL_CALLS,
        );
        let prompt = format!(
            "This step is stuck. Look at what was tried, check the code if needed, \
             then either rewrite the step with a different approach (action=revise) \
             or stop and explain what a human needs to decide (action=stop).\n\n\
             Step: {}\nAttempts so far: {}",
            to_json(step),
            to_json(history)
        );
        let result = self.run_session(spec, &prompt).await;
        let revision = report::parse_revision(&result.text).unwrap_or_else(|error| Revision {
            action: RevisionAction::Stop,
            task: String::new(),
            done_when: String::new(),
            reason: format!("Revision failed: {error:#}"),
        });
        (revision, result.cost_usd)
    }

    async fn review(&self, task: &str, step_log: &[StepLogEntry]) -> (String, f64) {
        let spec = self.planner(
            "reviewer",
            REVIEW_RULES.to_string(),
            REVIEW_TOOLS,
            config::REVIEW_MAX_TOOL_CALLS,
        );
        let prompt = format!(
            "Original request:\n{task}\n\nWhat each step reported:\n{}",
            serde_json::to_string_pretty(step_log).unwrap_or_else(|_| "[]".into())
        );
        let result = self.run_session(spec, &prompt).await;
        let text = if result.text.trim().is_empty() {
            result
                .error
                .map(|error| format!("(no review text: {error})"))
                .unwrap_or_else(|| "(no review text)".into())
        } else {
            result.text
        };
        (text, result.cost_usd)
    }
}

/// A registry holding only `tools`, each wrapped by the session guard.
async fn guarded_registry(
    provider: Arc<dyn Provider>,
    tools: &[&str],
    guard: &Arc<SessionGuard>,
) -> Registry {
    let full = Registry::new(provider).await;
    let registry = Registry::empty();
    for name in tools {
        if let Some(tool) = full.unregister(name).await {
            registry
                .register(
                    (*name).to_string(),
                    Arc::new(GuardedTool::new(tool, guard.clone())),
                )
                .await;
        }
    }
    registry
}

/// Token usage summed over every model response in one session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct UsageTotals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_creation: u64,
}

impl UsageTotals {
    /// Add one response's usage. Returns false for non-usage events.
    pub(crate) fn add(&mut self, event: &crate::protocol::ServerEvent) -> bool {
        let crate::protocol::ServerEvent::TokenUsage {
            input,
            output,
            cache_read_input,
            cache_creation_input,
        } = event
        else {
            return false;
        };
        self.input += input;
        self.output += output;
        self.cache_read += cache_read_input.unwrap_or(0);
        self.cache_creation += cache_creation_input.unwrap_or(0);
        true
    }
}

/// Run one turn on the streaming path, which reports usage after every model
/// response. Each report re-prices the session, so an attempt that blows
/// through its budget is stopped mid-turn rather than after it finishes.
/// Returns the turn result and what the meter saw: usage, the final
/// response's text, and whether the model declined the turn.
async fn run_metered(
    agent: &mut Agent,
    prompt: &str,
    guard: &Arc<SessionGuard>,
    price_key: &str,
    model: &str,
) -> (Result<()>, TurnMeter) {
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut meter = TurnMeter::default();
    let run = {
        let mut turn =
            std::pin::pin!(agent.run_once_streaming_mpsc(prompt, Vec::new(), None, event_tx));
        loop {
            tokio::select! {
                result = &mut turn => break result,
                Some(event) = event_rx.recv() => {
                    if meter.observe(event) {
                        guard.record_spend(session_cost(price_key, model, &meter.usage));
                    }
                }
            }
        }
    };
    while let Ok(event) = event_rx.try_recv() {
        meter.observe(event);
    }
    (run, meter)
}

/// Tracks a streaming turn: summed usage, the text of the latest model
/// response (a tool call starts a new response, so its text is discarded),
/// and the provider's explanation when the model declined to answer.
#[derive(Default)]
pub(crate) struct TurnMeter {
    pub usage: UsageTotals,
    pub text: String,
    pub declined: Option<String>,
}

impl TurnMeter {
    /// Returns true when the event changed the usage totals.
    pub(crate) fn observe(&mut self, event: crate::protocol::ServerEvent) -> bool {
        use crate::protocol::ServerEvent;
        if self.usage.add(&event) {
            return true;
        }
        match event {
            ServerEvent::TextDelta { text } => self.text.push_str(&text),
            ServerEvent::TextReplace { text } => self.text = text,
            ServerEvent::ToolStart { .. } => self.text.clear(),
            ServerEvent::ProviderGuardrail { message, .. } => self.declined = Some(message),
            _ => {}
        }
        false
    }
}

/// Map a provider name to the pricing ledger's source key.
fn price_source_key(provider_name: &str) -> String {
    match provider_name.to_ascii_lowercase().as_str() {
        "claude" | "anthropic" => "claude:api-key".into(),
        "openai" => "openai:api-key".into(),
        other => other.into(),
    }
}

/// Estimated USD at public per-token rates. Subscription (OAuth) routes are
/// not billed per token; this is still the right number for comparing runs.
pub(crate) fn session_cost(price_key: &str, model: &str, usage: &UsageTotals) -> f64 {
    crate::provider::pricing::metered_usage_cost_usd(
        price_key,
        model,
        usage.input,
        usage.output,
        usage.cache_read,
        usage.cache_creation,
    )
    .unwrap_or(0.0)
}

fn to_json<T: serde::Serialize + ?Sized>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "{}".into())
}

pub(crate) fn system_prompt(rules: &str, repo: &Path) -> String {
    format!(
        "You are one part of an automated coding loop working in the git repository at {}.\n\
         Work only inside that repository. Tool calls that break the loop's rules are \
         refused in code; do not try to work around a refusal.\n\
         Never create commits, branches, tags, or stashes, and never change git config: \
         leave every change uncommitted in the working tree so a person can review it \
         with `git diff` and undo it with git.\n\n{}",
        repo.display(),
        rules.trim()
    )
}

const PLAN_RULES: &str = "\
You are the planner. Read the codebase as needed, but do not change anything.
Break the request into 2-8 small steps, in order. Each step must be something
one helper can finish in a single attempt, and must say exactly how to check
it is done (a test command, a build command, or a specific observable result).
Prefer steps that each touch one area of the code.";

const PLAN_FORMAT: &str = "\
When you are done, reply with only this JSON object and nothing after it:
{\"steps\": [{\"id\": 1, \"task\": \"...\", \"done_when\": \"...\"}]}";

const HELPER_RULES: &str = "\
You are a helper doing ONE step of a larger plan. Do only this step.
When you finish, run the step's check yourself and report honestly.
If the check fails, say so and include the end of the error output.";

const REPORT_FORMAT: &str = "\
End your reply with only this JSON object:
{\"summary\": \"...\", \"files_changed\": [\"...\"], \"check_command\": \"...\",
 \"check_passed\": true, \"check_output_tail\": \"...\", \"blocker\": \"\"}";

const JUDGE_FORMAT: &str = "\
You judge one helper attempt. Read code if needed, but change nothing.
Reply with only this JSON object:
{\"decision\": \"done\" | \"retry\" | \"escalate\", \"reason\": \"...\"}";

const REVISE_FORMAT: &str = "\
You fix a stuck plan step. Read code if needed, but change nothing.
Reply with only this JSON object:
{\"action\": \"revise\" | \"stop\", \"task\": \"...\", \"done_when\": \"...\", \"reason\": \"...\"}";

const REVIEW_RULES: &str = "\
You are reviewing finished work. Do not edit files. Look at the full change
(for example with `git diff`), run the project's tests, and report: what was
done, whether tests pass, and anything that looks wrong or unfinished.";
