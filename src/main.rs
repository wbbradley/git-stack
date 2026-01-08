#![allow(dead_code, unused_imports, unused_variables)]
use std::{env, fs::canonicalize};

use anyhow::{Context, Result, anyhow, bail, ensure};
use clap::{Parser, Subcommand};
use colored::Colorize;
use git::{after_text, get_local_status, git_checkout_main, git_fetch, run_git_status};
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
mod sync;

/// Compute a deterministic RGB color from a string using its hash.
/// Uses MD5 to hash the string, derives a hue from the first two bytes,
/// and converts HSV to RGB with fixed saturation and value for readability.
fn string_to_rgb(s: &str) -> (u8, u8, u8) {
    let hash = md5::compute(s);
    // Use first two bytes to get a hue value (0-360)
    let hue = (u16::from(hash[0]) | (u16::from(hash[1]) << 8)) % 360;
    // Fixed saturation and value for good terminal readability
    let saturation = 0.35;
    let value = 0.75;
    hsv_to_rgb(hue as f32, saturation, value)
}

/// Convert HSV color to RGB.
/// h: hue (0-360), s: saturation (0-1), v: value (0-1)
fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = v - c;

    let (r, g, b) = match h as u32 {
        0..60 => (c, x, 0.0),
        60..120 => (x, c, 0.0),
        120..180 => (0.0, c, x),
        180..240 => (0.0, x, c),
        240..300 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };

    (
        ((r + m) * 255.0) as u8,
        ((g + m) * 255.0) as u8,
        ((b + m) * 255.0) as u8,
    )
}

// Color constants for consistent theming
mod colors {
    pub const GREEN: (u8, u8, u8) = (142, 192, 124);
    pub const RED: (u8, u8, u8) = (204, 36, 29);
    pub const GRAY: (u8, u8, u8) = (128, 128, 128);
    pub const GOLD: (u8, u8, u8) = (215, 153, 33);
    pub const TREE: (u8, u8, u8) = (55, 55, 50);
    pub const YELLOW: (u8, u8, u8) = (250, 189, 47);
    pub const PURPLE: (u8, u8, u8) = (180, 142, 173);
    pub const MUTED: (u8, u8, u8) = (90, 90, 90);
    pub const PR_NUMBER: (u8, u8, u8) = (90, 78, 98);
    pub const PR_ARROW: (u8, u8, u8) = (100, 105, 105);
    pub const UPSTREAM: (u8, u8, u8) = (88, 88, 88);
    pub const STACKED_ON: (u8, u8, u8) = (90, 120, 87);
}

/// Dimming factor for display. Wraps RGB values and applies a multiplier.
#[derive(Clone, Copy)]
struct Dim(f32);

impl Dim {
    fn full() -> Self {
        Dim(1.0)
    }
    fn dimmed() -> Self {
        Dim(0.75)
    }

    /// Apply dimming to RGB values
    fn rgb(&self, r: u8, g: u8, b: u8) -> (u8, u8, u8) {
        (
            (r as f32 * self.0) as u8,
            (g as f32 * self.0) as u8,
            (b as f32 * self.0) as u8,
        )
    }

