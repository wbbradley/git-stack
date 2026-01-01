#![allow(dead_code, unused_imports, unused_variables)]
use std::{env, fs::canonicalize};

use anyhow::{Context, Result, anyhow, bail, ensure};
use clap::{Parser, Subcommand};
use colored::Colorize;
use git::{after_text, git_checkout_main, git_fetch, run_git_status};
use state::{Branch, RestackStep, StackMethod};
use tracing::level_filters::LevelFilter;
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt};

use crate::{
    git::run_git,
    git2_ops::{DEFAULT_REMOTE, GitRepo},
    state::State,
};

mod git;
mod git2_ops;
mod github;
mod state;
mod stats;

const CREATE_BACKUP: bool = false;

// This is an important refactoring.
#[derive(Parser)]
#[command(author, version, about)]
struct Args {
    #[arg(long, short, global = true, help = "Enable verbose output")]
    verbose: bool,

    #[arg(long, global = true, help = "Show git command performance stats")]
    benchmark: bool,

    #[arg(
        long,
        global = true,
        help = "Output benchmark stats as JSON (implies --benchmark)"
    )]
    json: bool,

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
    /// Manage GitHub Pull Requests for stacked branches.
    Pr {
        #[command(subcommand)]
        action: PrAction,
    },
    /// Manage GitHub authentication.
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
}

#[derive(Subcommand)]
enum PrAction {
    /// Create a PR for the current branch with git-stack parent as base.
    Create {
        /// Branch to create PR for (defaults to current branch)
        #[arg(long, short)]
        branch: Option<String>,
        /// PR title (defaults to first commit message)
        #[arg(long, short)]
        title: Option<String>,
        /// PR body/description
        #[arg(long, short = 'm')]
        body: Option<String>,
        /// Create as draft PR
        #[arg(long)]
        draft: bool,
        /// Open PR in browser after creation
        #[arg(long)]
        web: bool,
    },
    /// Open PR in web browser.
    View {
        /// Branch whose PR to view (defaults to current)
        branch: Option<String>,
    },
}

#[derive(Subcommand)]
enum AuthAction {
    /// Set up GitHub authentication interactively.
    Login,
    /// Show current auth status.
    Status,
    /// Remove stored authentication.
    Logout,
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

    let result = inner_main();

    // Check if benchmarking was requested via flag or environment variable
    // Note: We check env var here since Args is consumed by inner_main
    let show_benchmark = std::env::var("GIT_STACK_BENCHMARK").is_ok();
    if show_benchmark {
        if std::env::var("GIT_STACK_BENCHMARK_JSON").is_ok() {
            stats::print_json();
        } else {
            stats::print_summary();
        }
    }

    if let Err(e) = result {
        tracing::error!(error = ?e);
        std::process::exit(1);
    }
    std::process::exit(0);
}

