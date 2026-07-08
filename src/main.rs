#![allow(dead_code, unused_imports, unused_variables)]
use std::{env, fs::canonicalize};

use anyhow::{Context, Result, anyhow, bail, ensure};
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::{Shell, generate};
use colored::Colorize;
use git::{after_text, git_checkout_main, git_fetch, git_trunk, run_git_status};
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
mod llms;
mod lock;
mod merge_base_cache;
mod pr_cache;
mod render;
mod state;
mod stats;
mod sync;
mod tui;
#[derive(Parser)]
#[command(author, version, about, infer_subcommands = true)]
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

    #[arg(
        long,
        global = true,
        help = "Show all branches, bypassing display_authors-based hiding"
    )]
    show_all: bool,

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
    /// Launch interactive TUI mode for branch navigation and checkout.
    Interactive,
    /// Move up the stack to the parent branch.
    Up,
    /// Move down the stack to a child branch (only if there's exactly one child).
    Down,
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
        /// Recursively restack all ancestors from trunk up to this branch.
        #[arg(long, short = 'a', default_value_t = false)]
        ancestors: bool,
        /// Squash all commits in the branch into a single commit.
        #[arg(long, short = 's', default_value_t = false)]
        squash: bool,
        /// Continue a previously interrupted squash operation after conflict resolution.
        #[arg(long, default_value_t = false)]
        r#continue: bool,
        /// Abort an in-progress squash operation and restore the original branch state.
        #[arg(long, default_value_t = false)]
        abort: bool,
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
        #[arg(long, short = 'n', default_value_t = false)]
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
    /// Generate shell completions.
    Completions {
        /// Shell to generate completions for.
        #[arg(value_enum)]
        shell: Shell,
    },
    /// Print an exhaustive markdown reference for LLM/agent consumers.
    Llms(llms::LlmsArgs),
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
        #[arg(long, short = 'n')]
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
        #[arg(long, short = 'n')]
        dry_run: bool,
    },
}

#[derive(Subcommand)]
enum AuthAction {
    /// Set up GitHub authentication interactively.
    Login {
        /// Force the paste-a-token flow (skip the browser OAuth menu).
        #[arg(long)]
        pat: bool,
    },
    /// Show current auth status.
    Status,
    /// Remove stored authentication.
    Logout {
        /// Clear only the OAuth token.
        #[arg(long)]
        oauth: bool,
        /// Clear only the PAT (default_token).
        #[arg(long)]
        pat: bool,
    },
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
                .with_line_number(true),
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

    // Handle completions early (doesn't require git repo)
    if let Some(Command::Completions { shell }) = args.command {
        let mut cmd = Args::command();
        generate(shell, &mut cmd, "git-stack", &mut std::io::stdout());
        return Ok(());
    }

