//! `jcode jev-loop`: run the Jev fork-layer coding loop against a git repo.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::jev_loop::{
    self, JcodeClaude, JevLayer, LoopConfig, Mode, Parts, RepoWorkspace, RunOptions,
};

#[derive(clap::Args, Debug, Clone)]
pub(crate) struct JevLoopArgs {
    /// Path to the git repository to work in
    #[arg(long, value_name = "PATH")]
    pub repo: PathBuf,

    /// What you want done, in plain words
    #[arg(long)]
    pub task: String,

    /// Baseline run: Jev is never called, and Opus reviews every step whose
    /// check passed
    #[arg(long, conflicts_with = "rules_only")]
    pub no_jev: bool,

    /// Baseline run: no Jev and no Opus judge. A step is accepted as soon as
    /// the loop sees its check pass
    #[arg(long)]
    pub rules_only: bool,

    /// Run even with uncommitted changes (not recommended)
    #[arg(long)]
    pub allow_dirty: bool,
}

impl JevLoopArgs {
    fn mode(&self) -> Mode {
        if self.rules_only {
            Mode::RulesOnly
        } else if self.no_jev {
            Mode::NoJev
        } else {
            Mode::Jev
        }
    }
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
    let mode = args.mode();

    let transport: Option<Box<dyn jev_loop::fork::DecisionTransport>> = match mode {
        Mode::Jev => {
            let client = crate::jev::JevClient::for_loop().context(
                "Jev is not configured. Set TYPESAFE_API_KEY (or ~/.config/jcode/typesafe.env), or run with --no-jev or --rules-only",
            )?;
            println!(
                "Jev: on ({} {}): picks files, and reviews passing steps (accepted at confidence >= {:.2})",
                client.provider_name(),
                client.model_id(),
                config.jev_confidence_threshold
            );
            Some(Box::new(client))
        }
        Mode::NoJev => {
            println!("Jev: off (--no-jev baseline: Opus reviews every passing step)");
            None
        }
        Mode::RulesOnly => {
            println!(
                "Jev: off (--rules-only baseline: a step is done when the loop sees its check pass)"
            );
            None
        }
    };
    let jev = JevLayer::new(transport, config.jev_confidence_threshold);

    let provider = super::provider_init::init_provider_quiet(provider_choice, model).await?;
    println!(
        "Planner: {} ({})   Helper: {} ({})",
        config.planner_model, config.planner_effort, config.helper_model, config.helper_effort
    );
    println!("Repo: {}", repo.display());

    let workspace = RepoWorkspace::new(
        repo.clone(),
        std::time::Duration::from_secs(config.check_timeout_secs),
    );
    let claude = JcodeClaude::new(provider, repo, config.clone());
    let options = RunOptions {
        task: args.task,
        config,
        mode,
    };
    let parts = Parts {
        claude: &claude,
        jev: &jev,
        workspace: &workspace,
    };
    let mut stdout = std::io::stdout();
    let summary = jev_loop::run(&options, parts, &mut stdout).await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: JevLoopArgs,
    }

    fn parse(extra: &[&str]) -> Result<JevLoopArgs, clap::Error> {
        let mut argv = vec!["jev-loop", "--repo", ".", "--task", "x"];
        argv.extend_from_slice(extra);
        Cli::try_parse_from(argv).map(|cli| cli.args)
    }

    #[test]
    fn flags_select_the_three_measurement_arms() {
        assert_eq!(parse(&[]).unwrap().mode(), Mode::Jev);
        assert_eq!(parse(&["--no-jev"]).unwrap().mode(), Mode::NoJev);
        assert_eq!(parse(&["--rules-only"]).unwrap().mode(), Mode::RulesOnly);
        let both = parse(&["--no-jev", "--rules-only"]).unwrap_err();
        assert_eq!(both.kind(), clap::error::ErrorKind::ArgumentConflict);
    }
}
