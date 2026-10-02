//! The loop itself: plan, then for each step run a helper, check its work in
//! code, settle the fork, and act.
//!
//! After each helper attempt the loop runs the helper's check command itself
//! (see `workspace`), and that result outranks the helper's claim. Then:
//!
//! - rules settle the obvious cases: a stopped attempt or a reported blocker
//!   escalates; a failed check retries with the error in hand, unless it is
//!   the same error as last time, which escalates; the attempt cap escalates;
//! - Jev reviews a passing step: does the change meet the step's
//!   `done_when`? A confident yes is accepted in code; anything else (and
//!   every pass with Jev off) goes to the Opus judge.
//! - `--rules-only` accepts a passing check with no review at all.

use anyhow::Result;
use std::io::Write;
use std::path::Path;

use super::claude::{AttemptRecord, ClaudeCalls, StepLogEntry};
use super::config::{self, LoopConfig};
use super::fork::{DecidedBy, Fork, ForkRecord, JEV_USD_PER_MILLION_INPUT_TOKENS, JevLayer, Pick};
use super::fork::{PickRecord, Route, append_record};
use super::report::{CheckResult, CheckStatus, HelperReport, RevisionAction, Stop, same_error};
use super::workspace::{Snapshot, Workspace};
use super::{Outcome, StepSpec};

/// Bytes of a step's diff a judge sees.
const DIFF_BYTES_FOR_JUDGE: usize = 48 * 1024;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stats {
    pub claude_cost: f64,
    pub jev_tokens: u64,
    /// Forks Jev settled (a confident "meets done_when").
    pub sharp: u32,
    /// Forks that went to the Opus judge.
    pub split: u32,
    /// Forks a rule settled without a model.
    pub rule_forks: u32,
    pub opus_fork_calls: u32,
    /// Times a rule overrode a model's decision.
    pub rule_overrides: u32,
    /// Attempts where the loop's own check disagreed with the helper's claim.
    pub check_mismatches: u32,
    /// Attempts whose check the loop could not run.
    pub checks_not_run: u32,
    pub file_picks: u32,
    pub files_picked: u32,
}

/// How forks are settled.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// Rules, then Jev reviews passing steps, then Opus when Jev is unsure.
    #[default]
    Jev,
    /// Rules, then Opus reviews every passing step (`--no-jev`).
    NoJev,
    /// Rules only: a passing check is accepted unreviewed (`--rules-only`).
    RulesOnly,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Jev => "jev",
            Self::NoJev => "no-jev",
            Self::RulesOnly => "rules-only",
        }
    }
}

pub struct RunOptions {
    pub task: String,
    pub config: LoopConfig,
    pub mode: Mode,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunSummary {
    pub finished: bool,
    pub steps_planned: usize,
    pub stats: Stats,
    pub review: String,
}

impl RunSummary {
    pub fn jev_cost_usd(&self) -> f64 {
        self.stats.jev_tokens as f64 * JEV_USD_PER_MILLION_INPUT_TOKENS / 1_000_000.0
    }

    /// 0 when every step finished, 2 when the run stopped for a human.
    pub fn exit_code(&self) -> i32 {
        if self.finished { 0 } else { 2 }
    }
}

/// Where the loop writes progress. Stdout in the CLI, a buffer in tests.
pub trait Progress: Send {
    fn line(&mut self, text: &str);
}

impl<W: Write + Send> Progress for W {
    fn line(&mut self, text: &str) {
        // Progress is best-effort; a closed pipe must not abort the loop.
        if writeln!(self, "{text}")
            .and_then(|()| self.flush())
            .is_err()
        {
            crate::logging::warn("jev-loop: could not write progress line");
        }
    }
}

/// What the loop talks to.
pub struct Parts<'a> {
    pub claude: &'a dyn ClaudeCalls,
    pub jev: &'a JevLayer,
    pub workspace: &'a dyn Workspace,
}

