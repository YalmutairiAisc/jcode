//! The loop itself: plan, then for each step run a helper, fork, and act.

use anyhow::Result;
use std::io::Write;
use std::path::Path;

use super::claude::{AttemptRecord, ClaudeCalls, StepLogEntry};
use super::config::LoopConfig;
use super::fork::{
    DecidedBy, Fork, ForkRecord, JEV_USD_PER_MILLION_INPUT_TOKENS, JevLayer, Route,
    append_fork_record,
};
use super::report::{HelperReport, RevisionAction};
use super::{Outcome, StepSpec};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Stats {
    pub claude_cost: f64,
    pub jev_tokens: u64,
    pub sharp: u32,
    pub split: u32,
    pub opus_fork_calls: u32,
    pub rule_overrides: u32,
}

pub struct RunOptions {
    pub task: String,
    pub config: LoopConfig,
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

struct Loop<'a> {
    claude: &'a dyn ClaudeCalls,
    jev: &'a JevLayer,
    config: &'a LoopConfig,
    stats: Stats,
    step_log: Vec<StepLogEntry>,
    out: &'a mut dyn Progress,
}

pub async fn run(
    options: &RunOptions,
    claude: &dyn ClaudeCalls,
    jev: &JevLayer,
    out: &mut dyn Progress,
) -> Result<RunSummary> {
    if let Some(parent) = options.config.fork_log.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let mut run = Loop {
        claude,
        jev,
        config: &options.config,
        stats: Stats::default(),
        step_log: Vec::new(),
        out,
    };

    run.out.line("Opus is planning...");
    let (steps, cost) = claude.make_plan(&options.task).await?;
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
    let (review, cost) = claude.review(&options.task, &run.step_log).await;
    run.stats.claude_cost += cost;
    run.out.line(&review);

    let summary = RunSummary {
        finished,
        steps_planned: steps.len(),
        stats: run.stats,
        review,
    };
    print_summary(run.out, &summary, &options.config.fork_log);
    Ok(summary)
}