    // Handle llms early (doesn't require git repo)
    if let Some(Command::Llms(a)) = args.command {
        return llms::run(a);
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

    // Check for pending squash operation - block other commands except --continue and --abort
    if state.has_pending_squash(&repo) {
        match &args.command {
            Some(Command::Restack {
                r#continue: true, ..
            }) => { /* allowed */ }
            Some(Command::Restack { abort: true, .. }) => { /* allowed */ }
            _ => {
                bail!(
                    "A squash operation is in progress for this repository.\n\
                     Run `git stack restack --continue` after resolving conflicts,\n\
                     or `git stack restack --abort` to cancel."
                );
            }
        }
    }

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
            ancestors,
            squash,
            r#continue,
            abort,
        }) => {
            // Handle --continue first
            if r#continue {
                return handle_squash_continue(&git_repo, &mut state, &repo);
            }
            // Handle --abort
            if abort {
                return handle_squash_abort(&git_repo, &mut state, &repo);
            }
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
                ancestors,
                squash,
            )
        }
        Some(Command::Mount { parent_branch }) => {
            state.mount(&git_repo, &repo, &current_branch, parent_branch.clone())?;

            // If this branch has a PR, retarget its base to the new parent
            let effective_parent =
                parent_branch.or_else(|| git::git_trunk(&git_repo).map(|t| t.main_branch));
            if let Some(parent) = effective_parent
                && let Some(branch) = state.get_tree_branch(&repo, &current_branch)
                && let Some(pr_number) = branch.pr_number
                && let Ok(repo_id) = github::get_repo_identifier(&git_repo)
                && let Ok(client) = github::GitHubClient::from_env(&repo_id)
            {
                match client.update_pr(
                    &repo_id,
                    pr_number,
                    github::UpdatePrRequest {
                        base: Some(&parent),
                        title: None,
                        body: None,
                    },
                ) {
                    Ok(_) => {
                        println!("Retargeted PR #{} base to '{}'.", pr_number, parent);
                    }
                    Err(e) => {
                        tracing::warn!("Failed to retarget PR #{pr_number}: {e}");
                    }
                }
            }
            Ok(())
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
                args.show_all,
            )
        }
        Some(Command::Interactive) => {
            state.try_auto_mount(&git_repo, &repo, &current_branch)?;
            interactive(
                &git_repo,
                state,
                &repo,
                &current_branch,
                args.verbose,
                args.show_all,
            )
        }
        Some(Command::Up) => {
            state.try_auto_mount(&git_repo, &repo, &current_branch)?;
            navigate_up(&state, &repo, &current_branch)
        }
        Some(Command::Down) => {
            state.try_auto_mount(&git_repo, &repo, &current_branch)?;
            navigate_down(&state, &repo, &current_branch)
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
        Some(Command::Completions { .. }) => unreachable!("handled above"),
        Some(Command::Llms(_)) => unreachable!("handled above"),
        None => {
            state.try_auto_mount(&git_repo, &repo, &current_branch)?;
            status(
                &git_repo,
                state,
                &repo,
                &current_branch,
                false,
                args.verbose,
                args.show_all,
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

/// Fetch open PRs from GitHub, returning None on any error (graceful degradation). Carries
/// `all_authors` (branch -> author, including fork PRs filtered out of `.prs`) alongside the
/// render-ready `.prs` map so `add_closed_pr_authors` doesn't need a second open-PR fetch.
fn fetch_pr_cache(git_repo: &GitRepo) -> Option<github::PrListResult> {
    // Try to get repo identifier from remote URL
    let repo_id = github::get_repo_identifier(git_repo).ok()?;

    // Try to get GitHub client (may fail if no token configured)
    let client = github::GitHubClient::from_env(&repo_id).ok()?;

    // Try to fetch all open PRs
    match client.list_open_prs(&repo_id, None) {
        Ok(result) => Some(result),
        Err(e) => {
            tracing::debug!("Failed to fetch PR info: {}", e);
            None
        }
    }
}

/// Extend an open-PR author lookup (from `fetch_pr_cache`'s `all_authors`) with authors of
/// closed/merged PRs, for display_authors-based hiding. Open-only author data can't tell
/// whether a branch with no open PR was never turned into one (must stay visible) from one
/// whose PR was merged or closed by someone else (should be hidden) — this fills that gap.
/// Closed PRs are fetched through the same on-disk watermark cache `git stack sync` uses, so
/// only the very first call after enabling `display_authors` pays the full historical fetch
/// cost; subsequent calls only page until the watermark. Returns `authors` unchanged on any
/// error (graceful degradation — hides nothing beyond what the input already allows).
fn add_closed_pr_authors(
    git_repo: &GitRepo,
    mut authors: std::collections::HashMap<String, String>,
) -> std::collections::HashMap<String, String> {
    let Ok(repo_id) = github::get_repo_identifier(git_repo) else {
        return authors;
    };
    let Ok(client) = github::GitHubClient::from_env(&repo_id) else {
        return authors;
    };

    let Ok(cache) = crate::pr_cache::PrCacheHandle::open() else {
        return authors;
    };
    if let Ok(result) = client.list_closed_prs_with_cache(&repo_id, &cache, None) {
        authors.extend(result.all_authors);
    }

    authors
}

/// Resolve authors for branches that still have no PR-based entry in `authors`, via GitHub's
/// commit-author API on each branch's tip commit. A PR-based lookup alone misses two real
/// cases: branches that never had a PR at all, and branches whose open PR lives under a
/// different (e.g. renamed) head ref than the tracked branch name — both looked
/// indistinguishable from "this is my own unpublished WIP" before this fallback existed, so they
/// stayed visible even when they belonged to someone else. Resolving by commit author fixes
/// both: your own unpublished commits resolve to your own login (so local WIP stays protected),
/// while another author's stray/renamed branch resolves to them and gets hidden. A commit that
/// GitHub can't attribute to any account (unpushed, or a non-GitHub-verified email) leaves
/// `authors` unchanged for that branch, which keeps it protected — "can't tell" still means
/// "don't hide". Mutates `authors` in place; does one API call per still-unresolved branch.
fn add_commit_authors(
    git_repo: &GitRepo,
    tree: &Branch,
    authors: &mut std::collections::HashMap<String, String>,
) {
    let Ok(repo_id) = github::get_repo_identifier(git_repo) else {
        return;
    };
    let Ok(client) = github::GitHubClient::from_env(&repo_id) else {
        return;
    };

    let mut unresolved = Vec::new();
    collect_branches_without_author(tree, authors, &mut unresolved);

    for branch_name in unresolved {
        let sha = git_repo.sha(&branch_name).ok().or_else(|| {
            git_repo
                .sha(&format!("{DEFAULT_REMOTE}/{branch_name}"))
                .ok()
        });
        let Some(sha) = sha else { continue };
        if let Ok(Some(login)) = client.get_commit_author(&repo_id, &sha) {
            authors.insert(branch_name, login);
        }
    }
}

/// Collect names of `tree`'s descendants (not `tree` itself — the trunk is always protected, so
/// its author is never consulted) that have no entry in `authors` yet.
fn collect_branches_without_author(
    tree: &Branch,
    authors: &std::collections::HashMap<String, String>,
    unresolved: &mut Vec<String>,
) {
    for child in &tree.branches {
        if !authors.contains_key(&child.name) {
            unresolved.push(child.name.clone());
        }
        collect_branches_without_author(child, authors, unresolved);
    }
}

/// Builds the renderable tree shared by `status()`/`interactive()`: fetches PR data from GitHub
/// and computes local git status/diff/sort, overlapping the two when hiding/dimming don't
/// require the fetch to finish first (i.e. whenever display_authors filtering isn't active).
fn build_renderable_tree(
    git_repo: &GitRepo,
    repo: &str,
    tree: &Branch,
    orig_branch: &str,
    verbose: bool,
    show_all: bool,
    display_authors: &[String],
) -> render::RenderableTree {
    let hiding_active = !show_all && !display_authors.is_empty();

    let (mut renderable, pr_cache) = if hiding_active {
        // pr_authors depends on the fetch result plus further network calls below, so there's
        // no local work left to overlap the fetch with in this mode.
        let pr_result = fetch_pr_cache(git_repo);
        let open_authors = pr_result
            .as_ref()
            .map(|r| r.all_authors.clone())
            .unwrap_or_default();
        let mut pr_authors = add_closed_pr_authors(git_repo, open_authors);
        add_commit_authors(git_repo, tree, &mut pr_authors);

        let renderable = render::compute_renderable_tree(
            git_repo,
            tree,
            orig_branch,
            verbose,
            display_authors,
            &pr_authors,
            show_all,
        );
        (renderable, pr_result.map(|r| r.prs))
    } else {
        // No author filter active, so hiding/dimming/sort need only an empty pr_authors map —
        // the local git walk has nothing to wait on. Overlap it with the PR fetch.
        let repo_path = repo.to_string();
        let pr_authors = std::collections::HashMap::new();

        let (renderable, pr_result, fetch_stats) = std::thread::scope(|scope| {
            let fetch_handle = scope.spawn(move || {
                // Own GitRepo handle: git2::Repository is Send but not Sync, and the main
                // thread is using `git_repo` concurrently below.
                let result = GitRepo::open(&repo_path)
                    .ok()
                    .and_then(|fresh_repo| fetch_pr_cache(&fresh_repo));
                (result, crate::stats::get_stats())
            });

            let renderable = render::compute_renderable_tree(
                git_repo,
                tree,
                orig_branch,
                verbose,
                display_authors,
                &pr_authors,
                show_all,
            );

            let (pr_result, fetch_stats) = fetch_handle
                .join()
                .unwrap_or_else(|_| (None, crate::stats::GitStats::default()));
            (renderable, pr_result, fetch_stats)
        });
        crate::stats::merge_into_current(&fetch_stats);
        (renderable, pr_result.map(|r| r.prs))
    };

    render::apply_pr_cache(&mut renderable, pr_cache.as_ref());
    renderable
}

fn status(
    git_repo: &GitRepo,
    mut state: State,
    repo: &str,
    orig_branch: &str,
    fetch: bool,
    verbose: bool,
    show_all: bool,
) -> Result<()> {
    if fetch {
        git_fetch()?;
    }
    // ensure_trunk creates the tree if it doesn't exist (no-op if no remote)
    let _trunk = state.ensure_trunk(git_repo, repo);

    // Auto-cleanup any missing branches before displaying the tree
    state.auto_cleanup_missing_branches(git_repo, repo)?;

    // Load display_authors for filtering (hides branches whose PR author isn't listed)
    let display_authors = github::load_display_authors();

    let Some(tree) = state.get_tree(repo) else {
        println!("No stack configured for this repository.");
        return Ok(());
    };

    let renderable = build_renderable_tree(
        git_repo,
        repo,
        tree,
        orig_branch,
        verbose,
        show_all,
        &display_authors,
    );

    // Render to CLI
    render::render_cli(&renderable, verbose);

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

fn interactive(
    git_repo: &GitRepo,
    mut state: State,
    repo: &str,
    orig_branch: &str,
    verbose: bool,
    show_all: bool,
) -> Result<()> {
    // ensure_trunk creates the tree if it doesn't exist (no-op if no remote)
    let _trunk = state.ensure_trunk(git_repo, repo);

    // Auto-cleanup any missing branches before displaying the tree
    state.auto_cleanup_missing_branches(git_repo, repo)?;

    // Load display_authors for filtering (hides branches whose PR author isn't listed)
    let display_authors = github::load_display_authors();

    let Some(tree) = state.get_tree(repo) else {
        println!("No stack configured for this repository.");
        return Ok(());
    };

    let renderable = build_renderable_tree(
        git_repo,
        repo,
        tree,
        orig_branch,
        verbose,
        show_all,
        &display_authors,
    );

    // Run TUI and handle checkout if user selected a branch
    if let Some(branch_to_checkout) = tui::run_tui(renderable, verbose)? {
        run_git(&["checkout", &branch_to_checkout])?;
    }

    state.save_state()?;
    Ok(())
}

/// Navigate up the stack to the parent branch.
fn navigate_up(state: &State, repo: &str, current_branch: &str) -> Result<()> {
    let parent = state.get_parent_branch_of(repo, current_branch);

    match parent {
        Some(parent_branch) => {
            run_git(&["checkout", &parent_branch.name])?;
            Ok(())
        }
        None => {
            bail!(
                "Branch '{}' has no parent in the stack (already at root).",
                current_branch.yellow()
            );
        }
    }
}

/// Navigate down the stack to a child branch.
fn navigate_down(state: &State, repo: &str, current_branch: &str) -> Result<()> {
    let branch = state.get_tree_branch(repo, current_branch);

    match branch {
        Some(branch) => {
            let children = &branch.branches;
            match children.len() {
                0 => {
                    bail!(
                        "Branch '{}' has no children in the stack.",
                        current_branch.yellow()
                    );
                }
                1 => {
                    run_git(&["checkout", &children[0].name])?;
                    Ok(())
                }
                n => {
                    bail!(
                        "Branch '{}' has {} children. Use `git stack interactive` to choose.",
                        current_branch.yellow(),
                        n
                    );
                }
            }
        }
        None => {
            bail!(
                "Branch '{}' not found in the stack.",
                current_branch.yellow()
            );
        }
    }
}

/// Get concatenated commit messages between ancestor and branch tip.
fn get_concatenated_commit_messages(branch: &str, ancestor: &str) -> Result<String> {
    let output = run_git(&[
        "log",
        "--reverse",
        "--format=%B",
        &format!("{}..{}", ancestor, branch),
    ])?;

    let messages = output.output().unwrap_or_default();
    if messages.trim().is_empty() {
        bail!("No commits found between {} and {}", ancestor, branch);
    }

    // Clean up the messages - join with separator for readability
    let cleaned: String = messages
        .split("\n\n")
        .filter(|s| !s.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");

    Ok(cleaned)
}

/// Complete a squash operation (either after clean merge or after conflict resolution).
fn complete_squash(
    git_repo: &GitRepo,
    state: &mut State,
    repo: &str,
    pending: &state::PendingSquashOperation,
) -> Result<()> {
    // Commit with the concatenated messages
    run_git(&["commit", "-m", &pending.squash_message])?;

    // Move the branch pointer: git checkout -B <branch>
    // This points <branch> to current HEAD (the squashed commit) and checks it out
    run_git(&["checkout", "-B", &pending.branch_name])?;

    // Clean up temp branch
    let _ = run_git(&["branch", "-D", &pending.tmp_branch_name]);

    // Clear the pending operation
    state.set_pending_squash(repo, None);
    state.save_state()?;

    println!(
        "Squash completed for branch '{}'.",
        pending.branch_name.yellow()
    );

    Ok(())
}

/// Execute a squash operation for a single branch.
fn squash_branch(
    git_repo: &GitRepo,
    state: &mut State,
    repo: &str,
    branch: &state::Branch,
    parent: &str,
) -> Result<bool> {
    let branch_name = &branch.name;
    let tmp_branch = format!("tmp-{}", branch_name);

    // Determine ancestor for commit message range
    let source_sha = git_repo.sha(branch_name)?;
    let lkg_ancestor = branch
        .lkg_parent
        .as_deref()
        .filter(|lkg| git_repo.is_ancestor(lkg, &source_sha).unwrap_or(false))
        .map(|s| s.to_string())
        .or_else(|| git_repo.merge_base(parent, branch_name).ok())
        .ok_or_else(|| anyhow!("Cannot determine ancestor for commit messages"))?;

    // Collect commit messages from ancestor to branch tip
    let squash_message = get_concatenated_commit_messages(branch_name, &lkg_ancestor)?;

    // Save original SHA for recovery
    let original_sha = git_repo.sha(branch_name)?;

    // Create pending operation state
    let pending = state::PendingSquashOperation {
        branch_name: branch_name.clone(),
        parent_branch: parent.to_string(),
        tmp_branch_name: tmp_branch.clone(),
        original_sha,
        squash_message: squash_message.clone(),
    };
    state.set_pending_squash(repo, Some(pending.clone()));
    state.save_state()?;

    // Execute the squash workflow
    // git checkout <parent>
    run_git(&["checkout", parent])?;

    // git checkout -B tmp-<branch>
    run_git(&["checkout", "-B", &tmp_branch])?;

    // git merge --squash <branch>
    let merge_status = run_git_status(&["merge", "--squash", branch_name], None)?;

    if !merge_status.success() {
        // Conflict! Print instructions and exit
        eprintln!("{}", "Merge conflict during squash operation.".red().bold());
        eprintln!();
        eprintln!("Resolve the conflicts, then run:");
        eprintln!("  git add <resolved-files>");
        eprintln!("  {}", "git stack restack --continue".green().bold());
        eprintln!();
        eprintln!("Or to abort and restore the original branch:");
        eprintln!("  {}", "git stack restack --abort".yellow().bold());
        std::process::exit(1);
    }

    // Complete the squash (no conflict)
    complete_squash(git_repo, state, repo, &pending)?;

    Ok(true)
}

/// Handle the --continue flag for resuming a squash operation.
fn handle_squash_continue(git_repo: &GitRepo, state: &mut State, repo: &str) -> Result<()> {
    let pending = state
        .get_pending_squash(repo)
        .ok_or_else(|| anyhow!("No pending squash operation to continue."))?
        .clone();

    // Check if there are unresolved conflicts
    let status_output = run_git(&["status", "--porcelain"])?;
    let has_conflicts = status_output
        .output()
        .map(|s| {
            s.lines()
                .any(|line| line.starts_with("UU") || line.starts_with("AA"))
        })
        .unwrap_or(false);

    if has_conflicts {
        bail!(
            "There are still unresolved conflicts. Resolve them and add the files, \
             then run --continue again."
        );
    }

    // Check if we're on the temp branch
    let current = git_repo.current_branch()?;
    if current != pending.tmp_branch_name {
        bail!(
            "Expected to be on branch '{}' but currently on '{}'. \
             Please checkout '{}' and run --continue again.",
            pending.tmp_branch_name,
            current,
            pending.tmp_branch_name
        );
    }

    // Complete the squash
    complete_squash(git_repo, state, repo, &pending)?;

    Ok(())
}

/// Handle the --abort flag for aborting a squash operation.
fn handle_squash_abort(git_repo: &GitRepo, state: &mut State, repo: &str) -> Result<()> {
    let pending = state
        .get_pending_squash(repo)
        .ok_or_else(|| anyhow!("No pending squash operation to abort."))?
        .clone();

    // Abort any in-progress merge
    let _ = run_git_status(&["merge", "--abort"], None);

    // Checkout the original branch at its original SHA
    run_git(&["checkout", "-f", &pending.original_sha])?;
    run_git(&["checkout", "-B", &pending.branch_name])?;

    // Clean up temp branch if it exists
    let _ = run_git(&["branch", "-D", &pending.tmp_branch_name]);

    // Clear pending state
    state.set_pending_squash(repo, None);
    state.save_state()?;

    println!(
        "Squash operation aborted. Branch '{}' restored to original state.",
        pending.branch_name.yellow()
    );

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
    ancestors: bool,
    squash: bool,
) -> Result<(), anyhow::Error> {
    // Hold a repo-scoped advisory lock for the whole restack so a second
    // git-stack invocation can't race us on ref updates (e.g. the fetch below,
    // or the checkout/branch updates that follow).
    let _lock = git_repo.lock()?;

    let restack_branch = restack_branch.unwrap_or(orig_branch.clone());

    // Track what changes occurred during restack (branch_name, status)
    let mut branch_results: Vec<(String, String)> = Vec::new();

    if fetch {
        git_fetch()?;
    }

    // Check if user is trying to restack the trunk branch
    let trunk = git_trunk(git_repo).ok_or_else(|| anyhow!("No remote configured"))?;
    if restack_branch == trunk.main_branch {
        println!(
            "You are on the trunk branch ({}). Nothing to restack.",
            trunk.main_branch.yellow()
        );
        return Ok(());
    }

    // Ensure target branch exists locally (check it out from remote if needed)
    if !git_repo.branch_exists(&restack_branch) {
        let remote_ref = format!("{DEFAULT_REMOTE}/{restack_branch}");
        if git_repo.ref_exists(&remote_ref) {
            run_git(&["checkout", "-b", &restack_branch, &remote_ref])?;
            branch_results.push((restack_branch.clone(), "created".to_string()));
        } else {
            bail!(
                "Branch {} does not exist locally or on remote.",
                restack_branch
            );
        }
    }

    // Find starting_branch in the stacks of branches to determine which stack to use.
    let plan = state.plan_restack(git_repo, repo, &restack_branch, ancestors)?;

    // Collect plan into owned data to allow mutable access to state during the loop
    let plan_owned: Vec<(String, state::Branch)> = plan
        .into_iter()
        .map(|step| (step.parent, step.branch.clone()))
        .collect();

    tracing::debug!("Restacking branches with plan. Checking out main...");
    git_checkout_main(git_repo, None)?;

    // Track pushed branches to record SHAs after the loop (avoids borrow issues with plan)
    let mut pushed_branches: Vec<String> = Vec::new();

    for (parent, branch) in plan_owned {
        // Ensure the branch exists locally (check it out from remote if needed)
        if !git_repo.branch_exists(&branch.name) {
            let remote_ref = format!("{DEFAULT_REMOTE}/{}", branch.name);
            if git_repo.ref_exists(&remote_ref) {
                run_git(&["checkout", "-b", &branch.name, &remote_ref])?;
                branch_results.push((branch.name.clone(), "created".to_string()));
            }
            // If remote doesn't exist either, let the subsequent operations fail
            // with a clear error message
        }

        tracing::debug!(
            "Starting branch: {} [pwd={}]",
            restack_branch,
            env::current_dir()?.display()
        );
        let source = git_repo.sha(&branch.name)?;

        // Handle squash mode - squash all commits into one on top of parent
        if squash {
            squash_branch(git_repo, &mut state, repo, &branch, &parent)?;
            let status = if push {
                git_push(git_repo, &branch.name)?;
                pushed_branches.push(branch.name.clone());
                "squashed, pushed"
            } else {
                "squashed"
            };
            branch_results.push((branch.name.clone(), status.to_string()));
            continue;
        }

        if git_repo.is_ancestor(&parent, &branch.name)? {
            tracing::debug!(
                "Branch '{}' is already stacked on '{}'.",
                branch.name,
                parent
            );
            let mut status = "no changes".to_string();
            if push
                && !git_repo.shas_match(&format!("{DEFAULT_REMOTE}/{}", branch.name), &branch.name)
            {
                let refspec = format!("{branch_name}:{branch_name}", branch_name = branch.name);
                let mut args = vec!["push", "-u"];
                if matches!(branch.stack_method, StackMethod::ApplyMerge) {
                    tracing::debug!(
                        "Force-pushing (with lease) '{}' to {DEFAULT_REMOTE}...",
                        branch.name
                    );
                    args.push("--force-with-lease");
                }
                args.push(DEFAULT_REMOTE);
                args.push(&refspec);
                run_git(&args)?;
                pushed_branches.push(branch.name.clone());
                status = "no changes, pushed".to_string();
            }
            branch_results.push((branch.name.clone(), status));
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
                        println!("Applying patch...");
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
                        let status = if push {
                            git_push(git_repo, &branch.name)?;
                            pushed_branches.push(branch.name.clone());
                            "restacked, pushed"
                        } else {
                            "restacked"
                        };
                        branch_results.push((branch.name.clone(), status.to_string()));
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
                    let status = if push {
                        git_push(git_repo, &branch.name)?;
                        pushed_branches.push(branch.name.clone());
                        "restacked, pushed"
                    } else {
                        "restacked"
                    };
                    branch_results.push((branch.name.clone(), status.to_string()));
                    tracing::info!("Rebase completed successfully. Continuing...");
                }
                StackMethod::Merge => {
                    run_git(&["checkout", &branch.name])
                        .with_context(|| format!("checking out {}", branch.name))?;
                    run_git(&["merge", &parent])
                        .with_context(|| format!("merging {parent} into {}", branch.name))?;
                    branch_results.push((branch.name.clone(), "restacked".to_string()));
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

    // Print summary report
    if branch_results.is_empty() {
        println!("No branches to restack.");
    } else {
        for (branch, status) in &branch_results {
            println!("{}: {}", branch.yellow(), status);
        }
    }

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

    let trunk = crate::git::git_trunk(git_repo).ok_or_else(|| anyhow!("No remote configured"))?;

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
            "-u",
            "--force-with-lease",
            DEFAULT_REMOTE,
            &format!("{}:{}", branch, branch),
        ])?;
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
        CreatePrRequest, GitHubClient, get_repo_identifier, has_github_token, login_interactive,
        open_in_browser,
    };

    let repo_id = get_repo_identifier(git_repo)?;

    // Ensure we have auth configured
    if !has_github_token(&repo_id.host) {
        println!(
            "{}",
            "GitHub authentication required (needs the 'repo' scope).".yellow()
        );
        login_interactive()?;
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
            let trunk =
                crate::git::git_trunk(git_repo).ok_or_else(|| anyhow!("No remote configured"))?;
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

            let trunk =
                crate::git::git_trunk(git_repo).ok_or_else(|| anyhow!("No remote configured"))?;

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
        AuthSource, clear_github_tokens, find_auth_source, get_repo_identifier, login_interactive,
        setup_github_token_interactive,
    };

    match action {
        AuthAction::Login { pat } => {
            if pat {
                setup_github_token_interactive()?;
            } else {
                login_interactive()?;
            }
            println!(
                "{}",
                "GitHub authentication configured successfully.".green()
            );
            println!(
                "Tip: if you already run `gh auth login`, git-stack will borrow gh's token automatically — no separate setup needed."
            );
            Ok(())
        }
        AuthAction::Status => {
            // Try to get repo identifier for host-specific check
            let host = get_repo_identifier(git_repo)
                .map(|r| r.host)
                .unwrap_or_else(|_| "github.com".to_string());

            match find_auth_source(&host) {
                Some(source) => {
                    let desc = match &source {
                        AuthSource::EnvGithubToken => {
                            "GITHUB_TOKEN environment variable".to_string()
                        }
                        AuthSource::EnvGhToken => "GH_TOKEN environment variable".to_string(),
                        AuthSource::GitConfig => "git config (github.token)".to_string(),
                        AuthSource::ConfigHostToken => {
                            "config file (host-specific token)".to_string()
                        }
                        AuthSource::ConfigDefaultToken => {
                            "config file (personal access token)".to_string()
                        }
                        AuthSource::ConfigOauth { scope } => match scope {
                            Some(s) => format!("config file (OAuth, scope: {s})"),
                            None => "config file (OAuth)".to_string(),
                        },
                        AuthSource::GhCli => {
                            format!("gh CLI (delegated credentials for {host})")
                        }
                    };
                    println!(
                        "{} Active method: {}.",
                        "GitHub token is configured.".green(),
                        desc
                    );
                    // For borrowed gh credentials, show gh's own status summary
                    // rather than implying git-stack owns the token.
                    if source == AuthSource::GhCli {
                        let _ = std::process::Command::new("gh")
                            .args(["auth", "status"])
                            .status();
                    }
                }
                None => {
                    println!("{}", "No GitHub token configured.".yellow());
                    println!("Run `git stack auth login` to set up authentication.");
                }
            }
            Ok(())
        }
        AuthAction::Logout { oauth, pat } => {
            let host = get_repo_identifier(git_repo)
                .map(|r| r.host)
                .unwrap_or_else(|_| "github.com".to_string());
            clear_github_tokens(oauth, pat)?;
            println!(
                "Note: Tokens in environment variables (GITHUB_TOKEN, GH_TOKEN) or git config are not affected."
            );
            if find_auth_source(&host) == Some(AuthSource::GhCli) {
                println!(
                    "Note: git-stack is borrowing credentials from the gh CLI, which it does not manage. Run `gh auth logout` to sign out of gh."
                );
            }
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
            crate::pr_cache::clear_pr_cache(&repo_full_name)?;
            println!("Cleared PR cache for {}.", repo_full_name);

            // Clear merge-base / is-ancestor cache for this repo (scoped locally, so it works
            // even for repos with no GitHub remote).
            git_repo.clear_merge_base_cache()?;
            println!("Cleared merge-base cache for {}.", repo);

            // Clear seen SHAs for current repo
            let count = state.get_seen_shas(repo).map(|s| s.len()).unwrap_or(0);
            state.clear_seen_shas(repo);
            state.save_state()?;
            println!("Cleared {} seen SHAs.", count);

            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn branch(name: &str, branches: Vec<Branch>) -> Branch {
        Branch {
            name: name.to_string(),
            stack_method: StackMethod::ApplyMerge,
            note: None,
            lkg_parent: None,
            pr_number: None,
            branches,
        }
    }

    #[test]
    fn collect_branches_without_author_skips_the_root_and_known_authors() {
        // main (root, always excluded)
        // ├─ alice-1 (known author)
        // │  └─ mystery-1 (no author yet)
        // └─ mystery-2 (no author yet)
        let tree = branch(
            "main",
            vec![
                branch("alice-1", vec![branch("mystery-1", vec![])]),
                branch("mystery-2", vec![]),
            ],
        );
        let mut authors = std::collections::HashMap::new();
        authors.insert("alice-1".to_string(), "alice".to_string());

        let mut unresolved = Vec::new();
        collect_branches_without_author(&tree, &authors, &mut unresolved);

        unresolved.sort();
        assert_eq!(
            unresolved,
            vec!["mystery-1".to_string(), "mystery-2".to_string()]
        );
    }

    #[test]
    fn collect_branches_without_author_returns_nothing_when_all_resolved() {
        let tree = branch("main", vec![branch("alice-1", vec![])]);
        let mut authors = std::collections::HashMap::new();
        authors.insert("alice-1".to_string(), "alice".to_string());

        let mut unresolved = Vec::new();
        collect_branches_without_author(&tree, &authors, &mut unresolved);

        assert!(unresolved.is_empty());
    }
}