struct Loop<'a> {
    claude: &'a dyn ClaudeCalls,
    jev: &'a JevLayer,
    workspace: &'a dyn Workspace,
    config: &'a LoopConfig,
    mode: Mode,
    stats: Stats,
    step_log: Vec<StepLogEntry>,
    out: &'a mut dyn Progress,
}

pub async fn run(
    options: &RunOptions,
    parts: Parts<'_>,
    out: &mut dyn Progress,
) -> Result<RunSummary> {
    if let Some(parent) = options.config.fork_log.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let mut run = Loop {
        claude: parts.claude,
        jev: parts.jev,
        workspace: parts.workspace,
        config: &options.config,
        mode: options.mode,
        stats: Stats::default(),
        step_log: Vec::new(),
        out,
    };

    let hint = run.pick_files(0, &options.task).await;
    run.out.line("Opus is planning...");
    let (steps, cost) = run.claude.make_plan(&options.task, &hint).await?;
    run.stats.claude_cost += cost;
    for step in &steps {
        run.out.line(&format!(
            "  {}. {}  [done when: {}]",
            step.id, step.task, step.done_when
        ));
    }

    let mut finished = true;
    for step in &steps {
        if !run.run_step(step.clone()).await {
            finished = false;
            break;
        }
    }

    run.out.line("\nOpus is reviewing the whole change...");
    let (review, cost) = run.claude.review(&options.task, &run.step_log).await;
    run.stats.claude_cost += cost;
    run.out.line(&review);

    let summary = RunSummary {
        finished,
        steps_planned: steps.len(),
        stats: run.stats,
        review,
    };
    print_summary(run.out, &summary, options);
    Ok(summary)
}

