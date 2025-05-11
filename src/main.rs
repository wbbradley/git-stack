#![allow(dead_code, unused_imports, unused_variables)]
use crate::git::run_git;
use crate::state::State;
use colored::Colorize;

use anyhow::{Context, Result, anyhow, bail, ensure};
use clap::{Parser, Subcommand};
use git::{
    DEFAULT_REMOTE, GitBranchStatus, after_text, git_branch_status, git_checkout_main, git_fetch,
    git_remote_main, is_ancestor, run_git_status, shas_match,
};
use state::{Branch, RebaseStep, load_state, save_state};
use std::env;
use std::fs::canonicalize;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt; //prelude::*;

mod git;
mod state;

const CREATE_BACKUP: bool = false;

// This is an important refactoring.
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
    /// Show the status of the git-stack tree in the current repo.
    Status,
    /// Restack your active branch and all branches in its related stack.
    Restack,
    /// Create a new branch and make it a descendent of the current branch.
    Checkout { branch_name: String },
    /// Delete a branch from the git-stack tree.
    Delete { branch_name: String },
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

    tracing_subscriber::registry()
        // We don't need timestamps in the logs.
        .with(tracing_subscriber::fmt::layer().without_time())
        // Allow usage of RUST_LOG environment variable to set the log level.
        .with(
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .init();

    let repo = canonicalize(
        run_git(&["rev-parse", "--show-toplevel"])?.output_or("No git directory found")?,
    )?
    .into_os_string()
    .into_string()
    .map_err(|error| anyhow!("Invalid git directory: '{}'", error.to_string_lossy()))?;

    let mut state = load_state().context("loading state")?;

    tracing::debug!("Current directory: {}", repo);

    let run_version = format!("{}", chrono::Utc::now().timestamp());
    let current_branch = run_git(&["rev-parse", "--abbrev-ref", "HEAD"])?
        .output()
        .ok_or(anyhow!("No current branch?"))?;
    let current_upstream = run_git(&["rev-parse", "--abbrev-ref", "@{upstream}"])
        .ok()
        .and_then(|out| out.output());
    tracing::debug!(run_version, current_branch, current_upstream);

    match args.command {
        Commands::Checkout { branch_name } => {
            state.checkout(&repo, current_branch, current_upstream, branch_name)
        }
        Commands::Restack => restack(state, &repo, run_version, current_branch),
        Commands::Status => status(state, &repo, &current_branch),
        Commands::Delete { branch_name } => state.delete_branch(&repo, &branch_name),
    }
}

fn selection_marker() -> &'static str {
    if cfg!(target_os = "windows") {
        ">"
    } else {
        "→"
    }
}

fn recur_tree(
    branch: &Branch,
    depth: usize,
    orig_branch: &str,
    parent_branch: Option<&str>,
) -> Result<()> {
    let branch_status: GitBranchStatus = git_branch_status(parent_branch, &branch.name)
        .with_context(|| {
            format!(
                "attempting to fetch the branch status of {}",
                branch.name.red()
            )
        })?;
    let is_current_branch = if branch.name == orig_branch {
        print!("{} ", selection_marker().purple());
        true
    } else {
        print!("  ");
        false
    };

    for _ in 0..depth {
        print!("  ");
    }

    println!(
        "{} {}{}",
        if is_current_branch {
            branch.name.green()
        } else {
            branch.name.truecolor(178, 178, 178)
        },
        {
            let details: String = if branch_status.exists {
                if branch_status.is_descendent {
                    format!(
                        "{} {}",
                        "is stacked on".truecolor(90, 120, 87),
                        branch_status.parent_branch.yellow()
                    )
                } else {
                    format!(
                        "{} {}",
                        "has drifted from".red(),
                        branch_status.parent_branch.yellow()
                    )
                }
            } else {
                "does not exist!".bright_red().to_string()
            };
            details
        },
        {
            if let Some(upstream_status) = branch_status.upstream_status {
                format!(
                    " (upstream {} {})",
                    upstream_status.symbolic_name.truecolor(88, 88, 88),
                    if upstream_status.synced {
                        "synced".truecolor(142, 192, 124)
                    } else {
                        "not synced".bright_red()
                    }
                )
            } else {
                format!(" ({})", "no upstream".truecolor(215, 153, 33))
            }
        }
    );

    for child in &branch.branches {
        recur_tree(child, depth + 1, orig_branch, Some(branch.name.as_ref()))?;
    }
    Ok(())
}