    /// Apply dimming to a color tuple
    fn apply(&self, color: (u8, u8, u8)) -> (u8, u8, u8) {
        self.rgb(color.0, color.1, color.2)
    }
}

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
    /// Restack your active branch onto its parent branch.
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
        /// Restack all parent branches recursively up to trunk.
        #[arg(long, short = 'a', default_value_t = false)]
        all_parents: bool,
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
    /// Manage caches (PR cache, seen SHAs).
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },
    /// Import branch stack from existing GitHub PRs.
    /// Reconstructs the git-stack tree by walking the PR base chain.
    Import {
        /// Branch to import (defaults to current branch)
        #[arg(long, short)]
        branch: Option<String>,
        /// Import all open PRs for the repo, not just the current branch's chain
        #[arg(long, short)]
        all: bool,
    },
    /// Sync local git-stack state with GitHub PRs.
    /// Default: weak push then weak pull (bidirectional sync).
    Sync {
        /// Push-only mode: sync local changes to GitHub (no pull)
        #[arg(long, conflicts_with = "pull")]
        push: bool,
        /// Pull-only mode: sync GitHub changes to local (no push)
        #[arg(long, conflicts_with = "push")]
        pull: bool,
        /// Show what would be done without making changes
        #[arg(long)]
        dry_run: bool,
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
    /// Sync PR bases to match git-stack parent branches.
    Sync {
        /// Sync all PRs in stack (defaults to current branch only)
        #[arg(long, short)]
        all: bool,
        /// Show what would be done without making changes
        #[arg(long)]
        dry_run: bool,
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

#[derive(Subcommand)]
enum CacheAction {
    /// Clear all caches (PR cache and seen SHAs).
    Clear,
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
            all_parents,
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
                all_parents,
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
        Some(Command::Cache { action }) => {
            handle_cache_command(&git_repo, &mut state, &repo, action)
        }
        Some(Command::Import { branch, all }) => {
            let branch = branch.unwrap_or(current_branch);
            handle_import_command(&git_repo, &mut state, &repo, &branch, all)
        }
        Some(Command::Sync {
            push,
            pull,
            dry_run,
        }) => {
            let options = sync::SyncOptions {
                push_only: push,
                pull_only: pull,
                dry_run,
            };
            sync::sync(&git_repo, &mut state, &repo, options)
        }
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

#[allow(clippy::too_many_arguments)]
fn recur_tree(
    git_repo: &GitRepo,
    branch: &Branch,
    depth: usize,
    orig_branch: &str,
    parent_branch: Option<&str>,
    verbose: bool,
    pr_cache: Option<&std::collections::HashMap<String, github::PullRequest>>,
    display_authors: &[String],
) -> Result<()> {
    // Check if this branch should be dimmed (filtered by display_authors)
    let pr_author = pr_cache
        .and_then(|cache| cache.get(&branch.name))
        .map(|pr| pr.user.login.as_str());
    let is_dimmed = if display_authors.is_empty() {
        false
    } else {
        pr_author.is_some_and(|author| !display_authors.contains(&author.to_string()))
    };
    let dim = if is_dimmed {
        Dim::dimmed()
    } else {
        Dim::full()
    };

    // Check if branch is remote-only (not local)
    let is_remote_only = !git_repo.branch_exists(&branch.name);

    let Ok(branch_status) = git_repo
        .branch_status(parent_branch, &branch.name)
        .with_context(|| {
            format!(
                "attempting to fetch the branch status of {}",
                branch.name.red()
            )
        })
    else {
        // For remote-only branches, the local branch may not exist
        if is_remote_only {
            // Show remote-only branch even without status
            let is_current_branch = branch.name == orig_branch;
            if is_current_branch {
                print!("{} ", selection_marker().bright_purple().bold());
            } else {
                print!("  ");
            }
            // Tree lines stay at full color regardless of dimming
            for _ in 0..depth {
                print!(
                    "{}",
                    "┃ ".truecolor(colors::TREE.0, colors::TREE.1, colors::TREE.2)
                );
            }
            let branch_color = dim.apply(colors::GRAY);
            let muted_color = dim.apply(colors::MUTED);
            println!(
                "{}",
                branch
                    .name
                    .truecolor(branch_color.0, branch_color.1, branch_color.2)
            );

            // Recurse into children
            for child in &branch.branches {
                recur_tree(
                    git_repo,
                    child,
                    depth + 1,
                    orig_branch,
                    Some(branch.name.as_ref()),
                    verbose,
                    pr_cache,
                    display_authors,
                )?;
            }
            return Ok(());
        }
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

    // Tree lines stay at full color regardless of dimming
    for _ in 0..depth {
        print!(
            "{}",
            "┃ ".truecolor(colors::TREE.0, colors::TREE.1, colors::TREE.2)
        );
    }

    // Branch name coloring: green for synced, red for diverged, bold for current branch
    let branch_color = if branch_status.is_descendent {
        dim.apply(colors::GREEN)
    } else {
        dim.apply(colors::YELLOW)
    };
    let branch_name_colored = if is_current_branch {
        branch
            .name
            .truecolor(branch_color.0, branch_color.1, branch_color.2)
            .bold()
    } else {
        branch
            .name
            .truecolor(branch_color.0, branch_color.1, branch_color.2)
    };

    // Get diff stats from LKG ancestor (or merge-base) to current branch
    let diff_stats = {
        // Determine base ref and whether it's reliable
        let (base_ref, is_reliable) = if let Some(lkg) = branch.lkg_parent.as_deref() {
            (Some(lkg.to_string()), true)
        } else if let Ok(merge_base) =
            git_repo.merge_base(&branch_status.parent_branch, &branch_status.sha)
        {
            (Some(merge_base), false) // merge-base is less reliable
        } else {
            (None, false)
        };

        match base_ref {
            Some(base) => match git_repo.diff_stats(&base, &branch_status.sha) {
                Ok((adds, dels)) => {
                    let green = dim.apply(colors::GREEN);
                    let red = dim.apply(colors::RED);
                    let prefix = if is_reliable { "" } else { "~ " };
                    format!(
                        " [{}{}{}]",
                        prefix,
                        format!("+{}", adds).truecolor(green.0, green.1, green.2),
                        format!(" -{}", dels).truecolor(red.0, red.1, red.2)
                    )
                }
                Err(_) => String::new(),
            },
            None => String::new(),
        }
    };

    // Get local changes summary for current branch only
    let local_status = if is_current_branch {
        match get_local_status() {
            Ok(status) if !status.is_clean() => {
                let mut parts = Vec::new();
                let green = dim.apply(colors::GREEN);
                let yellow = dim.apply(colors::YELLOW);
                let gray = dim.apply(colors::GRAY);
                if status.staged > 0 {
                    parts.push(
                        format!("+{}", status.staged)
                            .truecolor(green.0, green.1, green.2)
                            .to_string(),
                    );
                }
                if status.unstaged > 0 {
                    parts.push(
                        format!("~{}", status.unstaged)
                            .truecolor(yellow.0, yellow.1, yellow.2)
                            .to_string(),
                    );
                }
                if status.untracked > 0 {
                    parts.push(
                        format!("?{}", status.untracked)
                            .truecolor(gray.0, gray.1, gray.2)
                            .to_string(),
                    );
                }
                format!(" [{}]", parts.join(" "))
            }
            _ => String::new(),
        }
    } else {
        String::new()
    };

    if verbose {
        let gold = dim.apply(colors::GOLD);
        let stacked_on = dim.apply(colors::STACKED_ON);
        let yellow = dim.apply(colors::YELLOW);
        let red = dim.apply(colors::RED);
        let green = dim.apply(colors::GREEN);
        let upstream_color = dim.apply(colors::UPSTREAM);

        println!(
            "{}{}{} ({}) {}{}{}{}",
            branch_name_colored,
            diff_stats,
            local_status,
            branch_status.sha[..8].truecolor(gold.0, gold.1, gold.2),
            {
                let details: String = if branch_status.exists {
                    if branch_status.is_descendent {
                        format!(
                            "{} {}",
                            "is stacked on".truecolor(stacked_on.0, stacked_on.1, stacked_on.2),
                            branch_status
                                .parent_branch
                                .truecolor(yellow.0, yellow.1, yellow.2)
                        )
                    } else {
                        format!(
                            "{} {}",
                            "diverges from".truecolor(red.0, red.1, red.2),
                            branch_status
                                .parent_branch
                                .truecolor(yellow.0, yellow.1, yellow.2)
                        )
                    }
                } else {
                    "does not exist!".truecolor(red.0, red.1, red.2).to_string()
                };
                details
            },
            {
                if let Some(upstream_status) = branch_status.upstream_status {
                    format!(
                        " (upstream {} is {})",
                        upstream_status.symbolic_name.truecolor(
                            upstream_color.0,
                            upstream_color.1,
                            upstream_color.2
                        ),
                        if upstream_status.synced {
                            "synced".truecolor(green.0, green.1, green.2)
                        } else {
                            "not synced".truecolor(red.0, red.1, red.2)
                        }
                    )
                } else {
                    format!(" ({})", "no upstream".truecolor(gold.0, gold.1, gold.2))
                }
            },
            {
                if let Some(lkg_parent) = branch.lkg_parent.as_ref() {
                    format!(
                        " (lkg parent {})",
                        lkg_parent[..8].truecolor(gold.0, gold.1, gold.2)
                    )
                } else {
                    String::new()
                }
            },
            {
                let method_color = dim.apply(colors::GREEN);
                match branch.stack_method {
                    StackMethod::ApplyMerge => {
                        " (apply-merge)".truecolor(method_color.0, method_color.1, method_color.2)
                    }
                    StackMethod::Merge => {
                        " (merge)".truecolor(method_color.0, method_color.1, method_color.2)
                    }
                }
            },
        );
        if let Some(note) = &branch.note {
            print!("  ");
            // Tree lines stay at full color regardless of dimming
            for _ in 0..depth {
                print!(
                    "{}",
                    "┃ ".truecolor(colors::TREE.0, colors::TREE.1, colors::TREE.2)
                );
            }

            let first_line = note.lines().next().unwrap_or("");
            // Note: keeping blue for notes as it's distinct from status colors
            println!(
                "  {} {}",
                "›".truecolor(colors::TREE.0, colors::TREE.1, colors::TREE.2),
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
                let gray = dim.apply(colors::GRAY);
                let green = dim.apply(colors::GREEN);
                let purple = dim.apply(colors::PURPLE);
                let red = dim.apply(colors::RED);
                let state_colored = match state {
                    github::PrDisplayState::Draft => {
                        format!("[{}]", state).truecolor(gray.0, gray.1, gray.2)
                    }
                    github::PrDisplayState::Open => {
                        format!("[{}]", state).truecolor(green.0, green.1, green.2)
                    }
                    github::PrDisplayState::Merged => {
                        format!("[{}]", state).truecolor(purple.0, purple.1, purple.2)
                    }
                    github::PrDisplayState::Closed => {
                        format!("[{}]", state).truecolor(red.0, red.1, red.2)
                    }
                };
                let author_rgb = string_to_rgb(&pr.user.login);
                let author_color = dim.apply(author_rgb);
                let author_colored = format!("@{}", pr.user.login).truecolor(
                    author_color.0,
                    author_color.1,
                    author_color.2,
                );
                let pr_num = dim.apply(colors::PR_NUMBER);
                let number_colored =
                    format!("#{}", pr.number).truecolor(pr_num.0, pr_num.1, pr_num.2);
                let arrow = dim.apply(colors::PR_ARROW);
                format!(
                    " {} {} {} {}",
                    "".truecolor(arrow.0, arrow.1, arrow.2),
                    author_colored,
                    number_colored,
                    state_colored
                )
            } else {
                String::new()
            }
        } else {
            String::new()
        };
        println!(
            "{}{}{}{}",
            branch_name_colored, diff_stats, local_status, pr_info
        );
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
            display_authors,
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
    match client.list_open_prs(&repo_id, None) {
        Ok(result) => Some(result.prs),
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

    // Load display_authors for filtering (show other authors dimmed)
    let display_authors = github::load_display_authors();

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
        &display_authors,
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
    all_parents: bool,
) -> Result<(), anyhow::Error> {
    let restack_branch = restack_branch.unwrap_or(orig_branch.clone());

    if fetch {
        git_fetch()?;
    }

    // Ensure target branch exists locally (check it out from remote if needed)
    if !git_repo.branch_exists(&restack_branch) {
        let remote_ref = format!("{DEFAULT_REMOTE}/{restack_branch}");
        if git_repo.ref_exists(&remote_ref) {
            run_git(&["checkout", "-b", &restack_branch, &remote_ref])?;
            println!(
                "Created local branch {} from remote.",
                restack_branch.yellow()
            );
        } else {
            bail!(
                "Branch {} does not exist locally or on remote.",
                restack_branch
            );
        }
    }

    // Find starting_branch in the stacks of branches to determine which stack to use.
    let plan = state.plan_restack(git_repo, repo, &restack_branch, all_parents)?;

    tracing::debug!(?plan, "Restacking branches with plan. Checking out main...");
    git_checkout_main(git_repo, None)?;

    // Track pushed branches to record SHAs after the loop (avoids borrow issues with plan)
    let mut pushed_branches: Vec<String> = Vec::new();

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
                println!("Pushing branch '{}' to remote...", branch.name);
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
                pushed_branches.push(branch.name.clone());
            }
        } else {
            tracing::info!("Branch '{}' is not stacked on '{}'...", branch.name, parent);

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
                            pushed_branches.push(branch.name.clone());
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
                        pushed_branches.push(branch.name.clone());
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

    // Record pushed SHAs as seen on remote (for safe branch deletion)
    for branch_name in pushed_branches {
        if let Ok(sha) = git_repo.sha(&branch_name) {
            state.add_seen_sha(repo, sha);
        }
    }

    tracing::debug!("Restoring starting branch '{}'...", restack_branch);
    ensure!(
        run_git_status(&["checkout", "-q", &orig_branch], None)?.success(),
        "git checkout {} failed",
        restack_branch
    );
    println!("Done.");
    state.refresh_lkgs(git_repo, repo)?;

    // Note: PR sync is now handled separately via `git stack sync`
    // Run `git stack sync` after `restack -p` to sync PR bases

    Ok(())
}

/// Sync PR bases to match git-stack parents after restack (graceful degradation)
/// Uses a bottom-up traversal (leaves first) so each parent is processed once.
fn sync_pr_bases_after_restack(git_repo: &GitRepo, state: &State, repo: &str) -> Result<()> {
    use github::{GitHubClient, UpdatePrRequest, get_repo_identifier};

    let repo_id = get_repo_identifier(git_repo)?;
    let client = GitHubClient::from_env(&repo_id)?;

    // Get the tree
    let tree = state
        .get_tree(repo)
        .ok_or_else(|| anyhow!("No stack tree found"))?;

    let trunk = crate::git::git_trunk(git_repo)?;

    // Collect branches with depth for bottom-up processing
    let branches_with_depth = collect_branches_with_depth(tree, &tree.name, 0);

    // Sort by depth descending (leaves first) for bottom-up processing
    let mut sorted_branches = branches_with_depth;
    sorted_branches.sort_by(|a, b| b.2.cmp(&a.2));

    // Track which parents we've already processed/created
    let mut processed_parents: std::collections::HashSet<String> = std::collections::HashSet::new();
    processed_parents.insert(trunk.main_branch.clone()); // Trunk is always "processed"

    // Fetch all open PRs once
    let mut all_prs = client.list_open_prs(&repo_id, None)?.prs;

    for (branch_name, expected_base, _depth) in sorted_branches {
        // Skip trunk
        if branch_name == trunk.main_branch {
            continue;
        }

        // First, ensure parent is ready (if not trunk and not already processed)
        if expected_base != trunk.main_branch && !processed_parents.contains(&expected_base) {
            ensure_branch_pr(
                git_repo,
                &client,
                &repo_id,
                &mut all_prs,
                &expected_base,
                &trunk.main_branch,
                state,
                repo,
                false, // Don't push - if not on remote, likely merged
            )?;
            processed_parents.insert(expected_base.clone());
        }

        // Now sync this branch's PR
        if let Some(pr) = all_prs.get(&branch_name)
            && pr.base.ref_name != expected_base
        {
            println!(
                "Retargeting PR #{} for '{}': {} → {}",
                pr.number.to_string().green(),
                branch_name.yellow(),
                pr.base.ref_name.red(),
                expected_base.green()
            );

            client.update_pr(
                &repo_id,
                pr.number,
                UpdatePrRequest {
                    base: Some(&expected_base),
                    title: None,
                    body: None,
                },
            )?;
        }
    }

    Ok(())
}

/// Ensure a branch has a PR, optionally pushing if not on remote.
/// - `push_if_missing`: if true, push the branch if not on remote; if false, warn about likely merge
#[allow(clippy::too_many_arguments)]
fn ensure_branch_pr(
    git_repo: &GitRepo,
    client: &github::GitHubClient,
    repo_id: &github::RepoIdentifier,
    all_prs: &mut std::collections::HashMap<String, github::PullRequest>,
    branch_name: &str,
    trunk: &str,
    state: &State,
    repo: &str,
    push_if_missing: bool,
) -> Result<()> {
    use github::CreatePrRequest;

    // Check if already has an open PR
    if all_prs.contains_key(branch_name) {
        return Ok(());
    }

    // Check if branch exists on remote
    let remote_ref = format!("{DEFAULT_REMOTE}/{}", branch_name);
    if !git_repo.ref_exists(&remote_ref) {
        if push_if_missing {
            // Push the branch
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
        } else {
            // Branch doesn't exist - likely merged
            println!(
                "{} Branch '{}' no longer exists on remote (likely merged).",
                "Note:".cyan().bold(),
                branch_name.yellow()
            );
            println!(
                "  Run `git stack unmount {}` to update your stack tree.",
                branch_name
            );
            return Ok(());
        }
    }

    // Check if there's an existing PR we didn't see
    if let Some(pr) = client.find_pr_for_branch(repo_id, branch_name)? {
        all_prs.insert(branch_name.to_string(), pr);
        return Ok(());
    }

    // No PR - create one
    let parent = state
        .get_parent_branch_of(repo, branch_name)
        .map(|b| b.name.clone())
        .unwrap_or_else(|| trunk.to_string());

    println!(
        "Creating PR for '{}' with base '{}'...",
        branch_name.yellow(),
        parent.green()
    );

    // Generate title from first commit
    let title = git::run_git(&[
        "log",
        "--no-show-signature",
        "--format=%s",
        "-1",
        branch_name,
    ])
    .ok()
    .and_then(|r| r.output())
    .unwrap_or_else(|| branch_name.to_string());

    let pr = client.create_pr(
        repo_id,
        CreatePrRequest {
            title: &title,
            body: "",
            head: branch_name,
            base: &parent,
            draft: Some(true),
        },
    )?;

    println!(
        "Created PR #{} for '{}': {}",
        pr.number.to_string().green(),
        branch_name.yellow(),
        pr.html_url.blue()
    );

    all_prs.insert(branch_name.to_string(), pr);
    Ok(())
}

/// Collect branches with their parent and depth for bottom-up processing
fn collect_branches_with_depth(
    branch: &Branch,
    parent_name: &str,
    depth: usize,
) -> Vec<(String, String, usize)> {
    let mut result = Vec::new();

    // Add this branch (unless it's the root/trunk)
    if branch.name != parent_name {
        result.push((branch.name.clone(), parent_name.to_string(), depth));
    }

    // Recurse into children
    for child in &branch.branches {
        result.extend(collect_branches_with_depth(child, &branch.name, depth + 1));
    }

    result
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

// ============== GitHub Import Command ==============

fn handle_import_command(
    git_repo: &GitRepo,
    state: &mut State,
    repo: &str,
    branch: &str,
    import_all: bool,
) -> Result<()> {
    use github::{
        GitHubClient,
        get_repo_identifier,
        has_github_token,
        setup_github_token_interactive,
    };

    let repo_id = get_repo_identifier(git_repo)?;

    // Ensure we have auth configured
    if !has_github_token(&repo_id.host) {
        println!("{}", "GitHub authentication required.".yellow());
        setup_github_token_interactive()?;
    }

    let client = GitHubClient::from_env(&repo_id)?;
    let trunk = crate::git::git_trunk(git_repo)?;

    // Ensure trunk exists in tree
    state.ensure_trunk(git_repo, repo)?;

    if import_all {
        // Import all open PRs
        import_all_prs(git_repo, &client, &repo_id, state, repo, &trunk.main_branch)?;
    } else {
        // Import just this branch's chain
        import_branch_chain(
            git_repo,
            &client,
            &repo_id,
            state,
            repo,
            branch,
            &trunk.main_branch,
        )?;
    }

    state.save_state()?;
    println!("\n{}", "Import complete!".green().bold());

    // Show the tree
    println!();
    let tree = state.get_tree(repo).expect("tree exists after import");
    let pr_cache = fetch_pr_cache(git_repo);
    let display_authors = github::load_display_authors();
    recur_tree(
        git_repo,
        tree,
        0,
        branch,
        None,
        false,
        pr_cache.as_ref(),
        &display_authors,
    )?;

    Ok(())
}

/// Import a single branch's PR chain back to trunk
fn import_branch_chain(
    git_repo: &GitRepo,
    client: &github::GitHubClient,
    repo_id: &github::RepoIdentifier,
    state: &mut State,
    repo: &str,
    branch: &str,
    trunk: &str,
) -> Result<()> {
    // Find PR for this branch
    let pr = client.find_pr_for_branch(repo_id, branch)?.ok_or_else(|| {
        anyhow!(
            "No open PR found for branch '{}'. Nothing to import.",
            branch
        )
    })?;

    println!(
        "Found PR #{} for '{}' (base: '{}')",
        pr.number.to_string().green(),
        branch.yellow(),
        pr.base.ref_name.cyan()
    );

    // Build the chain from this branch up to trunk
    let mut chain: Vec<(String, Option<github::PullRequest>)> =
        vec![(branch.to_string(), Some(pr.clone()))];
    let mut current_base = pr.base.ref_name.clone();

    while current_base != trunk {
        // Check if we already have this branch in our chain (cycle detection)
        if chain.iter().any(|(b, _)| b == &current_base) {
            println!(
                "{} Detected cycle at '{}', stopping chain walk",
                "Warning:".yellow().bold(),
                current_base
            );
            break;
        }

        // Find PR for the base branch
        match client.find_pr_for_branch(repo_id, &current_base)? {
            Some(base_pr) => {
                println!(
                    "Found PR #{} for '{}' (base: '{}')",
                    base_pr.number.to_string().green(),
                    current_base.yellow(),
                    base_pr.base.ref_name.cyan()
                );
                let next_base = base_pr.base.ref_name.clone();
                chain.push((current_base.clone(), Some(base_pr)));
                current_base = next_base;
            }
            None => {
                // No PR for this base - it might be an intermediate branch or already merged
                // Check if it's the trunk
                if current_base == trunk {
                    break;
                }
                println!(
                    "{} No PR found for '{}', assuming it's an intermediate branch",
                    "Note:".cyan().bold(),
                    current_base.yellow()
                );
                chain.push((current_base.clone(), None));
                // We can't walk further without a PR, assume parent is trunk
                break;
            }
        }
    }

    // Reverse to process from trunk-side down
    chain.reverse();

    // Mount each branch in order
    let mut parent = trunk.to_string();
    for (branch_name, pr_opt) in chain {
        // Check if branch exists locally
        if !git_repo.branch_exists(&branch_name) {
            println!(
                "{} Branch '{}' doesn't exist locally, skipping",
                "Note:".cyan().bold(),
                branch_name.yellow()
            );
            continue;
        }

        // Mount this branch under the parent
        if !state.branch_exists_in_tree(repo, &branch_name) {
            println!(
                "Mounting '{}' on '{}'",
                branch_name.yellow(),
                parent.green()
            );
            state.mount(git_repo, repo, &branch_name, Some(parent.clone()))?;
        } else {
            println!("'{}' already in tree", branch_name.yellow());
        }

        // Store PR number if we have one
        if let Some(pr) = pr_opt
            && let Some(tree_branch) =
                find_branch_by_name_mut(state.get_tree_mut(repo).unwrap(), &branch_name)
            && tree_branch.pr_number.is_none()
        {
            tree_branch.pr_number = Some(pr.number);
        }

        parent = branch_name;
    }

    Ok(())
}

/// Import all open PRs for the repo
fn import_all_prs(
    git_repo: &GitRepo,
    client: &github::GitHubClient,
    repo_id: &github::RepoIdentifier,
    state: &mut State,
    repo: &str,
    trunk: &str,
) -> Result<()> {
    let all_prs = client.list_open_prs(repo_id, None)?.prs;

    if all_prs.is_empty() {
        println!("No open PRs found for this repository.");
        return Ok(());
    }

    println!("Found {} open PR(s)", all_prs.len().to_string().green());

    // Build a map of branch -> (parent, pr_number)
    let mut branch_parents: std::collections::HashMap<String, (String, u64)> =
        std::collections::HashMap::new();

    for (branch_name, pr) in &all_prs {
        branch_parents.insert(branch_name.clone(), (pr.base.ref_name.clone(), pr.number));
    }

    // Find branches whose parent is trunk or another PR branch (roots of stacks)
    // Then build each stack
    let mut imported = std::collections::HashSet::new();

    // Process in order: first branches based on trunk, then their children, etc.
    let mut to_process: Vec<String> = branch_parents
        .iter()
        .filter(|(_, (parent, _))| parent == trunk)
        .map(|(branch, _)| branch.clone())
        .collect();

    while !to_process.is_empty() {
        let branch_name = to_process.remove(0);

        if imported.contains(&branch_name) {
            continue;
        }

        // Get parent and PR number
        let (parent, pr_number) = match branch_parents.get(&branch_name) {
            Some((p, n)) => (p.clone(), *n),
            None => continue,
        };

        // Determine actual parent (could be trunk or another imported branch)
        let actual_parent = if parent == trunk || imported.contains(&parent) {
            parent.clone()
        } else {
            // Parent not yet imported - skip for now, will process later
            to_process.push(branch_name);
            continue;
        };

        // Check if branch exists locally
        if !git_repo.branch_exists(&branch_name) {
            println!(
                "{} Branch '{}' doesn't exist locally (PR #{}), skipping",
                "Note:".cyan().bold(),
                branch_name.yellow(),
                pr_number
            );
            imported.insert(branch_name.clone());
            continue;
        }

        // Mount this branch
        if !state.branch_exists_in_tree(repo, &branch_name) {
            println!(
                "Mounting '{}' on '{}' (PR #{})",
                branch_name.yellow(),
                actual_parent.green(),
                pr_number
            );
            state.mount(git_repo, repo, &branch_name, Some(actual_parent.clone()))?;
        }

        // Store PR number
        if let Some(tree_branch) =
            find_branch_by_name_mut(state.get_tree_mut(repo).unwrap(), &branch_name)
            && tree_branch.pr_number.is_none()
        {
            tree_branch.pr_number = Some(pr_number);
        }

        imported.insert(branch_name.clone());

        // Add children of this branch to process list
        for (child, (child_parent, _)) in &branch_parents {
            if child_parent == &branch_name && !imported.contains(child) {
                to_process.push(child.clone());
            }
        }
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

            // Check if this is the trunk branch (can't create PR for main)
            let trunk = crate::git::git_trunk(git_repo)?;
            if branch_name == trunk.main_branch {
                bail!(
                    "Cannot create a PR for the trunk branch '{}'.",
                    branch_name.yellow()
                );
            }

            // Collect ancestor chain from branch up to trunk (for recursive PR creation)
            let mut ancestor_chain = Vec::new();
            let mut current = branch_name.clone();
            while let Some(parent) = state.get_parent_branch_of(repo, &current) {
                if parent.name == trunk.main_branch {
                    break;
                }
                ancestor_chain.push(parent.name.clone());
                current = parent.name.clone();
            }
            // Reverse to process from trunk-side down
            ancestor_chain.reverse();

            // Fetch all open PRs once for efficiency
            let mut all_prs = client.list_open_prs(&repo_id, None)?.prs;

            // Ensure all ancestors have PRs (recursive creation)
            for ancestor in &ancestor_chain {
                ensure_branch_pr(
                    git_repo,
                    &client,
                    &repo_id,
                    &mut all_prs,
                    ancestor,
                    &trunk.main_branch,
                    state,
                    repo,
                    true, // Push if not on remote
                )?;
            }

            // Get parent branch from git-stack tree
            let parent = state
                .get_parent_branch_of(repo, &branch_name)
                .ok_or_else(|| {
                    anyhow!(
                        "Branch '{}' not found in git-stack tree. Run `git stack mount <parent>` first.",
                        branch_name
                    )
                })?;

            let base_branch = parent.name.clone();

            // Check if branch exists on remote, push if not
            let remote_ref = format!("{DEFAULT_REMOTE}/{}", branch_name);
            if !git_repo.ref_exists(&remote_ref) {
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
            if let Some(existing_pr) = all_prs
                .get(&branch_name)
                .or(client.find_pr_for_branch(&repo_id, &branch_name)?.as_ref())
            {
                println!(
                    "PR #{} already exists for branch '{}': {}",
                    existing_pr.number.to_string().green(),
                    branch_name.yellow(),
                    existing_pr.html_url.blue()
                );

                // Update stored PR number if not already set
                if let Some(branch) = state.get_tree_branch(repo, &branch_name)
                    && branch.pr_number.is_none()
                    && let Some(branch) =
                        find_branch_by_name_mut(state.get_tree_mut(repo).unwrap(), &branch_name)
                {
                    branch.pr_number = Some(existing_pr.number);
                    state.save_state()?;
                }

                if web {
                    open_in_browser(&existing_pr.html_url)?;
                }
                return Ok(());
            }

            // Generate title from first commit if not provided
            let title = title.unwrap_or_else(|| {
                // Get commit message of the branch's first unique commit
                git::run_git(&[
                    "log",
                    "--no-show-signature",
                    "--format=%s",
                    "-1",
                    &branch_name,
                ])
                .ok()
                .and_then(|r| r.output())
                .unwrap_or_else(|| branch_name.clone())
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
                    base: &base_branch,
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
        PrAction::Sync { all, dry_run } => {
            use github::UpdatePrRequest;

            let trunk = crate::git::git_trunk(git_repo)?;

            // Get branches to sync with depth for bottom-up processing
            let branches_to_sync: Vec<(String, String, usize)> = if all {
                let tree = state
                    .get_tree(repo)
                    .ok_or_else(|| anyhow!("No stack tree found for repo"))?;
                collect_branches_with_depth(tree, &tree.name, 0)
            } else {
                let parent = state
                    .get_parent_branch_of(repo, current_branch)
                    .ok_or_else(|| {
                        anyhow!("Branch '{}' not found in git-stack tree", current_branch)
                    })?;
                vec![(current_branch.to_string(), parent.name.clone(), 1)]
            };

            // Sort by depth descending (leaves first) for bottom-up processing
            let mut sorted_branches = branches_to_sync;
            sorted_branches.sort_by(|a, b| b.2.cmp(&a.2));

            // Track which parents we've already processed/created
            let mut processed_parents: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            processed_parents.insert(trunk.main_branch.clone());

            // Fetch all open PRs once
            let mut all_prs = client.list_open_prs(&repo_id, None)?.prs;

            let mut synced_count = 0;
            let mut created_count = 0;
            let mut skipped_count = 0;

            for (branch_name, expected_base, _depth) in sorted_branches {
                if branch_name == trunk.main_branch {
                    continue;
                }

                // First, ensure parent is ready (if not trunk and not already processed)
                if expected_base != trunk.main_branch && !processed_parents.contains(&expected_base)
                {
                    if dry_run {
                        if !all_prs.contains_key(&expected_base) {
                            let remote_ref = format!("{DEFAULT_REMOTE}/{}", expected_base);
                            if git_repo.branch_exists(&remote_ref) {
                                println!(
                                    "[dry-run] Would create PR for parent '{}'",
                                    expected_base.yellow()
                                );
                                created_count += 1;
                            }
                        }
                    } else {
                        let before_count = all_prs.len();
                        ensure_branch_pr(
                            git_repo,
                            &client,
                            &repo_id,
                            &mut all_prs,
                            &expected_base,
                            &trunk.main_branch,
                            state,
                            repo,
                            false, // Don't push - if not on remote, likely merged
                        )?;
                        if all_prs.len() > before_count {
                            created_count += 1;
                        }
                    }
                    processed_parents.insert(expected_base.clone());
                }

                // Now sync this branch's PR
                let pr = match all_prs.get(&branch_name) {
                    Some(pr) => pr,
                    None => {
                        tracing::debug!("No PR found for branch '{}'", branch_name);
                        continue;
                    }
                };

                if pr.base.ref_name == expected_base {
                    skipped_count += 1;
                    continue;
                }

                // Base mismatch - need to retarget
                if dry_run {
                    println!(
                        "[dry-run] Would retarget PR #{} for '{}': {} → {}",
                        pr.number.to_string().green(),
                        branch_name.yellow(),
                        pr.base.ref_name.red(),
                        expected_base.green()
                    );
                } else {
                    println!(
                        "Retargeting PR #{} for '{}': {} → {}",
                        pr.number.to_string().green(),
                        branch_name.yellow(),
                        pr.base.ref_name.red(),
                        expected_base.green()
                    );

                    client.update_pr(
                        &repo_id,
                        pr.number,
                        UpdatePrRequest {
                            base: Some(&expected_base),
                            title: None,
                            body: None,
                        },
                    )?;
                }
                synced_count += 1;
            }

            // Summary
            let prefix = if dry_run { "[dry-run] " } else { "" };
            if created_count > 0 || synced_count > 0 {
                println!(
                    "\n{}Created {} PR(s), synced {} PR(s), {} already correct",
                    prefix, created_count, synced_count, skipped_count
                );
            } else {
                println!("{}All PRs already have correct bases", prefix);
            }

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

// ============== Cache Commands ==============

fn handle_cache_command(
    git_repo: &GitRepo,
    state: &mut State,
    repo: &str,
    action: CacheAction,
) -> Result<()> {
    match action {
        CacheAction::Clear => {
            // Clear PR cache for this repo
            let repo_id = github::get_repo_identifier(git_repo)?;
            let repo_full_name = format!("{}/{}", repo_id.owner, repo_id.repo);
            github::clear_pr_cache(&repo_full_name)?;
            println!("Cleared PR cache for {}.", repo_full_name);

            // Clear seen SHAs for current repo
            let count = state.get_seen_shas(repo).map(|s| s.len()).unwrap_or(0);
            state.clear_seen_shas(repo);
            state.save_state()?;
            println!("Cleared {} seen SHAs.", count);

            Ok(())
        }
    }
}