impl Loop<'_> {
    /// Returns true if the step finished, false if the loop should stop.
    async fn run_step(&mut self, mut step: StepSpec) -> bool {
        let (mut attempt, mut revisions) = (0u32, 0u32);
        let (mut feedback, mut previous_error) = (String::new(), String::new());
        let mut history: Vec<AttemptRecord> = Vec::new();
        loop {
            attempt += 1;
            self.out.line(&format!(
                "  step {} attempt {attempt}: {}",
                step.id, step.task
            ));
            let (report, cost) = self.claude.run_helper(&step, &feedback).await;
            self.stats.claude_cost += cost;
            history.push(AttemptRecord {
                attempt,
                report: report.clone(),
            });
            self.out.line(&format!(
                "    helper: check {} - {}",
                if report.check_passed {
                    "passed"
                } else {
                    "FAILED"
                },
                first_chars(&report.summary, 120)
            ));

            let decision = self.decide(&step, &report, attempt, &previous_error).await;
            match decision {
                Outcome::Done => {
                    self.log_step(step, report, false, String::new());
                    return true;
                }
                Outcome::Retry => {
                    previous_error = if report.check_output_tail.is_empty() {
                        report.blocker.clone()
                    } else {
                        report.check_output_tail.clone()
                    };
                    feedback = format!(
                        "Check `{}` failed.\nBlocker: {}\nOutput end:\n{previous_error}",
                        report.check_command, report.blocker
                    );
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
            attempt = 0;
            feedback.clear();
            previous_error.clear();
            history.clear();
        }
    }

    /// The fork: Jev answers; code acts if sharp, the planner decides if split.
    async fn decide(
        &mut self,
        step: &StepSpec,
        report: &HelperReport,
        attempt: u32,
        previous_error: &str,
    ) -> Outcome {
        let mut fork = self
            .jev
            .step_outcome(step, report, attempt, previous_error)
            .await;
        self.stats.jev_tokens += fork.input_tokens;
        let outcome = self.settle(&mut fork, step, report, attempt).await;
        let shown = match fork.answer {
            Some(answer) => format!("{} @ {:.2}", answer.as_str(), fork.confidence),
            None => fork.note.clone(),
        };
        self.out.line(&format!(
            "    fork: jev={shown} -> {} -> {} (by {})",
            route_name(fork.route),
            outcome.decision.as_str(),
            outcome.decided_by.as_str()
        ));
        let record = ForkRecord {
            time: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
            step: step.id,
            attempt,
            fork: &fork,
            final_decision: outcome.decision,
            decided_by: outcome.decided_by,
            opus_reason: &outcome.reason,
        };
        if let Err(error) = append_fork_record(&self.config.fork_log, &record) {
            self.out
                .line(&format!("    warning: fork not logged: {error:#}"));
        }
        outcome.decision
    }

    async fn settle(
        &mut self,
        fork: &mut Fork,
        step: &StepSpec,
        report: &HelperReport,
        attempt: u32,
    ) -> Settled {
        // Policy stays in code: never accept "done" when the check did not pass.
        if fork.route == Route::Sharp && fork.answer == Some(Outcome::Done) && !report.check_passed
        {
            fork.route = Route::Split;
            fork.note = "overruled: jev said done but the check failed".into();
            self.stats.rule_overrides += 1;
        }

        // Split forks (Jev unsure, off, or unreachable) go to the planner.
        let mut settled = match (fork.route, fork.answer) {
            (Route::Sharp, Some(answer)) => {
                self.stats.sharp += 1;
                Settled::new(answer, DecidedBy::Jev, String::new())
            }
            _ => {
                self.stats.split += 1;
                self.stats.opus_fork_calls += 1;
                let (judgment, cost) = self.claude.judge(step, report, attempt).await;
                self.stats.claude_cost += cost;
                Settled::new(judgment.decision, DecidedBy::Opus, judgment.reason)
            }
        };

        // The same rule binds the planner: "a step is never accepted as done
        // if its check failed". The prototype only enforced it against Jev,
        // so an Opus "done" could slip through; a failed check means retry.
        if settled.decision == Outcome::Done && !report.check_passed {
            let opus_said = std::mem::take(&mut settled.reason);
            settled = Settled::new(
                Outcome::Retry,
                DecidedBy::Rule,
                format!("overruled: opus said done but the check failed ({opus_said})"),
            );
            self.stats.rule_overrides += 1;
        }

        // Hard limit in code: too many attempts means escalate, whatever anyone said.
        if settled.decision == Outcome::Retry && attempt >= self.config.max_attempts_per_step {
            settled = Settled::new(
                Outcome::Escalate,
                DecidedBy::Rule,
                format!("hit {} attempts", self.config.max_attempts_per_step),
            );
            self.stats.rule_overrides += 1;
        }
        settled
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

struct Settled {
    decision: Outcome,
    decided_by: DecidedBy,
    reason: String,
}

impl Settled {
    fn new(decision: Outcome, decided_by: DecidedBy, reason: String) -> Self {
        Self {
            decision,
            decided_by,
            reason,
        }
    }
}

fn route_name(route: Route) -> &'static str {
    match route {
        Route::Sharp => "sharp",
        Route::Split => "split",
    }
}

fn print_summary(out: &mut dyn Progress, summary: &RunSummary, fork_log: &Path) {
    let stats = &summary.stats;
    let total_forks = stats.sharp + stats.split;
    out.line("\n--- run summary ---");
    out.line(&format!("finished all steps:  {}", summary.finished));
    out.line(&format!(
        "forks:               {total_forks}  (sharp {}, split {})",
        stats.sharp, stats.split
    ));
    out.line(&format!("Opus fork calls:     {}", stats.opus_fork_calls));
    out.line(&format!("rule overrides:      {}", stats.rule_overrides));
    out.line(&format!("Claude cost (est.):  ${:.2}", stats.claude_cost));
    out.line(&format!(
        "Jev cost (est.):     ${:.6}",
        summary.jev_cost_usd()
    ));
    out.line(&format!("fork log:            {}", fork_log.display()));
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
