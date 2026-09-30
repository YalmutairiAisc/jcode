//! `jcode jev-loop`: run the Jev fork-layer coding loop against a git repo.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::jev_loop::{self, JcodeClaude, JevLayer, LoopConfig, RunOptions};

#[derive(clap::Args, Debug, Clone)]
pub(crate) struct JevLoopArgs {
    /// Path to the git repository to work in
    #[arg(long, value_name = "PATH")]
    pub repo: PathBuf,

    /// What you want done, in plain words
    #[arg(long)]
    pub task: String,

    /// Baseline run: every fork goes to the planner and Jev is never called
    #[arg(long)]
    pub no_jev: bool,

    /// Run even with uncommitted changes (not recommended)
    #[arg(long)]
    pub allow_dirty: bool,
}

pub(crate) async fn run_jev_loop_command(
    provider_choice: &super::provider_init::ProviderChoice,
    model: Option<&str>,
    args: JevLoopArgs,
) -> Result<()> {
    let repo = resolve_repo(&args.repo)?;
    if !args.allow_dirty && !jev_loop::engine::working_tree_is_clean(&repo)? {
        anyhow::bail!(
            "Commit or stash your changes first, so every edit this loop makes can be reviewed and undone with git."
        );
    }
    let config = LoopConfig::from_env()?;

    let transport: Option<Box<dyn jev_loop::fork::DecisionTransport>> = if args.no_jev {
        None
    } else {
        let client = crate::jev::JevClient::for_loop().context(
            "Jev is not configured. Set TYPESAFE_API_KEY (or ~/.config/jcode/typesafe.env), or run with --no-jev",
        )?;
        println!(
            "Jev: on ({} {}, sharp at confidence >= {:.2})",
            client.provider_name(),
            client.model_id(),
            config.jev_confidence_threshold
        );
        Some(Box::new(client))
    };
    let jev = JevLayer::new(transport, config.jev_confidence_threshold);
    if !jev.enabled() {
        println!("Jev: off (--no-jev baseline, every fork goes to the planner)");
    }

    let provider = super::provider_init::init_provider_quiet(provider_choice, model).await?;
    println!(
        "Planner: {} ({})   Helper: {} ({})",
        config.planner_model, config.planner_effort, config.helper_model, config.helper_effort
    );
    println!("Repo: {}", repo.display());

    let claude = JcodeClaude::new(provider, repo, config.clone());
    let options = RunOptions {
        task: args.task,
        config,
    };
    let mut stdout = std::io::stdout();
    let summary = jev_loop::run(&options, &claude, &jev, &mut stdout).await?;
    if !summary.finished {
        std::process::exit(summary.exit_code());
    }
    Ok(())
}

fn resolve_repo(path: &Path) -> Result<PathBuf> {
    let expanded = match path.strip_prefix("~") {
        Ok(rest) => dirs::home_dir()
            .context("Could not find your home directory to expand ~")?
            .join(rest),
        Err(_) => path.to_path_buf(),
    };
    std::fs::canonicalize(&expanded)
        .with_context(|| format!("Repository path {} does not exist", expanded.display()))
}