fn status(state: State, repo: &str, orig_branch: &str) -> Result<()> {
    git_fetch()?;

    let Some(tree) = state.get_tree(repo) else {
        eprintln!(
            "No stack tree found for repo {repo}.",
            repo = repo.truecolor(178, 178, 218)
        );
        return Ok(());
    };
    recur_tree(tree, 0, orig_branch, None)?;
    /*
    let stacks = state.get_stacks(repo);
    if stacks.is_empty() {
        println!("No stacks found.");
        return Ok(());
    }
    let orig_branch = orig_branch.to_string();
    let mut saw_stack = false;
    let remote_main = after_text(&git_remote_main(DEFAULT_REMOTE)?, "remotes/")
        .expect("remote main")
        .to_string();
    for (i, stack) in stacks.iter().enumerate().map(|(i, s)| (i + 1, s)) {
        let stack_header: String = format!("▤ stack {i}").truecolor(148, 148, 158).to_string();
        println!("{}", stack_header);
        let mut last_branch = None;
        let current_stack = stack.contains(&orig_branch);
        saw_stack = saw_stack || current_stack;
        for branch in stack.iter() {
            let branch_status: GitBranchStatus = git_branch_status(last_branch.clone(), branch)?;
            if branch == &orig_branch {
                print!("  {} ", selection_marker().purple());
            } else {
                print!("    ");
            }
            println!(
                "{} {}",
                if current_stack {
                    branch.green()
                } else {
                    branch.truecolor(178, 178, 178)
                },
                {
                    let details: String = if branch_status.exists {
                        if branch_status.is_descendent {
                            format!(
                                "{} with {}",
                                "is up to date".truecolor(90, 120, 87),
                                last_branch.unwrap_or(remote_main.clone()).yellow()
                            )
                        } else {
                            format!(
                                "{} {}",
                                "is behind".red(),
                                last_branch.unwrap_or(remote_main.clone()).yellow()
                            )
                        }
                    } else {
                        "does not exist".red().to_string()
                    };
                    details
                }
            );
            last_branch = Some(branch.to_string());
        }
    }
    if !saw_stack {
        println!(
            "No stack found for current branch: '{}'",
            orig_branch.green()
        );
    }
    */
    Ok(())
}

fn restack(
    state: State,
    repo: &str,
    run_version: String,
    starting_branch: String,
) -> Result<(), anyhow::Error> {
    // Find starting_branch in the stacks of branches to determine which stack to use.
    let plan = state.plan_restack(repo, &starting_branch)?;

    tracing::info!(?plan, "Restacking branches with plan...");
    git_checkout_main(None)?;
    for RebaseStep { parent, branch } in plan {
        tracing::info!(
            "Starting branch: {} [pwd={}]",
            starting_branch,
            env::current_dir()?.display()
        );
        let source = run_git(&["rev-parse", &branch])?
            .output_or(format!("branch {branch} does not exist?"))?;

        if is_ancestor(&parent, &branch)? {
            tracing::info!(
                "Branch '{}' is already up to date with '{}'.",
                branch,
                parent
            );
            tracing::info!("Force-pushing '{}' to origin...", branch);
            if !shas_match(&format!("origin/{branch}"), &branch) {
                run_git(&["push", "-fu", "origin", &format!("{}:{}", branch, branch)])?;
            }
            continue;
        } else {
            tracing::info!("Branch '{}' is not descended from '{}'...", branch, parent);
            if CREATE_BACKUP {
                let backup_branch = format!("{}-at-{}", branch, run_version);
                tracing::debug!(
                    "Creating backup branch '{}' from '{}'...",
                    backup_branch,
                    branch
                );
                if !run_git_status(&["branch", &backup_branch, &source])?.success() {
                    tracing::warn!("failed to create backup branch {}", backup_branch);
                }
            }
            tracing::info!("Initiating a rebase of '{}' onto '{}'...", branch, parent);
            if !run_git_status(&["checkout", "-q", &branch])?.success() {
                bail!("git checkout {} failed", branch);
            }
            let rebased = run_git_status(&["rebase", &parent])?.success();
            if !rebased {
                tracing::warn!("Rebase did not complete automatically.");
                tracing::warn!("Run `git mergetool` to resolve conflicts.");
                tracing::info!("Once you have finished the rebase, re-run this script.");
                std::process::exit(1);
            }
            if !shas_match(&format!("origin/{branch}"), &branch) {
                run_git(&["push", "-fu", "origin", &format!("{}:{}", branch, branch)])?;
            }
            tracing::info!("Rebase completed successfully. Continuing...");
        }
    }
    tracing::info!("Restoring starting branch '{}'...", starting_branch);
    ensure!(
        run_git_status(&["checkout", "-q", &starting_branch])?.success(),
        "git checkout {} failed",
        starting_branch
    );
    tracing::info!("Done.");
    Ok(())
}
