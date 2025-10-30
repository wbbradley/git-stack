#![allow(dead_code, unused_imports, unused_variables)]
use std::{env, fs::canonicalize};

use anyhow::{Context, Result, anyhow, bail, ensure};
use clap::{Parser, Subcommand};
use colored::Colorize;
use git::{
    DEFAULT_REMOTE,
    GitBranchStatus,
    after_text,
    git_branch_status,
    git_checkout_main,
    git_fetch,
    git_get_upstream,
    git_remote_main,
    git_sha,
    is_ancestor,
    run_git_status,
    shas_match,
};
use state::{Branch, RestackStep, StackMethod};
use tracing::level_filters::LevelFilter;
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt};

use crate::{git::run_git, state::State}; //prelude::*;

mod git;
mod state;

const CREATE_BACKUP: bool = false;

// This is an important refactoring.
#[derive(Parser)]
#[command(author, version, about)]
struct Args {
    #[arg(long, short, help = "Enable verbose output")]
    verbose: bool,

    /// Subcommand to run.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Show the status of the git-stack tree in the current repo. This is the default command when
    /// one is omitted. (ie: `git stack` is the same as `git stack status`)
    Status {
        /// Whether to fetch the latest changes from the remote before showing the status.
        #[arg(long, short, default_value_t = false)]
        fetch: bool,
    },
    /// Open the git-stack state file in an editor for manual editing.
    Edit,
    /// Restack your active branch and all branches in its related stack.
    Restack {
        /// The name of the branch to restack.
        #[arg(long, short)]
        branch: Option<String>,
        /// Whether to fetch the latest changes from the remote before restacking.
        #[arg(long, short, default_value_t = false)]
        fetch: bool,
        /// Push any changes up to the remote after restacking.
        #[arg(long, short)]
        push: bool,
    },
    /// Shows the log between the given branch and its parent (git-stack tree) branch.
    Log {
        /// Specifies the branch whose log should be shown. If omitted, the current branch will
        /// be used.
        branch: Option<String>,
    },
    /// Show or edit per-branch notes.
    Note {
        #[arg(long, short, default_value_t = false)]
        edit: bool,
        /// Specifies the branch whose note should be shown. If omitted, the current branch will
        /// be used.
        branch: Option<String>,
    },
    /// Shows the diff between the given branch and its parent (git-stack tree) branch.
    Diff {
        /// Specifies the branch whose diff should be shown. If omitted, the current branch will
        /// be used.
        branch: Option<String>,
    },
    /// Create a new branch and make it a descendent of the current branch. If the branch already
    /// exists, then it will simply be checked out.
    Checkout {
        /// The name of the branch to check out.
        branch_name: String,
    },
    /// Mount the current branch on top of the named parent branch. If no parent branch is named,
    /// then the trunk branch will be used.
    Mount {
        /// The name of the parent branch upon which to stack the current branch.
        parent_branch: Option<String>,
    },
    /// Delete a branch from the git-stack tree.
    Delete {
        /// The name of the branch to delete.
        branch_name: String,
    },
    /// Clean up branches from the git-stack tree that no longer exist locally.
    Cleanup {
        /// Show what would be cleaned up without actually removing anything.
        #[arg(long, short, default_value_t = false)]
        dry_run: bool,
        /// Clean up all trees in the config, removing invalid repos and cleaning branches.
        #[arg(long, short, default_value_t = false)]
        all: bool,
    },
}

