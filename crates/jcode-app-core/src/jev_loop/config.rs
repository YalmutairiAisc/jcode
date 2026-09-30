//! Every knob for the Jev loop, with the values from the original
//! `config.py`. Change values here, not in the engine.
//!
//! Environment overrides exist so the loop can be tuned without a rebuild:
//! `JCODE_JEV_LOOP_PLANNER_MODEL`, `JCODE_JEV_LOOP_HELPER_MODEL`,
//! `JCODE_JEV_LOOP_PLANNER_EFFORT`, `JCODE_JEV_LOOP_HELPER_EFFORT`,
//! `JCODE_JEV_LOOP_THRESHOLD`, `JCODE_JEV_LOOP_MAX_ATTEMPTS`,
//! `JCODE_JEV_LOOP_MAX_REVISIONS`, `JCODE_JEV_LOOP_HELPER_MAX_TOOL_CALLS`,
//! `JCODE_JEV_LOOP_STEP_BUDGET_USD`, and `JCODE_JEV_LOOP_FORK_LOG`.

use std::path::PathBuf;

/// Plans, decides split forks, revises stuck steps, reviews the change.
pub const PLANNER_MODEL: &str = "claude-opus-5-5";
/// Does each step (edits and checks).
pub const HELPER_MODEL: &str = "claude-sonnet-5-5";
pub const PLANNER_EFFORT: &str = "high";
pub const HELPER_EFFORT: &str = "medium";

/// At or above this confidence, code acts on Jev's answer ("sharp").
/// Below it, the planner decides ("split"). Tune it from the fork log.
pub const JEV_CONFIDENCE_THRESHOLD: f64 = 0.80;

/// After this many attempts a step is escalated no matter what.
pub const MAX_ATTEMPTS_PER_STEP: u32 = 3;
/// How many times the planner may rewrite a stuck step.
pub const MAX_REVISIONS_PER_STEP: u32 = 1;
/// Tool calls per helper attempt (the Agent SDK's `max_turns`).
pub const HELPER_MAX_TOOL_CALLS: u32 = 40;
/// Estimated spending cap per helper attempt.
pub const STEP_BUDGET_USD: f64 = 2.00;
/// Tool calls for the planner's read-only judge / revise / review sessions.
pub const JUDGE_MAX_TOOL_CALLS: u32 = 8;
pub const REVISE_MAX_TOOL_CALLS: u32 = 15;
pub const REVIEW_MAX_TOOL_CALLS: u32 = 25;
pub const PLAN_MAX_TOOL_CALLS: u32 = 40;

/// Fork log file name. The loop writes it under `~/.jcode/jev-loop/` (not
/// inside the target repo, where it would dirty the tree the loop edits).
pub const FORK_LOG: &str = "forks.jsonl";

