use crate::git::run_git;
use crate::state::{Stack, State, load_state};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use state::save_state;
use std::env;

mod git;
mod state;

#[derive(Parser)]
#[command(author, version, about, arg_required_else_help = true)]
struct Args {
    #[arg(long, short, help = "Enable verbose output")]
    verbose: bool,

    /// Subcommand to run.
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// List all stacks in the given directory.
    List,
    /// Show the status of nearby stacks.
    Status,
    /// Restack your active branch and all branches in its related stack.
    Restack,
    /// Create a new stack.
    New { name: String },
}

fn main() {
    if let Err(e) = inner_main() {
        tracing::error!(error = ?e);
        std::process::exit(1);
    }
    std::process::exit(0);
}

fn inner_main() -> Result<()> {
    // Run from the git root directory.
    let args = Args::parse();
    let subscriber = tracing_subscriber::FmtSubscriber::new();
    tracing::subscriber::set_global_default(subscriber)?;

    let dir_key = std::fs::canonicalize(
        run_git(&["rev-parse", "--show-toplevel"])?.output_or("No git directory found")?,
    )?
    .into_os_string()
    .into_string()
    .map_err(|error| anyhow!("Invalid git directory: '{}'", error.to_string_lossy()))?;

    let state = load_state().context("loading state")?;

    tracing::debug!("Current directory: {}", dir_key);

    let run_version = format!("{}", chrono::Utc::now().timestamp());
    let orig_branch = run_git(&["rev-parse", "--abbrev-ref", "HEAD"])?
        .output()
        .ok_or(anyhow!("No current branch?"))?;

    // --verbose: Print current stack and exit.
    /*if args.verbose {
        let stack = stacks.first().ok_or(anyhow!("No stacks found"))?;

        for branch in &stack.branches {
            let source = run_git(&["rev-parse", branch])?
                .output_or(format!("branch '{}' does not exist", branch))?;
            let log_msg = run_git(&["log", "-1", "--pretty=format:%s", source.as_ref()])?;
            if log_msg.is_empty() {
                bail!("branch '{}' has no commit message!?", branch);
            }
            tracing::info!("{}: {}", branch, log_msg.as_ref());
        }
        std::process::exit(0);
    }*/

    tracing::debug!("This is git-stack run version {}.", run_version);

    match args.command {
        Commands::List => list_stacks(&state, &dir_key),
        Commands::New { name } => new_stack(state, &dir_key, &name),
        Commands::Restack => restack(state, &dir_key, run_version, orig_branch),
        Commands::Status => todo!(),
    }
}

fn new_stack(mut state: State, dir_key: &str, branch_name: &str) -> Result<()> {
    if branch_name == "main" {
        bail!("Cannot stack a branch named 'main'");
    }

    state.add_stack(dir_key, branch_name)?;
    save_state(&state)?;
    Ok(())
}

fn list_stacks(state: &State, dir_key: &str) -> std::result::Result<(), anyhow::Error> {
    let stacks: Vec<Stack> = state.get_stacks(dir_key);

    for stack in stacks {
        tracing::info!("Stack: {}", stack.branches.join(", "));
    }
    Ok(())
}

fn restack(
    state: State,
    dir_key: &str,
    run_version: String,
    starting_branch: String,
) -> Result<(), anyhow::Error> {
    // Find starting_branch in the stacks of branches to determine which stack to use.
    let stack = state
        .get_stacks(dir_key)
        .into_iter()
        .find(|stack| stack.branches.contains(&starting_branch))
        .ok_or(anyhow!("No stack found for branch {}", starting_branch))?;
    git::git_checkout_main()?;
    let mut stack_on = "main".to_string();
    for branch in &stack.branches {
        tracing::info!(
            "Starting branch: {} [pwd={}]",
            starting_branch,
            env::current_dir()?.display()
        );
        let source = run_git(&["rev-parse", branch])?
            .output_or(format!("branch {branch} does not exist?"))?;
        let log_msg = run_git(&["log", "-1", "--pretty=format:%s", &source])?
            .output_or(format!("branch '{}' has no commit message!?", branch))?;

        if git::run_git_ok(&["merge-base", "--is-ancestor", &stack_on, branch])? {
            tracing::info!(
                "Branch '{}' is already up to date with '{}'.",
                branch,
                stack_on
            );
            stack_on = branch.to_string();
            tracing::info!("Force-pushing '{}' to origin...", branch);
            if !git::run_git_ok(&["push", "-fu", "origin", &format!("{}:{}", branch, branch)])? {
                bail!("git push -fu origin {}:{} failed", branch, branch);
            }
            continue;
        } else {
            tracing::info!(
                "Branch '{}' is not descended from '{}'...",
                branch,
                stack_on
            );
            let backup_branch = format!("{}-at-{}", branch, run_version);
            tracing::info!(
                "Creating backup branch '{}' from '{}'...",
                backup_branch,
                branch
            );
            if !git::run_git_ok(&["branch", &backup_branch, &source])? {
                bail!("git branch '{}' failed", backup_branch);
            }
            tracing::info!("Initiating a rebase of '{}' onto '{}'...", branch, stack_on);
            tracing::info!(
                "Note: use `git commit -m '{}'` to commit the changes.",
                log_msg
            );
            if !git::run_git_ok(&["checkout", branch])? {
                bail!("git checkout {} failed", branch);
            }
            let rebased = git::run_git_ok(&["rebase", &stack_on]).context("rebase")?;
            if !rebased {
                tracing::warn!("Rebase failed, aborting...");
                tracing::warn!("Run `git mergetool` to resolve conflicts.");
                tracing::info!("Once you have finished the rebase, re-run this script.");
                std::process::exit(1);
            }
            tracing::info!("Rebase completed successfully. Continuing...");
            stack_on = branch.to_string();
        }
    }
    tracing::info!("All branches are up to date with '{}'.", stack_on);
    tracing::info!("Restoring starting branch '{}'...", starting_branch);
    if !git::run_git_ok(&["checkout", &starting_branch])? {
        bail!("git checkout {} failed", starting_branch);
    }
    tracing::info!("Done.");
    Ok(())
}