impl Loop<'_> {
    /// Returns true if the step finished, false if the loop should stop.
    async fn run_step(&mut self, mut step: StepSpec) -> bool {
        let (mut attempt, mut revisions) = (0u32, 0u32);
        let (mut feedback, mut previous_error) = (String::new(), String::new());
        let mut history: Vec<AttemptRecord> = Vec::new();
        let mut hint = self
            .pick_files(step.id, &format!("{}\n{}", step.task, step.done_when))
            .await;
        let start = self.workspace.snapshot().await;
        loop {
            attempt += 1;
            self.out.line(&format!(
                "  step {} attempt {attempt}: {}",
                step.id, step.task
            ));
            let (mut report, cost) = self.claude.run_helper(&step, &feedback, &hint).await;
            self.stats.claude_cost += cost;
            self.verify(&mut report).await;
            let settled = self
                .decide(&step, &report, attempt, &previous_error, &start)
                .await;
            history.push(AttemptRecord {
                attempt,
                report: report.clone(),
                decision: settled.decision,
                reason: settled.reason.clone(),
            });
            match settled.decision {
                Outcome::Done => {
                    self.log_step(step, report, false, String::new());
                    return true;
                }
                // Only a review sends back a step whose check passed.
                Outcome::Retry if report.check_status() == CheckStatus::Passed => {
                    previous_error.clear();
                    feedback = format!(
                        "The loop ran your check `{}` and it passed, but the review found the \
                         step is not done yet:\n{}",
                        report.check_command, settled.reason
                    );
                    continue;
                }
                Outcome::Retry => {
                    previous_error = report.error_text();
                    feedback = retry_feedback(&report, &previous_error);
                    continue;
                }
                Outcome::Escalate => {}
            }

            if revisions >= self.config.max_revisions_per_step {
                let why = match revisions {
                    0 => "and no revisions are allowed".to_string(),
                    1 => "after a revision".to_string(),
                    n => format!("after {n} revisions"),
                };
                self.out.line(&format!(
                    "    stopping: step {} is still stuck {why}",
                    step.id
                ));
                self.log_step(step, report, true, String::new());
                return false;
            }
            let (revision, cost) = self.claude.revise(&step, &history).await;
            self.stats.claude_cost += cost;
            if revision.action == RevisionAction::Stop {
                self.out
                    .line(&format!("    Opus stopped the run: {}", revision.reason));
                self.log_step(step, report, true, revision.reason);
                return false;
            }
            revisions += 1;
            step = StepSpec {
                id: step.id,
                task: revision.task,
                done_when: revision.done_when,
            };
            self.out.line(&format!(
                "    Opus rewrote the step: {}",
                first_chars(&revision.reason, 120)
            ));
            hint = self
                .pick_files(step.id, &format!("{}\n{}", step.task, step.done_when))
                .await;
            attempt = 0;
            feedback.clear();
            previous_error.clear();
            history.clear();
        }
    }

    /// Run the helper's check in code and record what the loop saw. A
    /// stopped attempt has nothing to check.
    async fn verify(&mut self, report: &mut HelperReport) {
        if report.stopped.is_none() {
            let check = self.workspace.run_check(&report.check_command).await;
            report.loop_check = Some(check);
        }
        let shown = match &report.loop_check {
            Some(check) => format!("loop check {}", check.describe()),
            None => "helper stopped, nothing to check".into(),
        };
        let claimed = if report.check_passed {
            "passed"
        } else {
            "FAILED"
        };
        self.out.line(&format!(
            "    helper: says check {claimed}; {shown} - {}",
            first_chars(&report.summary, 120)
        ));
        let Some(check) = &report.loop_check else {
            return;
        };
        match check.result {
            CheckResult::NotRun => self.stats.checks_not_run += 1,
            result if (result == CheckResult::Passed) != report.check_passed => {
                self.stats.check_mismatches += 1;
                self.out.line(&format!(
                    "    note: the helper said its check {claimed}, but the loop's run {}",
                    check.describe()
                ));
            }
            _ => {}
        }
    }

    async fn decide(
        &mut self,
        step: &StepSpec,
        report: &HelperReport,
        attempt: u32,
        previous_error: &str,
        start: &Snapshot,
    ) -> Settled {
        let (fork, settled) = self
            .settle(step, report, attempt, previous_error, start)
            .await;
        self.stats.jev_tokens += fork.input_tokens;
        self.out.line(&fork_line(&fork, &settled));
        let record = ForkRecord {
            time: now(),
            step: step.id,
            attempt,
            fork: &fork,
            check: report.check_status(),
            helper_said_passed: report.check_passed,
            loop_check: report.loop_check.as_ref(),
            final_decision: settled.decision,
            decided_by: settled.decided_by,
            reason: &settled.reason,
        };
        if let Err(error) = append_record(&self.config.fork_log, &record) {
            self.out
                .line(&format!("    warning: fork not logged: {error:#}"));
        }
        settled
    }

    /// Rules first; a passing check is then accepted (`--rules-only`) or
    /// reviewed by Jev, with Opus deciding when Jev does not accept it. The
    /// judge is only ever asked about a check the loop saw pass.
    async fn settle(
        &mut self,
        step: &StepSpec,
        report: &HelperReport,
        attempt: u32,
        previous_error: &str,
        start: &Snapshot,
    ) -> (Fork, Settled) {
        let max_attempts = self.config.max_attempts_per_step;
        if let Some(settled) = rule_decision(report, attempt, max_attempts, previous_error) {
            self.stats.rule_forks += 1;
            return (Fork::rule(), settled);
        }
        if self.mode == Mode::RulesOnly {
            self.stats.rule_forks += 1;
            let why = "the loop's check passed (--rules-only: no review)";
            return (Fork::rule(), Settled::rule(Outcome::Done, why));
        }

        let diff = self.workspace.diff_since(start, DIFF_BYTES_FOR_JUDGE).await;
        let fork = self.jev.step_review(step, report, &diff).await;
        if fork.accepts() {
            self.stats.sharp += 1;
            return (
                fork,
                Settled::new(Outcome::Done, DecidedBy::Jev, String::new()),
            );
        }

        self.stats.split += 1;
        self.stats.opus_fork_calls += 1;
        let (judgment, cost) = self.claude.judge(step, report, attempt, &diff).await;
        self.stats.claude_cost += cost;
        let mut settled = Settled::new(judgment.decision, DecidedBy::Opus, judgment.reason);
        if settled.decision == Outcome::Retry && attempt >= max_attempts {
            let opus_said = std::mem::take(&mut settled.reason);
            settled = Settled::rule(
                Outcome::Escalate,
                format!("hit {max_attempts} attempts (opus said retry: {opus_said})"),
            );
            self.stats.rule_overrides += 1;
        }
        (fork, settled)
    }

    /// Ask Jev which files `text` needs and return the prompt hint. Step 0
    /// is the planner. Off in `--no-jev` and `--rules-only`.
    async fn pick_files(&mut self, step: u32, text: &str) -> String {
        if self.mode != Mode::Jev || !self.jev.enabled() {
            return String::new();
        }
        let candidates = self
            .workspace
            .candidates(text, config::MAX_PICK_CANDIDATES)
            .await;
        let pick = self.jev.pick_files(text, &candidates).await;
        self.stats.jev_tokens += pick.input_tokens;
        self.stats.file_picks += 1;
        self.stats.files_picked += pick.files.len() as u32;
        self.out.line(&pick_line(step, &pick));
        let record = PickRecord {
            time: now(),
            name: "file_pick",
            step,
            pick: &pick,
        };
        if let Err(error) = append_record(&self.config.fork_log, &record) {
            self.out
                .line(&format!("    warning: file pick not logged: {error:#}"));
        }
        super::pick::hint_block(&pick.files)
    }

    fn log_step(&mut self, step: StepSpec, report: HelperReport, stopped: bool, reason: String) {
        self.step_log.push(StepLogEntry {
            step,
            report,
            stopped,
            reason,
        });
    }
}