/// Commands no session in this loop may run, whatever a model says. Matched
/// against every shell command segment (so `cd x && git push` is caught),
/// after wrappers (`env`, `timeout`, `npx`, `uv run`, `python -m`, ...) and
/// a program's own options before its subcommand are skipped. A one-word
/// entry blocks the whole program; a longer entry blocks a subcommand, and
/// any words after the subcommand must appear somewhere in the arguments.
///
/// Beyond the prototype's five entries, everything here changes state
/// outside the working tree, where `git diff` cannot show it and git cannot
/// undo it. This is a denylist on the command line, not a sandbox.
pub const BLOCKED_COMMANDS: &[&[&str]] = &[
    // The prototype's list.
    &["git", "push"],
    &["git", "reset", "--hard"],
    &["git", "clean"],
    &["rm", "-rf"],
    &["rm", "-fr"],
    &["sudo"],
    // Other git commands that send work out of the repo.
    &["git", "lfs", "push"],
    &["git", "send-email"],
    // Whole programs whose normal job is acting on remote systems.
    &["gh"],
    &["aws"],
    &["awscli"], // `python -m awscli`
    &["gcloud"],
    &["az"],
    &["kubectl"],
    &["copilot"],
    &["eb"],
    &["ecs-cli"],
    &["ssh"],
    &["scp"],
    &["sftp"],
    &["vercel"],
    &["heroku"],
    &["netlify"],
    &["netlify-cli"],
    &["fly"],
    &["flyctl"],
    &["wrangler"],
    // Subcommands that change remote state, for programs that also have
    // local uses the loop needs as checks (`terraform plan`, `helm lint`,
    // `cdk synth`, `sam local`, `firebase emulators:exec`).
    &["terraform", "apply"],
    &["terraform", "destroy"],
    &["terraform", "import"],
    &["terraform", "refresh"],
    &["terraform", "state"],
    &["terraform", "taint"],
    &["terraform", "untaint"],
    &["terraform", "force-unlock"],
    &["terraform", "workspace"],
    &["terraform", "init", "-migrate-state"],
    &["tofu", "apply"],
    &["tofu", "destroy"],
    &["tofu", "import"],
    &["tofu", "refresh"],
    &["tofu", "state"],
    &["tofu", "taint"],
    &["tofu", "untaint"],
    &["tofu", "force-unlock"],
    &["tofu", "workspace"],
    &["tofu", "init", "-migrate-state"],
    &["pulumi", "up"],
    &["pulumi", "destroy"],
    &["pulumi", "refresh"],
    &["pulumi", "import"],
    &["pulumi", "state"],
    &["cdk", "deploy"],
    &["cdk", "destroy"],
    &["cdk", "bootstrap"],
    &["aws-cdk", "deploy"], // `npx aws-cdk deploy`
    &["aws-cdk", "destroy"],
    &["aws-cdk", "bootstrap"],
    &["sam", "deploy"],
    &["sam", "delete"],
    &["sam", "sync"],
    &["serverless", "deploy"],
    &["serverless", "remove"],
    &["sls", "deploy"],
    &["sls", "remove"],
    &["helm", "install"],
    &["helm", "upgrade"],
    &["helm", "uninstall"],
    &["helm", "rollback"],
    &["helm", "push"],
    &["firebase", "deploy"],
    &["firebase-tools", "deploy"], // `npx firebase-tools deploy`
    // Container image pushes.
    &["docker", "push"],
    &["docker", "image", "push"],
    &["docker", "manifest", "push"],
    &["docker", "compose", "push"],
    &["docker", "build", "--push"],
    &["docker", "buildx", "build", "--push"],
    &["docker", "buildx", "bake", "--push"],
    &["docker-compose", "push"],
    &["podman", "push"],
    // Package registry changes.
    &["npm", "publish"],
    &["npm", "unpublish"],
    &["npm", "deprecate"],
    &["npm", "dist-tag"],
    &["pnpm", "publish"],
    &["yarn", "publish"],
    &["yarn", "npm", "publish"],
    &["cargo", "publish"],
    &["cargo", "yank"],
    &["cargo", "owner"],
    &["twine", "upload"],
    &["uv", "publish"],
    &["poetry", "publish"],
    &["gem", "push"],
    &["gem", "yank"],
];

#[derive(Clone, Debug, PartialEq)]
pub struct LoopConfig {
    pub planner_model: String,
    pub helper_model: String,
    pub planner_effort: String,
    pub helper_effort: String,
    pub jev_confidence_threshold: f64,
    pub max_attempts_per_step: u32,
    pub max_revisions_per_step: u32,
    pub helper_max_tool_calls: u32,
    pub step_budget_usd: f64,
    pub fork_log: PathBuf,
}

impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            planner_model: PLANNER_MODEL.into(),
            helper_model: HELPER_MODEL.into(),
            planner_effort: PLANNER_EFFORT.into(),
            helper_effort: HELPER_EFFORT.into(),
            jev_confidence_threshold: JEV_CONFIDENCE_THRESHOLD,
            max_attempts_per_step: MAX_ATTEMPTS_PER_STEP,
            max_revisions_per_step: MAX_REVISIONS_PER_STEP,
            helper_max_tool_calls: HELPER_MAX_TOOL_CALLS,
            step_budget_usd: STEP_BUDGET_USD,
            fork_log: PathBuf::from(FORK_LOG),
        }
    }
}

