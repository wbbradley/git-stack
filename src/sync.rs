//! Unified sync module for bidirectional state synchronization.
//!
//! This module implements a Terraform-style staged workflow:
//! 1. Read: Gather current local and remote state
//! 2. Model: Build target state in memory
//! 3. Diff: Compute changes needed for each side
//! 4. Validate: Ensure changes are non-lossy
//! 5. Apply: Execute changes if safe

use std::{
    collections::{HashMap, HashSet},
    io::IsTerminal,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use colored::Colorize;
use indicatif::{ProgressBar, ProgressStyle};
use rand::seq::SliceRandom;

use crate::{
    git::{fetch_with_recovery, git_trunk, run_git},
    git2_ops::{DEFAULT_REMOTE, GitRepo},
    github::{
        GitHubClient, PrState, PullRequest, RepoIdentifier, UpdatePrRequest, get_repo_identifier,
    },
    state::{Branch, State},
};

// ============== Stage 1: State Types ==============

/// Local state gathered from git-stack tree and git refs
#[derive(Debug)]
pub struct LocalState {
    /// Map of branch name -> local branch info
    pub branches: HashMap<String, LocalBranch>,
    /// The trunk/main branch name
    pub trunk: String,
}

/// Information about a single local branch
#[derive(Debug, Clone)]
pub struct LocalBranch {
    /// Parent branch in git-stack tree (None for trunk)
    pub parent: Option<String>,
    /// Cached PR number from git-stack state
    pub pr_number: Option<u64>,
    /// Whether the branch has been pushed to remote
    pub pushed_to_remote: bool,
}

/// Remote state gathered from GitHub API
#[derive(Debug)]
pub struct RemoteState {
    /// Map of head branch name -> PR info (open PRs)
    pub prs: HashMap<String, RemotePr>,
    /// Map of head branch name -> PR info (closed/merged PRs)
    pub closed_prs: HashMap<String, RemotePr>,
    /// Map of head branch name -> open PR author login (for `authors_filter` gating)
    pub authors: HashMap<String, String>,
}

/// Information about a single remote PR
#[derive(Debug, Clone)]
pub struct RemotePr {
    pub number: u64,
    pub base: String,
    pub state: RemotePrState,
    pub title: String,
    pub html_url: String,
}

impl From<&PullRequest> for RemotePr {
    fn from(pr: &PullRequest) -> Self {
        Self {
            number: pr.number,
            base: pr.base.ref_name.clone(),
            state: if pr.is_merged() {
                RemotePrState::Merged
            } else if pr.state == PrState::Closed {
                RemotePrState::Closed
            } else if pr.draft {
                RemotePrState::Draft
            } else {
                RemotePrState::Open
            },
            title: pr.title.clone(),
            html_url: pr.html_url.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemotePrState {
    Draft,
    Open,
    Merged,
    Closed,
}

// ============== Stage 2: Target State ==============

/// Target state after merging local and remote
#[derive(Debug)]
pub struct TargetState {
    /// Map of branch name -> target branch info
    pub branches: HashMap<String, TargetBranch>,
    /// The trunk/main branch name
    pub trunk: String,
}

/// Target state for a single branch
#[derive(Debug, Clone)]
pub struct TargetBranch {
    /// Parent branch (from local git-stack - authoritative)
    pub parent: Option<String>,
    /// PR number (from remote if exists, else local cache)
    pub pr_number: Option<u64>,
    /// Expected PR base (should match parent branch name)
    pub expected_pr_base: Option<String>,
    /// Whether branch is pushed to remote
    pub pushed_to_remote: bool,
}

// ============== Stage 3: Change Types ==============

/// Reason for deleting a local branch
#[derive(Debug, Clone)]
pub enum DeleteReason {
    /// SHA was seen on remote, safe to delete (squash/rebase merges)
    SeenOnRemote { verified_sha: String },
    /// Branch is fully merged into main (git branch --merged)
    MergedIntoMain,
    /// Local branch is ancestor of origin/<branch> (all work pushed)
    AncestorOfRemote,
}

/// Changes to apply to local state
#[derive(Debug, Clone)]
pub enum LocalChange {
    /// Mount a branch under a parent in the git-stack tree
    MountBranch { name: String, parent: String },
    /// Unmount a branch (PR was merged/closed)
    UnmountBranch {
        name: String,
        repoint_children_to: String,
    },
    /// Update the cached PR number for a branch
    UpdatePrNumber { branch: String, pr_number: u64 },
    /// Delete a local branch that has been merged
    DeleteLocalBranch { name: String, reason: DeleteReason },
}

/// Changes to apply to remote state (GitHub)
#[derive(Debug, Clone)]
pub enum RemoteChange {
    /// Retarget a PR to a different base
    RetargetPr {
        number: u64,
        branch: String,
        old_base: String,
        new_base: String,
    },
    /// Push a branch to remote (used before retargeting to it)
    PushBranch { branch: String },
}

// ============== Stage 4: Sync Plan ==============

/// Complete sync plan with all changes
#[derive(Debug)]
pub struct SyncPlan {
    pub local_changes: Vec<LocalChange>,
    pub remote_changes: Vec<RemoteChange>,
    pub warnings: Vec<String>,
    /// Branches that will be unmounted (for checkout logic if user is on one)
    pub branches_to_unmount: Vec<String>,
    /// Branches safe to delete locally (work preserved on remote)
    pub branches_to_delete: Vec<String>,
}

impl SyncPlan {
    pub fn is_empty(&self) -> bool {
        self.local_changes.is_empty() && self.remote_changes.is_empty()
    }

    pub fn has_remote_changes(&self) -> bool {
        !self.remote_changes.is_empty()
    }
}

// ============== Sync Options ==============

#[derive(Debug, Clone, Copy, Default)]
pub struct SyncOptions {
    /// Only push local changes to remote (no pull)
    pub push_only: bool,
    /// Only pull remote changes to local (no push)
    pub pull_only: bool,
    /// Show plan without applying
    pub dry_run: bool,
}

// ============== Implementation ==============

/// Main sync entry point
pub fn sync(git_repo: &GitRepo, state: &mut State, repo: &str, options: SyncOptions) -> Result<()> {
    // Hold a repo-scoped advisory lock for the whole sync so a second git-stack
    // invocation can't race us on ref updates (e.g. concurrent fetch --prune).
    let _lock = git_repo.lock()?;

    // Get repo identifier for GitHub API
    let repo_id = get_repo_identifier(git_repo)?;
    let client = GitHubClient::from_env(&repo_id)?;

    // Fetch with prune to ensure remote tracking refs are up-to-date
    println!("Fetching from remote...");
    fetch_with_recovery(&["fetch", "--tags", "-f", "--prune", DEFAULT_REMOTE])?;

    // Stage 1: Read current state
    println!("Reading local state...");
    let local_state = read_local_state(git_repo, state, repo)?;

    // Scope the open-PR fetch + target injection to the user's stack (local tree, plus a
    // reconstructed base chain on a fresh clone). Gate remote-only injection by authors_filter.
    let current_branch = git_repo.current_branch().unwrap_or_default();
    // sync is always online with a live client, so refresh the identity cache here (an unset
    // filter resolves to your own login; explicit config passes through).
    let authors_filter = crate::github::resolve_effective_authors_filter(&repo_id, Some(&client))?;
    let scope_vec = compute_scope_branches(
        &client,
        &repo_id,
        &local_state,
        &current_branch,
        !options.push_only,
    );
    let mut scope: HashSet<String> = scope_vec.iter().cloned().collect();

    // Author-based open-PR discovery: seed scope with the user's own open PRs so sync mounts
    // them even from a trunk-only tree. Additive; skipped under --push and empty filter.
    // Best-effort — a failure never aborts sync.
    let discovered_prs: Vec<PullRequest> = if !options.push_only && !authors_filter.is_empty() {
        match client.list_open_prs_by_authors(&repo_id, &authors_filter) {
            Ok(prs) => prs,
            Err(e) => {
                tracing::warn!(
                    "Author-based PR discovery failed; continuing with stack scope: {e}"
                );
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };

    println!("Reading remote state...");
    let (mut remote_state, mut seen_shas) = read_remote_state(&client, &repo_id, &scope_vec)?;
    merge_discovered_prs(
        &discovered_prs,
        &mut scope,
        &mut remote_state,
        &mut seen_shas,
    );

    // Record PR head SHAs as seen (filtering to match GC criteria to avoid re-adding garbage)
    let origin_trunk = format!("{}/{}", DEFAULT_REMOTE, local_state.trunk);
    let existing_shas = state.get_seen_shas(repo).cloned().unwrap_or_default();
    let tracked_shas: Vec<String> = state
        .get_tree(repo)
        .map(|tree| collect_tracked_branch_shas(git_repo, tree))
        .unwrap_or_default();

    let mut skipped_existing = 0;
    let mut added = 0;
    let total = seen_shas.len();

    // A PR-head SHA belongs in the seen set exactly when it is reachable from one of our tracked
    // branch tips but is not already merged into trunk. The old implementation computed that per
    // SHA with is_ancestor, walking the commit graph (or triggering a refresh-on-miss ODB lookup
    // for the many SHAs never fetched into this clone) tens of thousands of times on every sync —
    // dominating sync time on a large repo. Invert it: one bounded revwalk over just the stack's
    // own commits (push tracked tips, hide origin/trunk) yields precisely that set, after which
    // membership is an O(1) lookup per SHA. Cost now scales with the stack size, not the repo's
    // closed-PR count. SHAs absent from this clone simply never appear in the walk.
    let reachable = git_repo
        .commits_reachable_excluding(&tracked_shas, &origin_trunk)
        .unwrap_or_else(|e| {
            tracing::warn!("Failed to compute reachable seen SHAs, adding none this run: {e:#}");
            HashSet::new()
        });

    for sha in seen_shas {
        // Skip if already tracked (no work needed)
        if existing_shas.contains(&sha) {
            skipped_existing += 1;
            continue;
        }
        if reachable.contains(&sha) {
            state.add_seen_sha(repo, sha);
            added += 1;
        }
    }

    tracing::debug!(
        "seen_shas: total={}, skipped_existing={}, reachable_commits={}, added={}",
        total,
        skipped_existing,
        reachable.len(),
        added
    );

    // Garbage collect old seen SHAs
    gc_seen_shas(git_repo, state, repo, &local_state.trunk);

    // Stage 2: Build target state
    println!("Building target model...");
    let target_state = build_target_state(
        git_repo,
        &local_state,
        &remote_state,
        &scope,
        &authors_filter,
    );

    // Stage 3: Compute diffs
    let plan = compute_sync_plan(
        git_repo,
        state,
        repo,
        &local_state,
        &remote_state,
        &target_state,
        &options,
    );

    // Stage 4: Validate
    validate_plan(&plan)?;

    // Print plan
    print_plan(&plan, options.dry_run);

    if plan.is_empty() {
        println!("\n{}", "Everything is in sync!".green());
        return Ok(());
    }

    // Stage 5: Apply (if not dry-run)
    if options.dry_run {
        println!(
            "\n{}",
            "Dry run mode: no changes applied.".bright_blue().bold()
        );
    } else if plan.has_remote_changes() {
        // Prompt for confirmation before applying remote changes
        if !std::io::stdin().is_terminal() {
            bail!(
                "Remote changes require confirmation. Use --dry-run to preview or run interactively to confirm."
            );
        }
        if confirm_remote_changes() {
            println!("\nApplying changes...");
            apply_plan(git_repo, state, repo, &client, &repo_id, &plan)?;
            println!("\n{}", "Sync complete!".green().bold());
        } else {
            println!("\n{}", "Aborted.".yellow());
        }
    } else {
        // Only local changes - apply without confirmation
        println!("\nApplying changes...");
        apply_plan(git_repo, state, repo, &client, &repo_id, &plan)?;
        println!("\n{}", "Sync complete!".green().bold());
    }

    Ok(())
}

/// Prompt user to confirm remote changes
fn confirm_remote_changes() -> bool {
    use std::io::{self, Write};

    print!("Apply remote changes? [y/N] ");
    io::stdout().flush().unwrap();

    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_err() {
        return false;
    }

    matches!(input.trim().to_lowercase().as_str(), "y" | "yes")
}

// ============== Stage 1: Read Functions ==============

/// Garbage collect seen SHAs that are no longer needed.
/// Prunes SHAs that are:
/// - Ancestors of origin/trunk (already merged)
/// - Not reachable from any tracked branch
fn gc_seen_shas(git_repo: &GitRepo, state: &mut State, repo: &str, trunk: &str) {
    const MAX_GC_DURATION: Duration = Duration::from_millis(100);

    let Some(repo_state) = state.get_repo_state_mut(repo) else {
        return;
    };

    // Collect all tracked branch HEADs
    let tracked_shas: Vec<String> = collect_tracked_branch_shas(git_repo, &repo_state.tree);
    let origin_trunk = format!("{}/{}", DEFAULT_REMOTE, trunk);

    // Copy SHAs into a Vec and shuffle for stochastic traversal
    let mut shas_to_check: Vec<String> = repo_state.seen_remote_shas.iter().cloned().collect();
    let total_shas = shas_to_check.len();
    shas_to_check.shuffle(&mut rand::rng());

    let start = Instant::now();
    let mut traversed = 0;
    let mut deleted = 0;

    for sha in shas_to_check {
        // Stop after time budget is exhausted
        if start.elapsed() >= MAX_GC_DURATION {
            break;
        }

        traversed += 1;

        // Check if this SHA should be pruned
        let should_keep = {
            // Prune if merged to main
            if git_repo.is_ancestor(&sha, &origin_trunk).unwrap_or(false) {
                false
            } else {
                // Keep if reachable from any tracked branch
                tracked_shas
                    .iter()
                    .any(|branch_sha| git_repo.is_ancestor(&sha, branch_sha).unwrap_or(false))
            }
        };

        if !should_keep {
            repo_state.seen_remote_shas.remove(&sha);
            deleted += 1;
        }
    }

    if total_shas > 0 {
        let percentage = (traversed as f64 / total_shas as f64) * 100.0;
        tracing::debug!(
            "gc_seen_shas: traversed {}/{} ({:.1}%), deleted {}",
            traversed,
            total_shas,
            percentage,
            deleted
        );
    }
}

/// Get all local branches that are fully merged into origin/trunk.
/// These branches are safe to delete unconditionally (Strategy B).
fn get_merged_branches(trunk: &str) -> Result<HashSet<String>> {
    let output = run_git(&[
        "branch",
        "--merged",
        &format!("{}/{}", DEFAULT_REMOTE, trunk),
    ])?;
    Ok(output
        .stdout
        .lines()
        .map(|line| line.trim().trim_start_matches("* ").to_string())
        .filter(|name| !name.is_empty() && name != trunk)
        .collect())
}

/// Collect SHAs of all tracked branch HEADs
fn collect_tracked_branch_shas(git_repo: &GitRepo, branch: &Branch) -> Vec<String> {
    let mut shas = Vec::new();

    // Add this branch's SHA if it exists
    if let Ok(sha) = git_repo.sha(&branch.name) {
        shas.push(sha);
    }

    // Recurse into children
    for child in &branch.branches {
        shas.extend(collect_tracked_branch_shas(git_repo, child));
    }

    shas
}

/// Read current local state from git-stack and git
fn read_local_state(git_repo: &GitRepo, state: &State, repo: &str) -> Result<LocalState> {
    let trunk = git_trunk(git_repo).ok_or_else(|| anyhow!("No remote configured"))?;
    let mut branches = HashMap::new();

    // Get the tree for this repo
    let Some(tree) = state.get_tree(repo) else {
        return Ok(LocalState {
            branches,
            trunk: trunk.main_branch,
        });
    };

    // Walk the tree and collect branch info
    collect_local_branches(git_repo, tree, None, &mut branches);

    Ok(LocalState {
        branches,
        trunk: trunk.main_branch,
    })
}

/// Recursively collect branch info from git-stack tree
fn collect_local_branches(
    git_repo: &GitRepo,
    branch: &Branch,
    parent: Option<&str>,
    branches: &mut HashMap<String, LocalBranch>,
) {
    let remote_ref = format!("{}/{}", DEFAULT_REMOTE, branch.name);
    let pushed_to_remote = git_repo.ref_exists(&remote_ref);

    branches.insert(
        branch.name.clone(),
        LocalBranch {
            parent: parent.map(|s| s.to_string()),
            pr_number: branch.pr_number,
            pushed_to_remote,
        },
    );

    for child in &branch.branches {
        collect_local_branches(git_repo, child, Some(&branch.name), branches);
    }
}

/// Read current remote state from GitHub, fetching open PRs only for the `scope` branches
/// (the user's stack) rather than enumerating every open PR in the repo.
/// Returns (RemoteState, seen_shas)
fn read_remote_state(
    client: &GitHubClient,
    repo_id: &RepoIdentifier,
    scope: &[String],
) -> Result<(RemoteState, HashSet<String>)> {
    // Only show spinner if stderr is a TTY
    let spinner = if std::io::stderr().is_terminal() {
        let pb = ProgressBar::new_spinner();
        pb.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.cyan} {msg}")
                .expect("valid template"),
        );
        pb.enable_steady_tick(std::time::Duration::from_millis(100));
        Some(pb)
    } else {
        None
    };

    // Fetch open PRs, scoped to the stack's branches (cost scales with stack size, not repo
    // PR count). `list_open_prs_for_branches` early-returns on an empty scope.
    if let Some(s) = &spinner {
        s.set_message("Fetching open PRs...");
    }
    let scoped = client.list_open_prs_for_branches(repo_id, scope);

    let prs: HashMap<String, RemotePr> = scoped
        .found
        .iter()
        .map(|(branch, pr)| (branch.clone(), RemotePr::from(pr)))
        .collect();
    let authors: HashMap<String, String> = scoped
        .found
        .iter()
        .map(|(branch, pr)| (branch.clone(), pr.user.login.clone()))
        .collect();

    // Fetch closed PRs (includes merged) with caching
    if let Some(s) = &spinner {
        s.set_message("Fetching closed PRs...");
    }
    let closed_progress = |_page: usize, count: usize| {
        if let Some(s) = &spinner {
            s.set_message(format!("Fetching closed PRs... ({count} loaded)"));
        }
    };

    let cache = crate::pr_cache::PrCacheHandle::open().context("Failed to open PR cache")?;

    let closed_result = client
        .list_closed_prs_with_cache(repo_id, &cache, Some(&closed_progress))
        .map_err(|e| anyhow!("{}", e))?;

    let closed_prs: HashMap<String, RemotePr> = closed_result
        .prs
        .iter()
        .map(|(branch, pr)| (branch.clone(), RemotePr::from(pr)))
        .collect();

    if let Some(s) = spinner {
        s.finish_and_clear();
    }

    // Collect all PR head SHAs for seen tracking
    let mut seen_shas: HashSet<String> = scoped
        .found
        .values()
        .map(|pr| pr.head.sha.clone())
        .collect();
    seen_shas.extend(closed_result.prs.values().map(|pr| pr.head.sha.clone()));

    Ok((
        RemoteState {
            prs,
            closed_prs,
            authors,
        },
        seen_shas,
    ))
}

/// Walk a PR base chain starting from `start`, returning the branch names visited (including
/// `start`). `lookup_base(branch)` yields the branch's open-PR base ref, or `None` when the
/// branch has no open PR. The walk stops at `trunk`, at the first branch with no PR, or on a
/// cycle. Pure over the injected lookup so it is unit-testable without a live client.
fn walk_pr_base_chain(
    start: &str,
    trunk: &str,
    mut lookup_base: impl FnMut(&str) -> Option<String>,
) -> Vec<String> {
    let mut visited: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut current = start.to_string();

    loop {
        if current == trunk || !seen.insert(current.clone()) {
            break;
        }
        visited.push(current.clone());
        match lookup_base(&current) {
            Some(base) => current = base,
            None => break,
        }
    }

    visited
}

/// Branch names whose open PRs `sync` should fetch/track: the local tree, plus (on a fresh clone
/// with an empty tree) the reachable stack reconstructed by walking the current branch's PR base
/// chain. Never enumerates the whole repo.
fn compute_scope_branches(
    client: &GitHubClient,
    repo_id: &RepoIdentifier,
    local: &LocalState,
    current_branch: &str,
    pull: bool,
) -> Vec<String> {
    let mut scope: HashSet<String> = local.branches.keys().cloned().collect();

    // Fresh clone / empty tree (no non-trunk branches tracked): reconstruct the reachable stack
    // by walking the current branch's PR base chain. `find_pr_for_branch` on trunk returns None.
    let has_non_trunk = local.branches.keys().any(|b| b != &local.trunk);
    if pull && !has_non_trunk && !current_branch.is_empty() && current_branch != local.trunk {
        let chain = walk_pr_base_chain(current_branch, &local.trunk, |branch| {
            client
                .find_pr_for_branch(repo_id, branch)
                .ok()
                .flatten()
                .map(|pr| pr.base.ref_name)
        });
        scope.extend(chain);
    }

    scope.into_iter().collect()
}

// ============== Stage 2: Model Functions ==============

/// Fold author-discovered open PRs into the pull-direction inputs: seed `scope` with each PR's
/// head branch (so the inject gate's `scope.contains` check passes), upsert `remote.prs`/
/// `remote.authors` (so the author gate passes by construction), and record head SHAs in
/// `seen_shas`. Pure over its inputs — no client — so it is unit-testable. Existing entries win
/// (`or_insert_with`) so a stack-scoped REST fetch is never clobbered by discovery.
fn merge_discovered_prs(
    discovered: &[PullRequest],
    scope: &mut HashSet<String>,
    remote: &mut RemoteState,
    seen_shas: &mut HashSet<String>,
) {
    for pr in discovered {
        let branch = pr.head.ref_name.clone();
        if branch.is_empty() {
            continue;
        }
        scope.insert(branch.clone());
        seen_shas.insert(pr.head.sha.clone());
        remote
            .authors
            .entry(branch.clone())
            .or_insert_with(|| pr.user.login.clone());
        remote
            .prs
            .entry(branch)
            .or_insert_with(|| RemotePr::from(pr));
    }
}

/// Remote-only PR branches eligible to be pulled into the tree, after stack-scoping and
/// `authors_filter` gating. Returns (branch, pr_base, pr_number). Pure and testable — no
/// `git_repo`. Rules for each remote open PR branch not already in `local.branches`:
/// - skip if not in `scope` (stack-scoping; defensive even though the fetch is scoped);
/// - skip if `authors_filter` is non-empty and the branch's author is missing or not listed
///   ("can't tell / not mine ⇒ don't track");
/// - otherwise include with the PR's base / number.
fn remote_only_branches_to_inject(
    local: &LocalState,
    remote: &RemoteState,
    scope: &HashSet<String>,
    authors_filter: &[String],
) -> Vec<(String, String, u64)> {
    let mut result: Vec<(String, String, u64)> = Vec::new();

    for (branch, pr) in &remote.prs {
        if local.branches.contains_key(branch) {
            continue;
        }
        if !scope.contains(branch) {
            continue;
        }
        if !authors_filter.is_empty() {
            let author_listed = remote
                .authors
                .get(branch)
                .is_some_and(|author| crate::github::author_in_filter(authors_filter, author));
            if !author_listed {
                continue;
            }
        }
        result.push((branch.clone(), pr.base.clone(), pr.number));
    }

    // Deterministic ordering for stable plans/tests.
    result.sort();
    result
}

/// Build target state by merging local and remote
fn build_target_state(
    git_repo: &GitRepo,
    local: &LocalState,
    remote: &RemoteState,
    scope: &HashSet<String>,
    authors_filter: &[String],
) -> TargetState {
    let mut branches = HashMap::new();

    // First, collect all branches from remote PRs to know what's being pulled
    let remote_branches: HashSet<&str> = remote.prs.keys().map(|s| s.as_str()).collect();

    // Process all local branches
    for (name, local_branch) in &local.branches {
        let pr = remote.prs.get(name);
        let pr_number = pr.map(|pr| pr.number).or(local_branch.pr_number);

        // Determine the correct parent:
        // - If there's a PR and its base is being pulled in (or already in local), use PR base
        // - Otherwise use local parent (local wins)
        let (parent, expected_pr_base) = if let Some(pr) = pr {
            // Check if the PR's base is in the set of branches we're pulling
            // or if it's the trunk
            let pr_base_is_available = pr.base == local.trunk
                || local.branches.contains_key(&pr.base)
                || remote_branches.contains(pr.base.as_str());

            if pr_base_is_available && local_branch.parent.as_ref() != Some(&pr.base) {
                // PR base is available and differs from local - prefer PR base
                // This handles the case where we're reconstructing from GitHub
                (Some(pr.base.clone()), Some(pr.base.clone()))
            } else {
                // Use local parent
                (local_branch.parent.clone(), local_branch.parent.clone())
            }
        } else {
            // No PR - use local parent
            (local_branch.parent.clone(), local_branch.parent.clone())
        };

        branches.insert(
            name.clone(),
            TargetBranch {
                parent,
                pr_number,
                expected_pr_base,
                pushed_to_remote: local_branch.pushed_to_remote,
            },
        );
    }

    // Process remote-only PR branches eligible for pulling (stack-scoped + author-gated).
    // Because tracked branches are always in `local.branches`, this only ever injects
    // reconstructed-chain branches — never an arbitrary `main`-based PR.
    for (branch_name, pr_base, pr_number) in
        remote_only_branches_to_inject(local, remote, scope, authors_filter)
    {
        let remote_ref = format!("{}/{}", DEFAULT_REMOTE, branch_name);
        let pushed_to_remote = git_repo.ref_exists(&remote_ref);

        branches.insert(
            branch_name,
            TargetBranch {
                parent: Some(pr_base.clone()),
                pr_number: Some(pr_number),
                expected_pr_base: Some(pr_base),
                pushed_to_remote,
            },
        );
    }

    TargetState {
        branches,
        trunk: local.trunk.clone(),
    }
}

// ============== Stage 3: Diff Functions ==============

/// Walk up the parent chain from `branch`, skipping ancestors that are themselves
/// being unmounted, and return the first surviving ancestor (falling back to
/// `trunk` when we run off the top of the chain).
fn resolve_repoint(
    branch: &str,
    branches: &HashMap<String, LocalBranch>,
    trunk: &str,
    unmount_set: &HashSet<String>,
) -> String {
    let mut visited: HashSet<String> = HashSet::new();
    let mut candidate: Option<String> = branches.get(branch).and_then(|b| b.parent.clone());

    while let Some(name) = candidate {
        if name == trunk || !unmount_set.contains(&name) {
            return name;
        }
        if !visited.insert(name.clone()) {
            // Cycle guard (shouldn't happen in a well-formed tree).
            return trunk.to_string();
        }
        candidate = branches.get(&name).and_then(|b| b.parent.clone());
    }

    trunk.to_string()
}

/// Compute the sync plan by diffing current state against target
fn compute_sync_plan(
    git_repo: &GitRepo,
    state: &State,
    repo: &str,
    local: &LocalState,
    remote: &RemoteState,
    target: &TargetState,
    options: &SyncOptions,
) -> SyncPlan {
    let mut local_changes = Vec::new();
    let mut remote_changes = Vec::new();
    let mut warnings = Vec::new();

    // Compute local changes (pull direction)
    if !options.push_only {
        // Collect branches that need to be mounted, then topologically sort them
        let mut branches_to_mount: Vec<(String, String)> = Vec::new(); // (branch, parent)
        let mut pr_updates: Vec<(String, u64)> = Vec::new();

        for (branch_name, target_branch) in &target.branches {
            // Skip trunk
            if branch_name == &local.trunk {
                continue;
            }

            let local_branch = local.branches.get(branch_name);

            // Check if we need to mount a branch from remote
            if local_branch.is_none() && target_branch.pr_number.is_some() {
                // Branch has a PR but isn't in our local tree
                if let Some(parent) = &target_branch.parent {
                    branches_to_mount.push((branch_name.clone(), parent.clone()));
                }
            }

            // Check if we need to update cached PR number
            if let Some(local_branch) = local_branch {
                if let Some(pr_number) = target_branch.pr_number
                    && local_branch.pr_number != Some(pr_number)
                {
                    pr_updates.push((branch_name.clone(), pr_number));
                }

                // Check if we need to re-mount (local parent differs from target parent)
                if local_branch.parent != target_branch.parent
                    && let Some(parent) = &target_branch.parent
                {
                    branches_to_mount.push((branch_name.clone(), parent.clone()));
                }
            }
        }

        // Ensure all parents are available (transitive closure)
        // Keep adding missing parents until no more are needed
        loop {
            let mut parents_to_add: Vec<(String, String)> = Vec::new();

            for (_, parent) in &branches_to_mount {
                // Skip if parent is trunk
                if parent == &local.trunk {
                    continue;
                }
                // Skip if parent is already in local tree
                if local.branches.contains_key(parent) {
                    continue;
                }
                // Skip if parent is already being mounted
                if branches_to_mount.iter().any(|(b, _)| b == parent) {
                    continue;
                }
                // Check if parent exists as remote tracking branch
                let remote_ref = format!("{}/{}", DEFAULT_REMOTE, parent);
                if git_repo.ref_exists(&remote_ref) {
                    // Mount missing parent on trunk
                    parents_to_add.push((parent.clone(), local.trunk.clone()));
                }
            }

            if parents_to_add.is_empty() {
                break;
            }
            branches_to_mount.extend(parents_to_add);
        }

        // Filter out branches whose parents still can't be resolved
        // (parents that don't exist as remote refs and aren't in tree)
        let branches_being_mounted: HashSet<String> =
            branches_to_mount.iter().map(|(b, _)| b.clone()).collect();

        let (valid, invalid): (Vec<_>, Vec<_>) =
            branches_to_mount.into_iter().partition(|(_, parent)| {
                parent == &local.trunk
                    || local.branches.contains_key(parent)
                    || branches_being_mounted.contains(parent)
            });

        for (branch, parent) in invalid {
            warnings.push(format!(
                "Skipping branch '{}': parent '{}' not available on remote",
                branch, parent
            ));
        }

        let branches_to_mount = valid;

        // Topologically sort branches to mount (parents before children)
        let sorted_branches = topological_sort_branches(&branches_to_mount, &local.trunk);

        // Add mount changes in topological order
        for (branch_name, parent) in sorted_branches {
            local_changes.push(LocalChange::MountBranch {
                name: branch_name,
                parent,
            });
        }

        // Add PR number updates (order doesn't matter for these)
        for (branch, pr_number) in pr_updates {
            local_changes.push(LocalChange::UpdatePrNumber { branch, pr_number });
        }
    }

    // Detect merged/closed PRs and handle unmounting (pull direction)
    // This runs regardless of push_only/pull_only since it's about reconciling state
    let mut branches_to_delete: Vec<String> = Vec::new();

    tracing::debug!(
        "Checking for merged PRs. Local branches: {:?}, Closed PRs: {:?}",
        local.branches.keys().collect::<Vec<_>>(),
        remote.closed_prs.keys().collect::<Vec<_>>()
    );

    // First pass: collect the raw set of branches to unmount (eligible by closed PR).
    let mut unmount_set: HashSet<String> = HashSet::new();
    for branch_name in local.branches.keys() {
        if branch_name == &local.trunk {
            continue;
        }
        if !remote.prs.contains_key(branch_name)
            && let Some(closed_pr) = remote.closed_prs.get(branch_name)
            && matches!(
                closed_pr.state,
                RemotePrState::Merged | RemotePrState::Closed
            )
        {
            unmount_set.insert(branch_name.clone());
        }
    }

    // Second pass: emit unmount entries with transitively-resolved repoint targets,
    // and compute safe-to-delete.
    let mut branches_to_unmount: Vec<(String, String)> = Vec::new(); // (branch, repoint_to)
    for branch_name in local.branches.keys() {
        if !unmount_set.contains(branch_name) {
            continue;
        }
        let closed_pr = remote
            .closed_prs
            .get(branch_name)
            .expect("unmount_set membership implies closed PR");

        tracing::debug!(
            "Branch '{}' has closed PR #{} with state {:?}",
            branch_name,
            closed_pr.number,
            closed_pr.state
        );

        let repoint_to = resolve_repoint(branch_name, &local.branches, &local.trunk, &unmount_set);
        branches_to_unmount.push((branch_name.clone(), repoint_to));

        // Determine if local branch is safe to delete
        // Safe if: merged, OR (closed AND remote exists AND local is ancestor of remote)
        let safe_to_delete = if closed_pr.state == RemotePrState::Merged {
            true
        } else {
            // Closed but not merged - check if remote has our work
            let remote_ref = format!("{}/{}", DEFAULT_REMOTE, branch_name);
            git_repo.ref_exists(&remote_ref)
                && git_repo
                    .is_ancestor(branch_name, &remote_ref)
                    .unwrap_or(false)
        };

        if safe_to_delete {
            branches_to_delete.push(branch_name.clone());
        }
    }

    // Add unmount changes and retarget PRs for children
    if !options.push_only {
        for (branch_name, repoint_to) in &branches_to_unmount {
            local_changes.push(LocalChange::UnmountBranch {
                name: branch_name.clone(),
                repoint_children_to: repoint_to.clone(),
            });

            // Add retarget changes for children of this unmounted branch
            // Find all branches whose parent is the unmounted branch
            for (child_name, child_branch) in &local.branches {
                if child_branch.parent.as_ref() == Some(branch_name) {
                    // Check if this child has an open PR that needs retargeting
                    if let Some(pr) = remote.prs.get(child_name) {
                        // PR's old base should be the unmounted branch, new base is repoint_to
                        if pr.base == *branch_name {
                            // Check if the new base branch is pushed to remote
                            let new_base_remote_ref = format!("{}/{}", DEFAULT_REMOTE, repoint_to);
                            if !git_repo.ref_exists(&new_base_remote_ref) {
                                // Need to push the intermediate branch first
                                remote_changes.push(RemoteChange::PushBranch {
                                    branch: repoint_to.clone(),
                                });
                            }
                            remote_changes.push(RemoteChange::RetargetPr {
                                number: pr.number,
                                branch: child_name.clone(),
                                old_base: branch_name.clone(),
                                new_base: repoint_to.clone(),
                            });
                        }
                    }
                }
            }
        }
    }

    // Set of merged branches for use in remote changes
    let merged_branches: HashSet<&str> = branches_to_unmount
        .iter()
        .map(|(b, _)| b.as_str())
        .collect();

    // Compute remote changes (push direction)
    if !options.pull_only {
        for (branch_name, target_branch) in &target.branches {
            // Skip trunk
            if branch_name == &local.trunk {
                continue;
            }

            // Skip branches not in local tree (we don't push absence)
            if !local.branches.contains_key(branch_name) {
                continue;
            }

            // Skip merged branches - don't try to create PRs for them
            if merged_branches.contains(branch_name.as_str()) {
                continue;
            }

            let local_branch = local.branches.get(branch_name).unwrap();

            // Skip branches not pushed to remote
            if !local_branch.pushed_to_remote {
                continue;
            }

            let remote_pr = remote.prs.get(branch_name);

            match (remote_pr, &target_branch.expected_pr_base) {
                // PR exists, check if base matches
                (Some(pr), Some(expected_base)) if pr.base != *expected_base => {
                    // Check if the new base branch is pushed to remote
                    let new_base_remote_ref = format!("{}/{}", DEFAULT_REMOTE, expected_base);
                    if !git_repo.ref_exists(&new_base_remote_ref) {
                        // Need to push the intermediate branch first
                        remote_changes.push(RemoteChange::PushBranch {
                            branch: expected_base.clone(),
                        });
                    }
                    remote_changes.push(RemoteChange::RetargetPr {
                        number: pr.number,
                        branch: branch_name.clone(),
                        old_base: pr.base.clone(),
                        new_base: expected_base.clone(),
                    });
                }
                // No open PR: don't auto-create PRs during sync.
                // Users create PRs explicitly with `git stack pr`.
                _ => {}
            }
        }
    }

    // Compute branch deletions (both strategies)
    if !options.push_only {
        let current_branch = git_repo.current_branch().unwrap_or_default();
        let seen_shas = state.get_seen_shas(repo);

        // Get branches fully merged into origin/trunk (Strategy B)
        let merged_into_main = get_merged_branches(&local.trunk).unwrap_or_default();

        // Track which branches we're already deleting to avoid duplicates
        let mut branches_to_delete: HashSet<String> = HashSet::new();

        // Strategy A: PR-based deletion with seen SHA verification
        // For squash/rebase merged PRs where the branch tip won't be an ancestor of main
        for branch_name in local.branches.keys() {
            // Skip trunk
            if branch_name == &local.trunk {
                continue;
            }

            // Skip if currently checked out
            if branch_name == &current_branch {
                continue;
            }

            // Check if this branch has a merged PR
            if let Some(closed_pr) = remote.closed_prs.get(branch_name)
                && closed_pr.state == RemotePrState::Merged
            {
                // Check if remote branch is deleted (fetch --prune already ran)
                let remote_ref = format!("{}/{}", DEFAULT_REMOTE, branch_name);
                if !git_repo.ref_exists(&remote_ref) {
                    // Check if local HEAD SHA is in seen set
                    if let Ok(local_sha) = git_repo.sha(branch_name)
                        && let Some(seen) = seen_shas
                        && seen.contains(&local_sha)
                    {
                        branches_to_delete.insert(branch_name.clone());
                        local_changes.push(LocalChange::DeleteLocalBranch {
                            name: branch_name.clone(),
                            reason: DeleteReason::SeenOnRemote {
                                verified_sha: local_sha,
                            },
                        });
                    }
                }
            }
        }

        // Strategy B: Git merge-based deletion
        // For merge-commit merges where branch tip IS an ancestor of main
        for branch_name in &merged_into_main {
            // Skip trunk
            if branch_name == &local.trunk {
                continue;
            }

            // Skip if currently checked out
            if branch_name == &current_branch {
                continue;
            }

            // Skip if already marked for deletion by Strategy A
            if branches_to_delete.contains(branch_name) {
                continue;
            }

            // Only delete if it's a tracked branch (in our local state)
            if local.branches.contains_key(branch_name) {
                branches_to_delete.insert(branch_name.clone());
                local_changes.push(LocalChange::DeleteLocalBranch {
                    name: branch_name.clone(),
                    reason: DeleteReason::MergedIntoMain,
                });
            }
        }

        // Strategy C: Local branch is ancestor of origin/<branch>
        // All local work has been pushed, safe to delete local branch
        for branch_name in local.branches.keys() {
            // Skip trunk
            if branch_name == &local.trunk {
                continue;
            }

            // Skip if currently checked out
            if branch_name == &current_branch {
                continue;
            }

            // Skip if already marked for deletion
            if branches_to_delete.contains(branch_name) {
                continue;
            }

            // Check if local branch is ancestor of origin/<branch>
            let remote_ref = format!("{}/{}", DEFAULT_REMOTE, branch_name);
            if git_repo.ref_exists(&remote_ref)
                && let Ok(true) = git_repo.is_ancestor(branch_name, &remote_ref)
            {
                local_changes.push(LocalChange::DeleteLocalBranch {
                    name: branch_name.clone(),
                    reason: DeleteReason::AncestorOfRemote,
                });
            }
        }
    }

    SyncPlan {
        local_changes,
        remote_changes,
        warnings,
        branches_to_unmount: branches_to_unmount
            .iter()
            .map(|(name, _)| name.clone())
            .collect(),
        branches_to_delete,
    }
}

/// Topologically sort branches so parents come before children.
/// Uses Kahn's algorithm for topological sorting.
fn topological_sort_branches(branches: &[(String, String)], trunk: &str) -> Vec<(String, String)> {
    if branches.is_empty() {
        return Vec::new();
    }

    // Build a map of branch -> parent
    let branch_to_parent: HashMap<&str, &str> = branches
        .iter()
        .map(|(b, p)| (b.as_str(), p.as_str()))
        .collect();

    // Build set of all branches we're mounting
    let branches_set: HashSet<&str> = branches.iter().map(|(b, _)| b.as_str()).collect();

    // Compute in-degree for each branch (how many branches depend on it being mounted first)
    // A branch has in-degree > 0 if its parent is also in the set of branches to mount
    let mut in_degree: HashMap<&str, usize> = HashMap::new();
    for (branch, parent) in branches {
        in_degree.entry(branch.as_str()).or_insert(0);
        if branches_set.contains(parent.as_str()) {
            *in_degree.entry(branch.as_str()).or_insert(0) += 1;
        }
    }

    // Start with branches whose parent is trunk or already mounted (in-degree 0)
    let mut queue: Vec<&str> = in_degree
        .iter()
        .filter(|(_, deg)| **deg == 0)
        .map(|(b, _)| *b)
        .collect();

    // Sort for deterministic output
    queue.sort();

    let mut result = Vec::new();

    while let Some(branch) = queue.pop() {
        // Add this branch to result
        if let Some(&parent) = branch_to_parent.get(branch) {
            result.push((branch.to_string(), parent.to_string()));
        }

        // Decrease in-degree for branches that depend on this one
        for (child, parent) in branches {
            if parent == branch
                && let Some(deg) = in_degree.get_mut(child.as_str())
            {
                *deg -= 1;
                if *deg == 0 {
                    queue.push(child.as_str());
                    queue.sort(); // Keep sorted for determinism
                }
            }
        }
    }

    result
}

// ============== Stage 4: Validate Functions ==============

/// Validate the sync plan for safety
fn validate_plan(plan: &SyncPlan) -> Result<()> {
    // Currently we don't have any validations that would fail
    // Future: check for potential data loss scenarios
    Ok(())
}

// ============== Stage 5: Apply Functions ==============

/// Apply the sync plan
fn apply_plan(
    git_repo: &GitRepo,
    state: &mut State,
    repo: &str,
    client: &GitHubClient,
    repo_id: &RepoIdentifier,
    plan: &SyncPlan,
) -> Result<()> {
    // If current branch is being unmounted, checkout a safe ancestor first
    if !plan.branches_to_unmount.is_empty() {
        let current_branch = git_repo.current_branch().unwrap_or_default();
        let unmount_set: HashSet<&str> = plan
            .branches_to_unmount
            .iter()
            .map(|s| s.as_str())
            .collect();

        if unmount_set.contains(current_branch.as_str()) {
            // Find the first ancestor that's NOT being unmounted
            let trunk = git_trunk(git_repo).ok_or_else(|| anyhow!("No remote configured"))?;
            let mut safe_branch = trunk.main_branch.clone();
            let mut current = current_branch.clone();

            while let Some(parent) = state.get_parent_branch_of(repo, &current) {
                if !unmount_set.contains(parent.name.as_str()) {
                    safe_branch = parent.name.clone();
                    break;
                }
                current = parent.name.clone();
            }

            println!(
                "Branch {} was merged. Switching to {}...",
                current_branch.yellow(),
                safe_branch.green()
            );
            run_git(&["checkout", &safe_branch])?;
        }
    }

    // Apply local changes first (checkout, mount, update pr_number)
    for change in &plan.local_changes {
        apply_local_change(git_repo, state, repo, change)?;
    }

    // Save state after local changes
    state.save_state()?;

    // Apply remote changes (retarget PRs, push intermediate branches)
    for change in &plan.remote_changes {
        apply_remote_change(client, repo_id, change)?;
    }

    // Save state again if PR numbers were updated
    state.save_state()?;

    // Delete local branches that are safe to delete (work preserved on remote)
    for branch_name in &plan.branches_to_delete {
        if git_repo.branch_exists(branch_name) {
            println!("Deleting local branch {}...", branch_name.yellow());
            if let Err(e) = run_git(&["branch", "-D", branch_name]) {
                tracing::warn!("Failed to delete local branch {}: {}", branch_name, e);
            }
        }
    }

    Ok(())
}

/// Remove a branch from the git-stack tree, repointing its children to the given parent.
fn unmount_branch_from_tree(
    git_repo: &GitRepo,
    state: &mut State,
    repo: &str,
    name: &str,
    repoint_children_to: &str,
) -> Result<()> {
    // Find all children of this branch and repoint them
    let children: Vec<String> = if let Some(tree) = state.get_tree(repo) {
        if let Some(branch) = find_branch_by_name(tree, name) {
            branch.branches.iter().map(|b| b.name.clone()).collect()
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };

    // Repoint each child to the new parent
    for child in children {
        println!(
            "    Repointing '{}' → '{}'",
            child.yellow(),
            repoint_children_to.green()
        );
        state.mount(
            git_repo,
            repo,
            &child,
            Some(repoint_children_to.to_string()),
        )?;
    }

    // Delete the branch from the tree
    state.delete_branch(repo, name)?;
    Ok(())
}

/// Apply a single local change
fn apply_local_change(
    git_repo: &GitRepo,
    state: &mut State,
    repo: &str,
    change: &LocalChange,
) -> Result<()> {
    match change {
        LocalChange::MountBranch { name, parent } => {
            println!("  Mounting '{}' on '{}'", name.yellow(), parent.green());
            state.mount(git_repo, repo, name, Some(parent.clone()))?;
        }
        LocalChange::UnmountBranch {
            name,
            repoint_children_to,
        } => {
            println!(
                "  Unmounting '{}' (children → '{}')",
                name.yellow(),
                repoint_children_to.green()
            );
            unmount_branch_from_tree(git_repo, state, repo, name, repoint_children_to)?;
        }
        LocalChange::UpdatePrNumber { branch, pr_number } => {
            println!(
                "  Updating PR# for '{}' → #{}",
                branch.yellow(),
                pr_number.to_string().green()
            );
            if let Some(tree) = state.get_tree_mut(repo)
                && let Some(b) = find_branch_by_name_mut(tree, branch)
            {
                b.pr_number = Some(*pr_number);
            }
        }
        LocalChange::DeleteLocalBranch { name, reason } => {
            let reason_str = match reason {
                DeleteReason::SeenOnRemote { verified_sha } => {
                    format!(
                        "PR merged, SHA {} verified on remote",
                        &verified_sha[..8.min(verified_sha.len())]
                    )
                }
                DeleteReason::MergedIntoMain => "fully merged into main".to_string(),
                DeleteReason::AncestorOfRemote => "local is ancestor of remote".to_string(),
            };
            println!(
                "  {} local branch '{}' ({})",
                "Deleting".red().bold(),
                name.yellow(),
                reason_str
            );

            // For SeenOnRemote, double-check SHA hasn't changed since plan was computed
            if let DeleteReason::SeenOnRemote { verified_sha } = reason
                && let Ok(current_sha) = git_repo.sha(name)
                && current_sha != *verified_sha
            {
                println!(
                    "    {} Branch SHA changed ({} -> {}), skipping deletion",
                    "Warning:".yellow(),
                    &verified_sha[..8.min(verified_sha.len())],
                    &current_sha[..8.min(current_sha.len())]
                );
                return Ok(());
            }

            // Delete the git branch
            run_git(&["branch", "-D", name])?;
            println!("    Branch '{}' deleted.", name);

            // Remove from git-stack tree ONLY if the remote is also gone
            // For AncestorOfRemote, we keep the branch in tree since remote still exists
            if !matches!(reason, DeleteReason::AncestorOfRemote) {
                // A prior UnmountBranch in the same plan may have already removed it.
                let still_in_tree = state
                    .get_tree(repo)
                    .and_then(|t| find_branch_by_name(t, name))
                    .is_some();
                if still_in_tree {
                    let repoint_to = state
                        .get_parent_branch_of(repo, name)
                        .map(|p| p.name.clone())
                        .or_else(|| state.get_tree(repo).map(|t| t.name.clone()))
                        .unwrap_or_else(|| "main".to_string());
                    unmount_branch_from_tree(git_repo, state, repo, name, &repoint_to)?;
                    println!("    Removed '{}' from git-stack tree.", name);
                }
            }
        }
    }
    Ok(())
}

/// Apply a single remote change
fn apply_remote_change(
    client: &GitHubClient,
    repo_id: &RepoIdentifier,
    change: &RemoteChange,
) -> Result<()> {
    match change {
        RemoteChange::RetargetPr {
            number,
            branch,
            old_base,
            new_base,
        } => {
            println!(
                "  Retargeting PR #{} for '{}': {} → {}",
                number.to_string().green(),
                branch.yellow(),
                old_base.red(),
                new_base.green()
            );

            client
                .update_pr(
                    repo_id,
                    *number,
                    UpdatePrRequest {
                        base: Some(new_base),
                        title: None,
                        body: None,
                    },
                )
                .map_err(|e| anyhow!("{}", e))?;
        }
        RemoteChange::PushBranch { branch } => {
            println!("  Pushing '{}' to remote", branch.yellow());
            run_git(&[
                "push",
                "-u",
                "--force-with-lease",
                DEFAULT_REMOTE,
                &format!("{}:{}", branch, branch),
            ])?;
        }
    }
    Ok(())
}

/// Print the sync plan
fn print_plan(plan: &SyncPlan, dry_run: bool) {
    let prefix = if dry_run { "[dry-run] " } else { "" };

    if plan.is_empty() {
        return;
    }

    println!("\n{}Plan:", prefix);

    if !plan.local_changes.is_empty() {
        println!("  Local changes:");
        for change in &plan.local_changes {
            match change {
                LocalChange::MountBranch { name, parent } => {
                    println!("    - Mount '{}' on '{}'", name.yellow(), parent.green());
                }
                LocalChange::UnmountBranch {
                    name,
                    repoint_children_to,
                } => {
                    println!(
                        "    - Unmount '{}' (children → '{}')",
                        name.yellow(),
                        repoint_children_to.green()
                    );
                }
                LocalChange::UpdatePrNumber { branch, pr_number } => {
                    println!(
                        "    - Update PR# for '{}' → #{}",
                        branch.yellow(),
                        pr_number.to_string().green()
                    );
                }
                LocalChange::DeleteLocalBranch { name, reason } => {
                    let reason_str = match reason {
                        DeleteReason::SeenOnRemote { verified_sha } => {
                            format!(
                                "SHA {} verified on remote",
                                &verified_sha[..8.min(verified_sha.len())]
                            )
                        }
                        DeleteReason::MergedIntoMain => "merged into main".to_string(),
                        DeleteReason::AncestorOfRemote => "ancestor of remote".to_string(),
                    };
                    println!(
                        "    - {} local branch '{}' ({})",
                        "Delete".red().bold(),
                        name.red(),
                        reason_str
                    );
                }
            }
        }
    }

    if !plan.remote_changes.is_empty() {
        println!("  Remote changes:");
        for change in &plan.remote_changes {
            match change {
                RemoteChange::RetargetPr {
                    number,
                    branch,
                    old_base,
                    new_base,
                } => {
                    println!(
                        "    - Retarget PR #{} for '{}': {} → {}",
                        number.to_string().green(),
                        branch.yellow(),
                        old_base.red(),
                        new_base.green()
                    );
                }
                RemoteChange::PushBranch { branch } => {
                    println!("    - Push '{}' to remote", branch.yellow());
                }
            }
        }
    }

    if !plan.warnings.is_empty() {
        println!("  Warnings:");
        for warning in &plan.warnings {
            println!("    {}: {}", "!".yellow(), warning);
        }
    }
}

/// Helper to find a branch by name in the tree (immutable)
fn find_branch_by_name<'a>(tree: &'a Branch, name: &str) -> Option<&'a Branch> {
    if tree.name == name {
        return Some(tree);
    }
    for child in &tree.branches {
        if let Some(found) = find_branch_by_name(child, name) {
            return Some(found);
        }
    }
    None
}

/// Helper to find a branch by name in the tree (mutable)
fn find_branch_by_name_mut<'a>(tree: &'a mut Branch, name: &str) -> Option<&'a mut Branch> {
    if tree.name == name {
        return Some(tree);
    }
    for child in &mut tree.branches {
        if let Some(found) = find_branch_by_name_mut(child, name) {
            return Some(found);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lb(parent: Option<&str>) -> LocalBranch {
        LocalBranch {
            parent: parent.map(String::from),
            pr_number: None,
            pushed_to_remote: true,
        }
    }

    fn remote_pr(number: u64, base: &str) -> RemotePr {
        RemotePr {
            number,
            base: base.to_string(),
            state: RemotePrState::Open,
            title: format!("PR #{number}"),
            html_url: format!("https://example.test/pr/{number}"),
        }
    }

    /// Build a `RemoteState` from (branch, base, pr_number, author) tuples for the open PRs.
    fn remote_state(open: &[(&str, &str, u64, &str)]) -> RemoteState {
        let mut prs = HashMap::new();
        let mut authors = HashMap::new();
        for (branch, base, number, author) in open {
            prs.insert(branch.to_string(), remote_pr(*number, base));
            authors.insert(branch.to_string(), author.to_string());
        }
        RemoteState {
            prs,
            closed_prs: HashMap::new(),
            authors,
        }
    }

    fn local_state(trunk: &str, branches: &[(&str, Option<&str>)]) -> LocalState {
        let mut map = HashMap::new();
        for (name, parent) in branches {
            map.insert(name.to_string(), lb(*parent));
        }
        LocalState {
            branches: map,
            trunk: trunk.to_string(),
        }
    }

    fn scope_of(names: &[&str]) -> HashSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn inject_skips_out_of_scope_main_based_pr_and_includes_in_scope_branch() {
        // Two remote-only open PRs based on main: only `mine` is in scope.
        let local = local_state("main", &[("main", None)]);
        let remote = remote_state(&[
            ("mine", "main", 10, "alice"),
            ("someone-elses", "main", 99, "carol"),
        ]);

        // Scope excludes the arbitrary main-based PR ⇒ it is never injected.
        let scope = scope_of(&["main", "mine"]);
        let injected = remote_only_branches_to_inject(&local, &remote, &scope, &[]);
        assert_eq!(injected, vec![("mine".to_string(), "main".to_string(), 10)]);
    }

    #[test]
    fn inject_is_empty_when_scope_excludes_all_remote_branches() {
        let local = local_state("main", &[("main", None)]);
        let remote = remote_state(&[("someone-elses", "main", 99, "carol")]);
        let scope = scope_of(&["main"]);
        let injected = remote_only_branches_to_inject(&local, &remote, &scope, &[]);
        assert!(injected.is_empty());
    }

    #[test]
    fn inject_gated_by_authors_filter() {
        let local = local_state("main", &[("main", None)]);
        let remote = remote_state(&[("feature", "main", 7, "bob")]);
        let scope = scope_of(&["main", "feature"]);

        // Author "bob" is not in authors_filter ⇒ not injected.
        let injected = remote_only_branches_to_inject(&local, &remote, &scope, &["alice".into()]);
        assert!(injected.is_empty());

        // Author "bob" is listed ⇒ injected.
        let injected = remote_only_branches_to_inject(&local, &remote, &scope, &["bob".into()]);
        assert_eq!(
            injected,
            vec![("feature".to_string(), "main".to_string(), 7)]
        );

        // Empty authors_filter ⇒ injected regardless of author.
        let injected = remote_only_branches_to_inject(&local, &remote, &scope, &[]);
        assert_eq!(
            injected,
            vec![("feature".to_string(), "main".to_string(), 7)]
        );

        // Matching is case-insensitive: `BOB` in the filter matches the `bob` author.
        let injected = remote_only_branches_to_inject(&local, &remote, &scope, &["BOB".into()]);
        assert_eq!(
            injected,
            vec![("feature".to_string(), "main".to_string(), 7)]
        );
    }

    #[test]
    fn inject_skips_branch_with_missing_author_when_authors_filter_active() {
        let local = local_state("main", &[("main", None)]);
        // Remote PR present but no author entry recorded ⇒ "can't tell" ⇒ not injected.
        let mut remote = remote_state(&[("feature", "main", 7, "bob")]);
        remote.authors.remove("feature");
        let scope = scope_of(&["main", "feature"]);
        let injected = remote_only_branches_to_inject(&local, &remote, &scope, &["bob".into()]);
        assert!(injected.is_empty());
    }

    #[test]
    fn walk_pr_base_chain_reconstructs_stack_and_stops_at_trunk() {
        // main ← a ← b ← c ; start at leaf `c`.
        let bases: HashMap<&str, &str> = [("c", "b"), ("b", "a"), ("a", "main")]
            .into_iter()
            .collect();
        let chain = walk_pr_base_chain("c", "main", |b| bases.get(b).map(|s| s.to_string()));
        assert_eq!(chain, vec!["c", "b", "a"]);
    }

    #[test]
    fn walk_pr_base_chain_stops_when_no_pr() {
        // `b` has no PR (lookup returns None) ⇒ chain stops after including `b`.
        let bases: HashMap<&str, &str> = [("c", "b")].into_iter().collect();
        let chain = walk_pr_base_chain("c", "main", |b| bases.get(b).map(|s| s.to_string()));
        assert_eq!(chain, vec!["c", "b"]);
    }

    #[test]
    fn walk_pr_base_chain_guards_against_cycles() {
        // c → b → c cycle: each visited once, then the walk terminates.
        let bases: HashMap<&str, &str> = [("c", "b"), ("b", "c")].into_iter().collect();
        let chain = walk_pr_base_chain("c", "main", |b| bases.get(b).map(|s| s.to_string()));
        assert_eq!(chain, vec!["c", "b"]);
    }

    #[test]
    fn walk_pr_base_chain_returns_empty_when_start_is_trunk() {
        let chain = walk_pr_base_chain("main", "main", |_| Some("main".to_string()));
        assert!(chain.is_empty());
    }

    #[test]
    fn resolve_repoint_skips_chain_of_unmounted_ancestors() {
        // Two chains on top of main, all closed-PR → all in unmount_set.
        //
        //   main ← grpc-level-08 ← grpc-level-09 ← grpc-level-10 (leaf)
        //   main ← sadhan/09     ← sadhan/12     ← sadhan/15    ← sadhan/17
        let trunk = "main".to_string();
        let mut branches: HashMap<String, LocalBranch> = HashMap::new();
        branches.insert("grpc-level-08".into(), lb(Some("main")));
        branches.insert("grpc-level-09".into(), lb(Some("grpc-level-08")));
        branches.insert("grpc-level-10".into(), lb(Some("grpc-level-09")));
        branches.insert("sadhan/09".into(), lb(Some("main")));
        branches.insert("sadhan/12".into(), lb(Some("sadhan/09")));
        branches.insert("sadhan/15".into(), lb(Some("sadhan/12")));
        branches.insert("sadhan/17".into(), lb(Some("sadhan/15")));

        let unmount_set: HashSet<String> = branches.keys().cloned().collect();

        for branch in branches.keys() {
            let got = resolve_repoint(branch, &branches, &trunk, &unmount_set);
            assert_eq!(
                got, trunk,
                "branch '{branch}' should resolve past all unmounted ancestors to trunk, got '{got}'"
            );
        }
    }

    #[test]
    fn resolve_repoint_stops_at_first_surviving_ancestor() {
        // main ← A (live) ← B (unmount) ← C (unmount) ← D (leaf, unmount)
        // D, C, B are being unmounted; A survives.
        // Expected: D and C and B all repoint to A.
        let trunk = "main".to_string();
        let mut branches: HashMap<String, LocalBranch> = HashMap::new();
        branches.insert("A".into(), lb(Some("main")));
        branches.insert("B".into(), lb(Some("A")));
        branches.insert("C".into(), lb(Some("B")));
        branches.insert("D".into(), lb(Some("C")));

        let unmount_set: HashSet<String> = ["B", "C", "D"].iter().map(|s| s.to_string()).collect();

        assert_eq!(resolve_repoint("D", &branches, &trunk, &unmount_set), "A");
        assert_eq!(resolve_repoint("C", &branches, &trunk, &unmount_set), "A");
        assert_eq!(resolve_repoint("B", &branches, &trunk, &unmount_set), "A");
    }

    #[test]
    fn resolve_repoint_falls_back_to_trunk_when_no_parent() {
        let trunk = "main".to_string();
        let mut branches: HashMap<String, LocalBranch> = HashMap::new();
        branches.insert("orphan".into(), lb(None));
        let unmount_set: HashSet<String> = HashSet::new();
        assert_eq!(
            resolve_repoint("orphan", &branches, &trunk, &unmount_set),
            trunk
        );
    }

    /// Build a `PullRequest` shaped like one returned from author-based GraphQL discovery: open,
    /// non-fork (head/base repos match), with a synthetic head SHA derived from the branch name.
    fn discovered_pr(head: &str, base: &str, number: u64, author: &str) -> PullRequest {
        use crate::github::{PrBranchRef, PrRepoRef, PrUser};
        let repo = || {
            Some(PrRepoRef {
                full_name: "acme/app".to_string(),
            })
        };
        PullRequest {
            number,
            state: PrState::Open,
            title: format!("PR #{number}"),
            html_url: format!("https://example.test/pr/{number}"),
            base: PrBranchRef {
                ref_name: base.to_string(),
                sha: String::new(),
                repo: repo(),
            },
            head: PrBranchRef {
                ref_name: head.to_string(),
                sha: format!("sha-{head}"),
                repo: repo(),
            },
            user: PrUser {
                login: author.to_string(),
            },
            draft: false,
            merged: false,
            merged_at: None,
            updated_at: "2024-01-01T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn merge_discovered_prs_seeds_scope_and_enables_injection() {
        // Trunk-only tree, empty remote scope — the on-trunk case the feature targets.
        let local = local_state("main", &[("main", None)]);
        let mut remote = remote_state(&[]);
        let mut scope = scope_of(&["main"]);
        let mut seen_shas: HashSet<String> = HashSet::new();

        let discovered = vec![discovered_pr("mine", "main", 42, "wbbradley")];
        merge_discovered_prs(&discovered, &mut scope, &mut remote, &mut seen_shas);

        // The head branch is folded into scope, remote.prs, remote.authors, and seen_shas.
        assert!(scope.contains("mine"));
        assert_eq!(remote.prs.get("mine").unwrap().number, 42);
        assert_eq!(remote.authors.get("mine").unwrap(), "wbbradley");
        assert!(seen_shas.contains("sha-mine"));

        // The author gate now passes by construction ⇒ the PR is injected as a mount tuple.
        let injected =
            remote_only_branches_to_inject(&local, &remote, &scope, &["wbbradley".into()]);
        assert_eq!(injected, vec![("mine".to_string(), "main".to_string(), 42)]);
    }

    #[test]
    fn merge_discovered_prs_non_matching_author_not_injected() {
        // A discovered PR by someone outside the effective filter is seeded into scope but still
        // dropped by the existing author gate — discovery never bypasses `authors_filter`.
        let local = local_state("main", &[("main", None)]);
        let mut remote = remote_state(&[]);
        let mut scope = scope_of(&["main"]);
        let mut seen_shas: HashSet<String> = HashSet::new();

        let discovered = vec![discovered_pr("theirs", "main", 7, "bob")];
        merge_discovered_prs(&discovered, &mut scope, &mut remote, &mut seen_shas);

        let injected =
            remote_only_branches_to_inject(&local, &remote, &scope, &["wbbradley".into()]);
        assert!(injected.is_empty());
    }
}