/// The rules that settle a fork without a model. None means the check
/// passed and a review decides (a blocker note on a passing check goes to
/// the review too).
///
/// - A session stopped by a code limit (tool-call or spending cap)
///   escalates: the step is too big as written.
/// - A reported blocker on an attempt that did not pass escalates.
/// - A failed check, a check the loop could not run, or a failed session (a
///   provider error, no valid report) is retried with the error in hand,
///   unless the error is the same as last time, which escalates.
/// - The attempt cap escalates whatever happened.
pub(crate) fn rule_decision(
    report: &HelperReport,
    attempt: u32,
    max_attempts: u32,
    previous_error: &str,
) -> Option<Settled> {
    let escalate = |why: String| Some(Settled::rule(Outcome::Escalate, why));
    let retryable = match (report.stopped, report.check_status()) {
        (Some(Stop::Limit), _) => {
            return escalate(format!("a limit stopped the helper: {}", report.blocker));
        }
        (Some(Stop::Error), _) => "the helper session failed",
        (None, CheckStatus::Passed) => return None,
        (None, _) if report.has_blocker() => {
            return escalate(format!("the helper reported a blocker: {}", report.blocker));
        }
        (None, CheckStatus::Unverified) => "the loop could not run the check",
        (None, CheckStatus::Failed) => "the check failed",
    };
    if !previous_error.is_empty() && same_error(previous_error, &report.error_text()) {
        return escalate(format!(
            "{retryable} with the same error as the previous attempt"
        ));
    }
    if attempt >= max_attempts {
        return escalate(format!("{retryable} on all {max_attempts} attempts"));
    }
    Some(Settled::rule(
        Outcome::Retry,
        format!("{retryable}; retrying with the error"),
    ))
}

fn retry_feedback(report: &HelperReport, error: &str) -> String {
    if report.stopped.is_some() {
        return format!("The session ended before you reported: {error}");
    }
    let command = &report.check_command;
    let ran = match &report.loop_check {
        Some(check) if check.result == CheckResult::NotRun => format!(
            "The loop could not run your check `{command}` itself ({}). Report a check_command \
             it can run from the repository root.",
            check.why_not_run
        ),
        Some(check) => format!(
            "The loop ran your check `{command}` itself: it {}.",
            check.describe()
        ),
        None => "Your check failed.".into(),
    };
    format!("{ran}\nOutput end:\n{error}")
}