impl LoopConfig {
    /// Defaults, then `JCODE_JEV_LOOP_*` environment overrides. Invalid
    /// override values are rejected rather than silently ignored.
    pub fn from_env() -> anyhow::Result<Self> {
        let default_log = crate::storage::jcode_dir()
            .map(|dir| dir.join("jev-loop").join(FORK_LOG))
            .unwrap_or_else(|_| PathBuf::from(FORK_LOG));
        Self::from_lookup(
            |key| match std::env::var(key) {
                Ok(value) => Ok(Some(value)),
                Err(std::env::VarError::NotPresent) => Ok(None),
                Err(std::env::VarError::NotUnicode(_)) => {
                    anyhow::bail!("{key} must be valid UTF-8")
                }
            },
            default_log,
        )
    }

    pub(crate) fn from_lookup(
        lookup: impl Fn(&str) -> anyhow::Result<Option<String>>,
        default_fork_log: PathBuf,
    ) -> anyhow::Result<Self> {
        let mut config = Self {
            fork_log: default_fork_log,
            ..Self::default()
        };
        let text = |key: &str| -> anyhow::Result<Option<String>> {
            Ok(lookup(key)?.map(|v| v.trim().to_string()))
        };
        if let Some(v) = text("JCODE_JEV_LOOP_PLANNER_MODEL")?.filter(|v| !v.is_empty()) {
            config.planner_model = v;
        }
        if let Some(v) = text("JCODE_JEV_LOOP_HELPER_MODEL")?.filter(|v| !v.is_empty()) {
            config.helper_model = v;
        }
        if let Some(v) = text("JCODE_JEV_LOOP_PLANNER_EFFORT")?.filter(|v| !v.is_empty()) {
            config.planner_effort = v;
        }
        if let Some(v) = text("JCODE_JEV_LOOP_HELPER_EFFORT")?.filter(|v| !v.is_empty()) {
            config.helper_effort = v;
        }
        if let Some(v) = text("JCODE_JEV_LOOP_THRESHOLD")? {
            config.jev_confidence_threshold = parse_threshold(&v)?;
        }
        if let Some(v) = text("JCODE_JEV_LOOP_MAX_ATTEMPTS")? {
            config.max_attempts_per_step = parse_positive("JCODE_JEV_LOOP_MAX_ATTEMPTS", &v)?;
        }
        if let Some(v) = text("JCODE_JEV_LOOP_MAX_REVISIONS")? {
            config.max_revisions_per_step = v.parse().map_err(|_| {
                anyhow::anyhow!("JCODE_JEV_LOOP_MAX_REVISIONS must be a whole number")
            })?;
        }
        if let Some(v) = text("JCODE_JEV_LOOP_HELPER_MAX_TOOL_CALLS")? {
            config.helper_max_tool_calls =
                parse_positive("JCODE_JEV_LOOP_HELPER_MAX_TOOL_CALLS", &v)?;
        }
        if let Some(v) = text("JCODE_JEV_LOOP_STEP_BUDGET_USD")? {
            let budget: f64 = v.parse().map_err(|_| {
                anyhow::anyhow!("JCODE_JEV_LOOP_STEP_BUDGET_USD must be a number of dollars")
            })?;
            anyhow::ensure!(
                budget.is_finite() && budget > 0.0,
                "JCODE_JEV_LOOP_STEP_BUDGET_USD must be greater than zero"
            );
            config.step_budget_usd = budget;
        }
        if let Some(v) = text("JCODE_JEV_LOOP_FORK_LOG")?.filter(|v| !v.is_empty()) {
            config.fork_log = PathBuf::from(v);
        }
        Ok(config)
    }
}

pub(crate) fn parse_threshold(value: &str) -> anyhow::Result<f64> {
    let threshold: f64 = value
        .trim()
        .parse()
        .map_err(|_| anyhow::anyhow!("The Jev threshold must be a number between 0 and 1"))?;
    anyhow::ensure!(
        threshold.is_finite() && (0.0..=1.0).contains(&threshold),
        "The Jev threshold must be between 0 and 1"
    );
    Ok(threshold)
}

fn parse_positive(key: &str, value: &str) -> anyhow::Result<u32> {
    let parsed: u32 = value
        .parse()
        .map_err(|_| anyhow::anyhow!("{key} must be a whole number"))?;
    anyhow::ensure!(parsed > 0, "{key} must be at least 1");
    Ok(parsed)
}