fn main() {
    tracing_subscriber::registry()
        // We don't need timestamps in the logs.
        .with(
            tracing_subscriber::fmt::layer()
                .with_file(true)
                .with_line_number(true)
                .without_time(),
        )
        // Allow usage of RUST_LOG environment variable to set the log level.
        .with(
            tracing_subscriber::EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .init();
    if let Err(e) = inner_main() {
        tracing::error!(error = ?e);
        std::process::exit(1);
    }
    std::process::exit(0);
}

fn inner_main() -> Result<()> {
    // Run from the git root directory.
    let args = Args::parse();

    let repo = canonicalize(
        run_git(&["rev-parse", "--show-toplevel"])?.output_or("No git directory found")?,
    )?
    .into_os_string()
    .into_string()
    .map_err(|error| anyhow!("Invalid git directory: '{}'", error.to_string_lossy()))?;

    let mut state = State::load_state().context("loading state")?;
    state.refresh_lkgs(&repo)?;

    tracing::debug!("Current directory: {}", repo);

    let run_version = format!("{}", chrono::Utc::now().timestamp());
    let current_branch = run_git(&["rev-parse", "--abbrev-ref", "HEAD"])?
        .output()
        .ok_or(anyhow!("No current branch?"))?;
    let current_upstream = git_get_upstream("");
    tracing::debug!(run_version, current_branch, current_upstream);

    match args.command {
        Some(Command::Checkout { branch_name }) => {
            state.checkout(&repo, current_branch, current_upstream, branch_name)
        }
        Some(Command::Edit) => state.edit_config(),
        Some(Command::Restack {
            branch,
            fetch,
            push,
        }) => restack(
            state,
            &repo,
            run_version,
            branch,
            current_branch,
            fetch,
            push,
        ),
        Some(Command::Mount { parent_branch }) => {
            state.mount(&repo, &current_branch, parent_branch)
        }
        Some(Command::Status { fetch }) => status(state, &repo, &current_branch, fetch),
        Some(Command::Delete { branch_name }) => state.delete_branch(&repo, &branch_name),
        Some(Command::Cleanup { dry_run, all }) => {
            state.cleanup_missing_branches(&repo, dry_run, all)
        }
        Some(Command::Diff { branch }) => diff(state, &repo, &branch.unwrap_or(current_branch)),
        Some(Command::Log { branch }) => show_log(state, &repo, &branch.unwrap_or(current_branch)),
        Some(Command::Note { edit, branch }) => {
            let branch = branch.unwrap_or(current_branch);
            if edit {
                state.edit_note(&repo, &branch)
            } else {
                state.show_note(&repo, &branch)
            }
        }
        None => status(state, &repo, &current_branch, false),
    }
}

fn diff(state: State, repo: &str, branch: &str) -> Result<()> {
    let parent_branch = state
        .get_parent_branch_of(repo, branch)
        .ok_or_else(|| anyhow!("No parent branch found for current branch: {}", branch))?;
    tracing::debug!(
        parent_branch = &parent_branch.name,
        branch = branch,
        "Diffing branches"
    );
    let branch = state
        .get_tree_branch(repo, branch)
        .ok_or_else(|| anyhow!("No branch found for current branch: {}", branch))?;
    let status = git::run_git_passthrough(&[
        "diff",
        &format!(
            "{}..{}",
            branch.lkg_parent.as_deref().unwrap_or(&parent_branch.name),
            branch.name
        ),
    ])?;
    if !status.success() {
        bail!("git format-patch failed");
    }
    Ok(())
}

fn show_log(state: State, repo: &str, branch: &str) -> Result<()> {
    let parent_branch = state
        .get_parent_branch_of(repo, branch)
        .ok_or_else(|| anyhow!("No parent branch found for current branch: {}", branch))?;
    tracing::debug!(
        parent_branch = &parent_branch.name,
        branch = branch,
        "Log changes"
    );
    let status = git::run_git_passthrough(&[
        "log",
        "--graph",
        "--oneline",
        "-p",
        "--decorate",
        &format!("{}..{}", &parent_branch.name, branch),
    ])?;
    if !status.success() {
        bail!("git format-patch failed");
    }
    Ok(())
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
    let Ok(branch_status) = git_branch_status(parent_branch, &branch.name).with_context(|| {
        format!(
            "attempting to fetch the branch status of {}",
            branch.name.red()
        )
    }) else {
        tracing::warn!("Branch {} does not exist", branch.name);
        return Ok(());
    };
    let is_current_branch = if branch.name == orig_branch {
        print!("{} ", selection_marker().bright_purple().bold());
        true
    } else {
        print!("  ");
        false
    };

    for _ in 0..depth {
        print!("{}", "┃ ".truecolor(55, 55, 50));
    }

    println!(
        "{} ({}) {}{}{}{}",
        match (is_current_branch, branch_status.is_descendent) {
            (true, true) => branch.name.truecolor(142, 192, 124).bold(),
            (true, false) => branch.name.truecolor(215, 153, 33).bold(),
            (false, true) => branch.name.truecolor(142, 192, 124),
            (false, false) => branch.name.truecolor(215, 153, 33),
        },
        branch_status.sha[..8].truecolor(215, 153, 33),
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
                        "diverges from".red(),
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
                    " (upstream {} is {})",
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
        },
        {
            if let Some(lkg_parent) = branch.lkg_parent.as_ref() {
                format!(" (lkg parent {})", lkg_parent[..8].truecolor(215, 153, 33))
            } else {
                String::new()
            }
        },
        match branch.stack_method {
            StackMethod::ApplyMerge => " (apply-merge)".truecolor(142, 192, 124),
            StackMethod::Merge => " (merge)".truecolor(142, 192, 124),
        },
    );
    if let Some(note) = &branch.note {
        print!("  ");
        for _ in 0..depth {
            print!("{}", "┃ ".truecolor(55, 55, 50));
        }

        let first_line = note.lines().next().unwrap_or("");
        println!(
            "  {} {}",
            "›".truecolor(55, 55, 50),
            if is_current_branch {
                first_line.bright_blue().bold()
            } else {
                first_line.blue()
            }
        );
    }

    let mut branches_sorted = branch.branches.iter().collect::<Vec<_>>();
    branches_sorted.sort_by(|&a, &b| {
        let a_is_ancestor = is_ancestor(&a.name, orig_branch).unwrap_or(false);
        let b_is_ancestor = is_ancestor(&b.name, orig_branch).unwrap_or(false);
        match (a_is_ancestor, b_is_ancestor) {
            (true, true) => a.name.cmp(&b.name),
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            (false, false) => a.name.cmp(&b.name),
        }
    });
    for child in branches_sorted {
        recur_tree(child, depth + 1, orig_branch, Some(branch.name.as_ref()))?;
    }
    Ok(())
}