fn inner_main() -> Result<()> {
    // Run from the git root directory.
    let args = Args::parse();

    // Set env vars if benchmark flags were passed (for main() to check later)
    if args.benchmark || args.json {
        // SAFETY: We're single-threaded at this point in startup
        unsafe { std::env::set_var("GIT_STACK_BENCHMARK", "1") };
    }
    if args.json {
        // SAFETY: We're single-threaded at this point in startup
        unsafe { std::env::set_var("GIT_STACK_BENCHMARK_JSON", "1") };
    }

    let repo = canonicalize(
        run_git(&["rev-parse", "--show-toplevel"])?.output_or("No git directory found")?,
    )?
    .into_os_string()
    .into_string()
    .map_err(|error| anyhow!("Invalid git directory: '{}'", error.to_string_lossy()))?;

    // Open git2 repository for fast read-only operations
    let git_repo = GitRepo::open(&repo)?;

    let mut state = State::load_state().context("loading state")?;
    state.refresh_lkgs(&git_repo, &repo)?;

    tracing::debug!("Current directory: {}", repo);

    let run_version = format!("{}", chrono::Utc::now().timestamp());
    let current_branch = git_repo.current_branch()?;
    let current_upstream = git_repo.get_upstream("");
    tracing::debug!(run_version, current_branch, current_upstream);

    match args.command {
        Some(Command::Checkout { branch_name }) => state.checkout(
            &git_repo,
            &repo,
            current_branch,
            current_upstream,
            branch_name,
        ),
        Some(Command::Edit) => state.edit_config(),
        Some(Command::Restack {
            branch,
            fetch,
            push,
        }) => {
            let restack_branch = branch.clone().unwrap_or_else(|| current_branch.clone());
            state.try_auto_mount(&git_repo, &repo, &restack_branch)?;
            restack(
                &git_repo,
                state,
                &repo,
                run_version,
                branch,
                current_branch,
                fetch,
                push,
            )
        }
        Some(Command::Mount { parent_branch }) => {
            state.mount(&git_repo, &repo, &current_branch, parent_branch)
        }
        Some(Command::Status { fetch }) => {
            state.try_auto_mount(&git_repo, &repo, &current_branch)?;
            status(
                &git_repo,
                state,
                &repo,
                &current_branch,
                fetch,
                args.verbose,
            )
        }
        Some(Command::Delete { branch_name }) => state.delete_branch(&repo, &branch_name),
        Some(Command::Cleanup { dry_run, all }) => {
            state.cleanup_missing_branches(&git_repo, &repo, dry_run, all)
        }
        Some(Command::Diff { branch }) => {
            let branch_to_diff = branch.clone().unwrap_or_else(|| current_branch.clone());
            state.try_auto_mount(&git_repo, &repo, &branch_to_diff)?;
            diff(state, &repo, &branch.unwrap_or(current_branch))
        }
        Some(Command::Log { branch }) => {
            let branch_to_log = branch.clone().unwrap_or_else(|| current_branch.clone());
            state.try_auto_mount(&git_repo, &repo, &branch_to_log)?;
            show_log(state, &repo, &branch.unwrap_or(current_branch))
        }
        Some(Command::Note { edit, branch }) => {
            let branch = branch.unwrap_or(current_branch);
            state.try_auto_mount(&git_repo, &repo, &branch)?;
            if edit {
                state.edit_note(&repo, &branch)
            } else {
                state.show_note(&repo, &branch)
            }
        }
        Some(Command::Pr { action }) => {
            handle_pr_command(&git_repo, &mut state, &repo, &current_branch, action)
        }
        Some(Command::Auth { action }) => handle_auth_command(&git_repo, action),
        None => {
            state.try_auto_mount(&git_repo, &repo, &current_branch)?;
            status(
                &git_repo,
                state,
                &repo,
                &current_branch,
                false,
                args.verbose,
            )
        }
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
    git_repo: &GitRepo,
    branch: &Branch,
    depth: usize,
    orig_branch: &str,
    parent_branch: Option<&str>,
    verbose: bool,
    pr_cache: Option<&std::collections::HashMap<String, github::PullRequest>>,
) -> Result<()> {
    let Ok(branch_status) = git_repo
        .branch_status(parent_branch, &branch.name)
        .with_context(|| {
            format!(
                "attempting to fetch the branch status of {}",
                branch.name.red()
            )
        })
    else {
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

    // Branch name coloring: green for synced, red for diverged, bold for current branch
    let branch_name_colored = match (is_current_branch, branch_status.is_descendent) {
        (true, true) => branch.name.truecolor(142, 192, 124).bold(),
        (true, false) => branch.name.red().bold(),
        (false, true) => branch.name.truecolor(142, 192, 124),
        (false, false) => branch.name.red(),
    };

    // Get diff stats from LKG ancestor to current branch
    let diff_stats = if let Some(lkg_parent) = branch.lkg_parent.as_ref() {
        match git_repo.diff_stats(lkg_parent, &branch_status.sha) {
            Ok((adds, dels)) => format!(
                " {} {}",
                format!("+{}", adds).green(),
                format!("-{}", dels).red()
            ),
            Err(_) => String::new(), // Silently skip on error
        }
    } else {
        String::new() // No LKG = no stats (e.g., trunk root)
    };

    if verbose {
        println!(
            "{}{} ({}) {}{}{}{}",
            branch_name_colored,
            diff_stats,
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
    } else {
        // Format PR info for display
        let pr_info = if let Some(cache) = pr_cache {
            if let Some(pr) = cache.get(&branch.name) {
                let state = pr.display_state();
                let state_colored = match state {
                    github::PrDisplayState::Draft => format!("[{}]", state).truecolor(128, 128, 128),
                    github::PrDisplayState::Open => format!("[{}]", state).truecolor(142, 192, 124),
                    github::PrDisplayState::Merged => format!("[{}]", state).truecolor(180, 142, 173),
                    github::PrDisplayState::Closed => format!("[{}]", state).truecolor(204, 36, 29),
                };
                format!("  #{} {}", pr.number, state_colored)
            } else {
                String::new()
            }
        } else {
            String::new()
        };
        println!("{}{}{}", branch_name_colored, diff_stats, pr_info);
    }

    let mut branches_sorted = branch.branches.iter().collect::<Vec<_>>();
    // Pre-compute is_ancestor results to avoid repeated git merge-base calls during sorting
    let ancestor_cache: std::collections::HashMap<&str, bool> = branches_sorted
        .iter()
        .map(|b| {
            (
                b.name.as_str(),
                git_repo.is_ancestor(&b.name, orig_branch).unwrap_or(false),
            )
        })
        .collect();
    branches_sorted.sort_by(|&a, &b| {
        let a_is_ancestor = ancestor_cache
            .get(a.name.as_str())
            .copied()
            .unwrap_or(false);
        let b_is_ancestor = ancestor_cache
            .get(b.name.as_str())
            .copied()
            .unwrap_or(false);
        match (a_is_ancestor, b_is_ancestor) {
            (true, true) => a.name.cmp(&b.name),
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            (false, false) => a.name.cmp(&b.name),
        }
    });
    for child in branches_sorted {
        recur_tree(
            git_repo,
            child,
            depth + 1,
            orig_branch,
            Some(branch.name.as_ref()),
            verbose,
            pr_cache,
        )?;
    }
    Ok(())
}

/// Fetch PR cache from GitHub, returning None on any error (graceful degradation)
fn fetch_pr_cache(
    git_repo: &GitRepo,
) -> Option<std::collections::HashMap<String, github::PullRequest>> {
    // Try to get repo identifier from remote URL
    let repo_id = github::get_repo_identifier(git_repo).ok()?;

    // Try to get GitHub client (may fail if no token configured)
    let client = github::GitHubClient::from_env(&repo_id).ok()?;

    // Try to fetch all open PRs
    match client.list_open_prs(&repo_id) {
        Ok(prs) => Some(prs),
        Err(e) => {
            tracing::debug!("Failed to fetch PR info: {}", e);
            None
        }
    }
}

fn status(
    git_repo: &GitRepo,
    mut state: State,
    repo: &str,
    orig_branch: &str,
    fetch: bool,
    verbose: bool,
) -> Result<()> {
    if fetch {
        git_fetch()?;
    }
    // ensure_trunk creates the tree if it doesn't exist
    let _trunk = state.ensure_trunk(git_repo, repo)?;

    // Auto-cleanup any missing branches before displaying the tree
    state.auto_cleanup_missing_branches(git_repo, repo)?;

    // Try to fetch PR info from GitHub (graceful degradation on failure)
    let pr_cache = fetch_pr_cache(git_repo);

    let tree = state
        .get_tree_mut(repo)
        .expect("tree exists after ensure_trunk");
    recur_tree(
        git_repo,
        tree,
        0,
        orig_branch,
        None,
        verbose,
        pr_cache.as_ref(),
    )?;
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

#[allow(clippy::too_many_arguments)]
fn restack(
    git_repo: &GitRepo,
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
    let plan = state.plan_restack(git_repo, repo, &restack_branch)?;

    tracing::debug!(?plan, "Restacking branches with plan. Checking out main...");
    git_checkout_main(git_repo, None)?;
    for RestackStep { parent, branch } in plan {
        tracing::debug!(
            "Starting branch: {} [pwd={}]",
            restack_branch,
            env::current_dir()?.display()
        );
        let source = git_repo.sha(&branch.name)?;

        if git_repo.is_ancestor(&parent, &branch.name)? {
            tracing::debug!(
                "Branch '{}' is already stacked on '{}'.",
                branch.name,
                parent
            );
            if push
                && !git_repo.shas_match(&format!("{DEFAULT_REMOTE}/{}", branch.name), &branch.name)
            {
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
                    // Check if we can use the fast format-patch/am approach:
                    // requires an LKG parent that is still an ancestor of the branch
                    if let Some(lkg_parent) = branch.lkg_parent.as_deref()
                        && git_repo.is_ancestor(lkg_parent, &source)?
                    {
                        tracing::info!("LKG parent: {}", lkg_parent);
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
                            git_push(git_repo, &branch.name)?;
                        }
                        continue;
                    }

                    // Fall back to regular rebase (no LKG parent, or branch diverged from LKG)
                    tracing::info!("Using `git rebase` for '{}'...", branch.name);
                    run_git(&["checkout", &branch.name])?;
                    let rebased = run_git_status(&["rebase", &parent], None)?.success();

                    if !rebased {
                        eprintln!("{} did not complete automatically.", "Rebase".blue().bold());
                        eprintln!("Run `git mergetool` to resolve conflicts.");
                        eprintln!(
                            "Once you have finished the {}, re-run this script.",
                            "rebase".blue().bold()
                        );
                        std::process::exit(1);
                    }
                    if push {
                        git_push(git_repo, &branch.name)?;
                    }
                    tracing::info!("Rebase completed successfully. Continuing...");
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
    state.refresh_lkgs(git_repo, repo)?;
    Ok(())
}

fn git_push(git_repo: &GitRepo, branch: &str) -> Result<()> {
    if !git_repo.shas_match(&format!("{DEFAULT_REMOTE}/{}", branch), branch) {
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

// ============== GitHub PR Commands ==============

fn handle_pr_command(
    git_repo: &GitRepo,
    state: &mut State,
    repo: &str,
    current_branch: &str,
    action: PrAction,
) -> Result<()> {
    use github::{
        CreatePrRequest,
        GitHubClient,
        get_repo_identifier,
        has_github_token,
        open_in_browser,
        setup_github_token_interactive,
    };

    let repo_id = get_repo_identifier(git_repo)?;

    // Ensure we have auth configured
    if !has_github_token(&repo_id.host) {
        println!("{}", "GitHub authentication required.".yellow());
        setup_github_token_interactive()?;
    }

    let client = GitHubClient::from_env(&repo_id)?;

    match action {
        PrAction::Create {
            branch,
            title,
            body,
            draft,
            web,
        } => {
            let branch_name = branch.unwrap_or_else(|| current_branch.to_string());

            // Ensure branch is in the tree
            state.try_auto_mount(git_repo, repo, &branch_name)?;

            // Get parent branch from git-stack tree
            let parent = state
                .get_parent_branch_of(repo, &branch_name)
                .ok_or_else(|| {
                    anyhow!(
                        "Branch '{}' not found in git-stack tree. Run `git stack mount <parent>` first.",
                        branch_name
                    )
                })?;

            let base_branch = &parent.name;

            // Check if this is the trunk branch (can't create PR for main)
            let trunk = crate::git::git_trunk(git_repo)?;
            if branch_name == trunk.main_branch {
                bail!(
                    "Cannot create a PR for the trunk branch '{}'.",
                    branch_name.yellow()
                );
            }

            // Check if branch exists on remote, push if not
            let remote_ref = format!("{DEFAULT_REMOTE}/{}", branch_name);
            if !git_repo.branch_exists(&remote_ref) {
                println!(
                    "Branch '{}' is not on remote. Pushing...",
                    branch_name.yellow()
                );
                git::run_git(&[
                    "push",
                    "-u",
                    DEFAULT_REMOTE,
                    &format!("{}:{}", branch_name, branch_name),
                ])?;
            }

            // Check if PR already exists
            if let Some(existing_pr) = client.find_pr_for_branch(&repo_id, &branch_name)? {
                println!(
                    "PR #{} already exists for branch '{}': {}",
                    existing_pr.number.to_string().green(),
                    branch_name.yellow(),
                    existing_pr.html_url.blue()
                );

                // Update stored PR number if not already set
                if let Some(branch) = state.get_tree_branch(repo, &branch_name) {
                    if branch.pr_number.is_none() {
                        if let Some(branch) =
                            find_branch_by_name_mut(state.get_tree_mut(repo).unwrap(), &branch_name)
                        {
                            branch.pr_number = Some(existing_pr.number);
                            state.save_state()?;
                        }
                    }
                }

                if web {
                    open_in_browser(&existing_pr.html_url)?;
                }
                return Ok(());
            }

            // Generate title from first commit if not provided
            let title = title.unwrap_or_else(|| {
                // Get commit message of the branch's first unique commit
                let commit_msg = git::run_git(&["log", "--format=%s", "-1", &branch_name])
                    .ok()
                    .and_then(|r| r.output())
                    .unwrap_or_else(|| branch_name.clone());
                commit_msg
            });

            let body = body.unwrap_or_default();

            println!(
                "Creating PR for '{}' with base '{}'...",
                branch_name.yellow(),
                base_branch.green()
            );

            let pr = client.create_pr(
                &repo_id,
                CreatePrRequest {
                    title: &title,
                    body: &body,
                    head: &branch_name,
                    base: base_branch,
                    draft: if draft { Some(true) } else { None },
                },
            )?;

            println!(
                "Created PR #{}: {}",
                pr.number.to_string().green(),
                pr.html_url.blue()
            );

            // Store PR number in state
            if let Some(branch) =
                find_branch_by_name_mut(state.get_tree_mut(repo).unwrap(), &branch_name)
            {
                branch.pr_number = Some(pr.number);
                state.save_state()?;
            }

            if web {
                open_in_browser(&pr.html_url)?;
            }

            Ok(())
        }
        PrAction::View { branch } => {
            let branch_name = branch.unwrap_or_else(|| current_branch.to_string());

            // Check if we have a stored PR number
            let pr_number = state
                .get_tree_branch(repo, &branch_name)
                .and_then(|b| b.pr_number);

            let pr = if let Some(pr_number) = pr_number {
                client.get_pr(&repo_id, pr_number)?
            } else {
                // Try to find PR by branch name
                client
                    .find_pr_for_branch(&repo_id, &branch_name)?
                    .ok_or_else(|| anyhow!("No PR found for branch '{}'", branch_name))?
            };

            println!("Opening PR #{}: {}", pr.number, pr.html_url);
            open_in_browser(&pr.html_url)?;
            Ok(())
        }
    }
}

fn find_branch_by_name_mut<'a>(tree: &'a mut Branch, name: &str) -> Option<&'a mut Branch> {
    if tree.name == name {
        Some(tree)
    } else {
        for child in &mut tree.branches {
            if let Some(found) = find_branch_by_name_mut(child, name) {
                return Some(found);
            }
        }
        None
    }
}

// ============== GitHub Auth Commands ==============

fn handle_auth_command(git_repo: &GitRepo, action: AuthAction) -> Result<()> {
    use github::{
        get_repo_identifier,
        has_github_token,
        save_github_token,
        setup_github_token_interactive,
    };

    match action {
        AuthAction::Login => {
            setup_github_token_interactive()?;
            println!(
                "{}",
                "GitHub authentication configured successfully.".green()
            );
            Ok(())
        }
        AuthAction::Status => {
            // Try to get repo identifier for host-specific check
            let host = get_repo_identifier(git_repo)
                .map(|r| r.host)
                .unwrap_or_else(|_| "github.com".to_string());

            if has_github_token(&host) {
                println!("{}", "GitHub token is configured.".green());
            } else {
                println!("{}", "No GitHub token configured.".yellow());
                println!("Run `git stack auth login` to set up authentication.");
            }
            Ok(())
        }
        AuthAction::Logout => {
            // Remove the config file
            let base_dirs = xdg::BaseDirectories::with_prefix("git-stack");
            if let Ok(config_path) = base_dirs.get_config_file("github.yaml").ok_or(()) {
                if config_path.exists() {
                    std::fs::remove_file(&config_path)?;
                    println!("GitHub token removed from {}", config_path.display());
                } else {
                    println!("No stored GitHub token found.");
                }
            } else {
                println!("No stored GitHub token found.");
            }
            println!(
                "Note: Tokens in environment variables (GITHUB_TOKEN, GH_TOKEN) or git config are not affected."
            );
            Ok(())
        }
    }
}