pub(crate) struct Settled {
    pub decision: Outcome,
    pub decided_by: DecidedBy,
    pub reason: String,
}

impl Settled {
    fn new(decision: Outcome, decided_by: DecidedBy, reason: String) -> Self {
        Self {
            decision,
            decided_by,
            reason,
        }
    }

    fn rule(decision: Outcome, reason: impl Into<String>) -> Self {
        Self::new(decision, DecidedBy::Rule, reason.into())
    }
}

fn pick_line(step: u32, pick: &Pick) -> String {
    let who = if step == 0 {
        "the planner".to_string()
    } else {
        format!("step {step}")
    };
    if pick.files.is_empty() {
        let why = if pick.note.is_empty() {
            "none looked relevant"
        } else {
            &pick.note
        };
        return format!(
            "    jev file pick for {who}: no files ({} candidates; {})",
            pick.candidates,
            first_chars(why, 100)
        );
    }
    format!(
        "    jev file pick for {who}: {} of {} candidates: {}",
        pick.files.len(),
        pick.candidates,
        first_chars(&pick.files.join(", "), 300)
    )
}

/// One progress line per fork: `fork: rule -> retry (the check failed...)`
/// or `fork: jev=meets @ 0.93 -> sharp -> done (by jev)`.
fn fork_line(fork: &Fork, settled: &Settled) -> String {
    let decision = settled.decision.as_str();
    let reason = first_chars(&settled.reason, 100);
    if fork.route == Route::Rule {
        return format!("    fork: rule -> {decision} ({reason})");
    }
    let jev = match &fork.answer {
        Some(answer) => format!("jev={answer} @ {:.2}", fork.confidence),
        None => format!("jev: {}", first_chars(&fork.note, 80)),
    };
    let by = match settled.decided_by {
        DecidedBy::Rule => format!("by rule: {reason}"),
        other => format!("by {}", other.as_str()),
    };
    format!(
        "    fork: {jev} -> {} -> {decision} ({by})",
        fork.route.as_str()
    )
}

fn print_summary(out: &mut dyn Progress, summary: &RunSummary, options: &RunOptions) {
    let stats = &summary.stats;
    let total_forks = stats.sharp + stats.split + stats.rule_forks;
    out.line("\n--- run summary ---");
    out.line(&format!("mode:                {}", options.mode.as_str()));
    out.line(&format!("finished all steps:  {}", summary.finished));
    out.line(&format!(
        "forks:               {total_forks}  (rule {}, jev {}, opus {})",
        stats.rule_forks, stats.sharp, stats.split
    ));
    out.line(&format!("Opus fork calls:     {}", stats.opus_fork_calls));
    out.line(&format!("rule overrides:      {}", stats.rule_overrides));
    out.line(&format!(
        "loop checks:         {} disagreed with the helper, {} not run",
        stats.check_mismatches, stats.checks_not_run
    ));
    if options.mode == Mode::Jev {
        out.line(&format!(
            "jev file picks:      {} ({} files suggested)",
            stats.file_picks, stats.files_picked
        ));
    }
    out.line(&format!("Claude cost (est.):  ${:.2}", stats.claude_cost));
    out.line(&format!(
        "Jev cost (est.):     ${:.6}",
        summary.jev_cost_usd()
    ));
    out.line(&format!(
        "fork log:            {}",
        options.config.fork_log.display()
    ));
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

fn first_chars(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

/// True when `repo` is a git work tree with no uncommitted changes.
pub fn working_tree_is_clean(repo: &Path) -> Result<bool> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["status", "--porcelain"])
        .output()
        .map_err(|error| anyhow::anyhow!("Could not run git: {error}"))?;
    anyhow::ensure!(
        output.status.success(),
        "{} is not a git repository",
        repo.display()
    );
    Ok(output.stdout.iter().all(u8::is_ascii_whitespace))
}