fn status(mut state: State, repo: &str, orig_branch: &str, fetch: bool) -> Result<()> {
    if fetch {
        git_fetch()?;
    }
    let trunk = state.ensure_trunk(repo)?;

    let Some(tree) = state.get_tree_mut(repo) else {
        eprintln!(
            "No stack tree found for repo {repo}.",
            repo = repo.truecolor(178, 178, 218)
        );
        return Ok(());
    };
    recur_tree(tree, 0, orig_branch, None)?;
    if !state.branch_exists_in_tree(repo, orig_branch) {
        eprintln!(
            "The current branch {} is not in the stack tree.",
            orig_branch.red()
        );
        eprintln!("Run `git stack mount <parent_branch>` to add it.");
    }
    state.save_state()?;
    Ok(())
}

fn restack(
    mut state: State,
    repo: &str,
    run_version: String,
    restack_branch: Option<String>,
    orig_branch: String,
    fetch: bool,
    push: bool,
) -> Result<(), anyhow::Error> {
    let restack_branch = restack_branch.unwrap_or(orig_branch.clone());

    if fetch {
        git_fetch()?;
    }

    // Find starting_branch in the stacks of branches to determine which stack to use.
    let plan = state.plan_restack(repo, &restack_branch)?;

    tracing::debug!(?plan, "Restacking branches with plan. Checking out main...");
    git_checkout_main(None)?;
    for RestackStep { parent, branch } in plan {
        tracing::debug!(
            "Starting branch: {} [pwd={}]",
            restack_branch,
            env::current_dir()?.display()
        );
        let source = git_sha(&branch.name)?;

        if is_ancestor(&parent, &branch.name)? {
            tracing::debug!(
                "Branch '{}' is already stacked on '{}'.",
                branch.name,
                parent
            );
            if push && !shas_match(&format!("{DEFAULT_REMOTE}/{}", branch.name), &branch.name) {
                run_git(&[
                    "push",
                    match branch.stack_method {
                        StackMethod::ApplyMerge => {
                            tracing::debug!(
                                "Force-pushing '{}' to {DEFAULT_REMOTE}...",
                                branch.name
                            );
                            "-fu"
                        }
                        StackMethod::Merge => "-u",
                    },
                    DEFAULT_REMOTE,
                    &format!("{branch_name}:{branch_name}", branch_name = branch.name),
                ])?;
            }
        } else {
            tracing::info!("Branch '{}' is not stacked on '{}'...", branch.name, parent);
            make_backup(&run_version, branch, &source)?;

            match branch.stack_method {
                StackMethod::ApplyMerge => {
                    if let Some(lkg_parent) = branch.lkg_parent.as_deref() {
                        tracing::info!("LKG parent: {}", lkg_parent);
                        if is_ancestor(lkg_parent, &source)? {
                            let patch_rev = format!("{}..{}", &lkg_parent, &branch.name);
                            tracing::info!("Creating patch {}", &patch_rev);
                            // The branch is still on top of the LKG parent. Let's create a format-patch of the
                            // difference, and apply it on top of the new parent.
                            let format_patch =
                                run_git(&["format-patch", "--stdout", &patch_rev])?.output();
                            run_git(&["checkout", "-B", &branch.name, &parent])?;
                            let Some(format_patch) = format_patch else {
                                tracing::debug!("No diff between LKG and branch?!");
                                continue;
                            };
                            tracing::info!("Applying patch...");
                            let rebased =
                                run_git_status(&["am", "--3way"], Some(&format_patch))?.success();
                            if !rebased {
                                eprintln!(
                                    "{} did not complete successfully.",
                                    "`git am`".green().bold()
                                );
                                eprintln!("Run `git mergetool` to resolve conflicts.");
                                eprintln!(
                                    "Once you have finished with {}, re-run `git stack restack`.",
                                    "`git am --continue`".green().bold()
                                );
                                std::process::exit(1);
                            }
                            if push {
                                git_push(&branch.name)?;
                            }
                            continue;
                        } else {
                            tracing::info!(
                                "Branch '{}' is not on top of the LKG parent. Using `git rebase`...",
                                branch.name
                            );
                            run_git(&["checkout", &branch.name])?;
                            let rebased = run_git_status(&["rebase", &parent], None)?.success();

                            if !rebased {
                                eprintln!(
                                    "{} did not complete automatically.",
                                    "Rebase".blue().bold()
                                );
                                eprintln!("Run `git mergetool` to resolve conflicts.");
                                eprintln!(
                                    "Once you have finished the {}, re-run this script.",
                                    "rebase".blue().bold()
                                );
                                std::process::exit(1);
                            }
                            if push {
                                git_push(&branch.name)?;
                            }
                            tracing::info!("Rebase completed successfully. Continuing...");
                        }
                    }
                }
                StackMethod::Merge => {
                    run_git(&["checkout", &branch.name])
                        .with_context(|| format!("checking out {}", branch.name))?;
                    run_git(&["merge", &parent])
                        .with_context(|| format!("merging {parent} into {}", branch.name))?;
                }
            }
        }
    }
    tracing::debug!("Restoring starting branch '{}'...", restack_branch);
    ensure!(
        run_git_status(&["checkout", "-q", &orig_branch], None)?.success(),
        "git checkout {} failed",
        restack_branch
    );
    tracing::info!("Done.");
    state.refresh_lkgs(repo)?;
    Ok(())
}

fn git_push(branch: &str) -> Result<()> {
    if !shas_match(&format!("{DEFAULT_REMOTE}/{}", branch), branch) {
        run_git(&[
            "push",
            "-fu",
            DEFAULT_REMOTE,
            &format!("{}:{}", branch, branch),
        ])?;
    }
    Ok(())
}

fn make_backup(run_version: &String, branch: &Branch, source: &str) -> Result<(), anyhow::Error> {
    if !CREATE_BACKUP {
        return Ok(());
    }
    let backup_branch = format!("{}-at-{}", branch.name, run_version);
    tracing::debug!(
        "Creating backup branch '{}' from '{}'...",
        backup_branch,
        branch.name
    );
    if !run_git_status(&["branch", &backup_branch, source], None)?.success() {
        tracing::warn!("failed to create backup branch {}", backup_branch);
    }
    Ok(())
}
