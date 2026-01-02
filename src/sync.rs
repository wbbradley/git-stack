//! Unified sync module for bidirectional state synchronization.
//!
//! This module implements a Terraform-style staged workflow:
//! 1. Read: Gather current local and remote state
//! 2. Model: Build target state in memory
//! 3. Diff: Compute changes needed for each side
//! 4. Validate: Ensure changes are non-lossy
//! 5. Apply: Execute changes if safe

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result, anyhow, bail};
use colored::Colorize;

use crate::{
    git::{git_trunk, run_git},
    git2_ops::{DEFAULT_REMOTE, GitRepo},
    github::{
        CreatePrRequest,
        GitHubClient,
        PrState,
        PullRequest,
        RepoIdentifier,
        UpdatePrRequest,
        get_repo_identifier,
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
    /// Whether the branch exists as a local git ref
    pub exists_locally: bool,
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
    /// Whether branch exists locally
    pub exists_locally: bool,
    /// Whether branch is pushed to remote
    pub pushed_to_remote: bool,
}

// ============== Stage 3: Change Types ==============

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
    /// Checkout a branch from remote (branch exists on remote but not locally)
    CheckoutBranch { name: String },
}

/// Changes to apply to remote state (GitHub)
#[derive(Debug, Clone)]
pub enum RemoteChange {
    /// Create a new PR
    CreatePr {
        branch: String,
        base: String,
        title: String,
    },
    /// Retarget a PR to a different base
    RetargetPr {
        number: u64,
        branch: String,
        old_base: String,
        new_base: String,
    },
}

// ============== Stage 4: Sync Plan ==============

/// Complete sync plan with all changes
#[derive(Debug)]
pub struct SyncPlan {
    pub local_changes: Vec<LocalChange>,
    pub remote_changes: Vec<RemoteChange>,
    pub warnings: Vec<String>,
}

impl SyncPlan {
    pub fn is_empty(&self) -> bool {
        self.local_changes.is_empty() && self.remote_changes.is_empty()
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
    // Get repo identifier for GitHub API
    let repo_id = get_repo_identifier(git_repo)?;
    let client = GitHubClient::from_env(&repo_id)?;

    // Stage 1: Read current state
    println!("Reading local state...");
    let local_state = read_local_state(git_repo, state, repo)?;

    println!("Reading remote state...");
    let remote_state = read_remote_state(&client, &repo_id)?;

    // Stage 2: Build target state
    println!("Building target model...");
    let target_state = build_target_state(git_repo, &local_state, &remote_state);

    // Stage 3: Compute diffs
    let plan = compute_sync_plan(&local_state, &remote_state, &target_state, &options);

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
    } else {
        println!("\nApplying changes...");
        apply_plan(git_repo, state, repo, &client, &repo_id, &plan)?;
        println!("\n{}", "Sync complete!".green().bold());
    }

    Ok(())
}

// ============== Stage 1: Read Functions ==============

/// Read current local state from git-stack and git
fn read_local_state(git_repo: &GitRepo, state: &State, repo: &str) -> Result<LocalState> {
    let trunk = git_trunk(git_repo)?;
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
    let exists_locally = git_repo.branch_exists(&branch.name);
    let remote_ref = format!("{}/{}", DEFAULT_REMOTE, branch.name);
    let pushed_to_remote = git_repo.ref_exists(&remote_ref);

    branches.insert(
        branch.name.clone(),
        LocalBranch {
            parent: parent.map(|s| s.to_string()),
            pr_number: branch.pr_number,
            exists_locally,
            pushed_to_remote,
        },
    );

    for child in &branch.branches {
        collect_local_branches(git_repo, child, Some(&branch.name), branches);
    }
}

/// Read current remote state from GitHub
fn read_remote_state(client: &GitHubClient, repo_id: &RepoIdentifier) -> Result<RemoteState> {
    // Fetch open PRs
    let open_prs_map = client
        .list_open_prs(repo_id)
        .map_err(|e| anyhow!("{}", e))?;

    let prs: HashMap<String, RemotePr> = open_prs_map
        .iter()
        .map(|(branch, pr)| (branch.clone(), RemotePr::from(pr)))
        .collect();

    // Fetch closed PRs (includes merged)
    let closed_prs_map = client
        .list_prs(repo_id, "closed")
        .map_err(|e| anyhow!("{}", e))?;

    let closed_prs: HashMap<String, RemotePr> = closed_prs_map
        .iter()
        .map(|(branch, pr)| (branch.clone(), RemotePr::from(pr)))
        .collect();

    Ok(RemoteState { prs, closed_prs })
}

// ============== Stage 2: Model Functions ==============

/// Build target state by merging local and remote
fn build_target_state(git_repo: &GitRepo, local: &LocalState, remote: &RemoteState) -> TargetState {
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
                exists_locally: local_branch.exists_locally,
                pushed_to_remote: local_branch.pushed_to_remote,
            },
        );
    }

    // Process remote PRs that aren't in local tree (for pull)
    for (branch_name, pr) in &remote.prs {
        if !branches.contains_key(branch_name) {
            // This PR's branch is not in our local tree
            // Check if the branch exists locally (even if not in tree)
            let exists_locally = git_repo.branch_exists(branch_name);
            let remote_ref = format!("{}/{}", DEFAULT_REMOTE, branch_name);
            let pushed_to_remote = git_repo.ref_exists(&remote_ref);

            branches.insert(
                branch_name.clone(),
                TargetBranch {
                    parent: Some(pr.base.clone()),
                    pr_number: Some(pr.number),
                    expected_pr_base: Some(pr.base.clone()),
                    exists_locally,
                    pushed_to_remote,
                },
            );
        }
    }

    TargetState {
        branches,
        trunk: local.trunk.clone(),
    }
}

// ============== Stage 3: Diff Functions ==============

/// Compute the sync plan by diffing current state against target
fn compute_sync_plan(
    local: &LocalState,
    remote: &RemoteState,
    target: &TargetState,
    options: &SyncOptions,
) -> SyncPlan {
    let mut local_changes = Vec::new();
    let mut remote_changes = Vec::new();
    let warnings = Vec::new();

    // Compute local changes (pull direction)
    if !options.push_only {
        // Collect branches that need to be mounted, then topologically sort them
        let mut branches_to_mount: Vec<(String, String)> = Vec::new(); // (branch, parent)
        let mut branches_to_checkout: HashSet<String> = HashSet::new();
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

                // Check if we need to checkout the branch
                if !target_branch.exists_locally {
                    branches_to_checkout.insert(branch_name.clone());
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

        // Topologically sort branches to mount (parents before children)
        let sorted_branches = topological_sort_branches(&branches_to_mount, &local.trunk);

        // Add mount and checkout changes in topological order
        for (branch_name, parent) in sorted_branches {
            // Checkout first if needed (need the ref before mounting)
            if branches_to_checkout.contains(&branch_name) {
                local_changes.push(LocalChange::CheckoutBranch {
                    name: branch_name.clone(),
                });
            }
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

    // Detect merged PRs and handle unmounting (pull direction)
    // This runs regardless of push_only/pull_only since it's about reconciling state
    let mut branches_to_unmount: Vec<(String, String)> = Vec::new(); // (branch, repoint_to)

    tracing::debug!(
        "Checking for merged PRs. Local branches: {:?}, Closed PRs: {:?}",
        local.branches.keys().collect::<Vec<_>>(),
        remote.closed_prs.keys().collect::<Vec<_>>()
    );

    for (branch_name, local_branch) in &local.branches {
        // Skip trunk
        if branch_name == &local.trunk {
            continue;
        }

        // Check if this branch has a merged/closed PR but no open PR
        if !remote.prs.contains_key(branch_name) {
            if let Some(closed_pr) = remote.closed_prs.get(branch_name) {
                tracing::debug!(
                    "Branch '{}' has closed PR #{} with state {:?}",
                    branch_name,
                    closed_pr.number,
                    closed_pr.state
                );
                if closed_pr.state == RemotePrState::Merged {
                    // This branch's PR was merged - it should be unmounted
                    // Children should be repointed to this branch's parent
                    let repoint_to = local_branch
                        .parent
                        .clone()
                        .unwrap_or_else(|| local.trunk.clone());
                    branches_to_unmount.push((branch_name.clone(), repoint_to));
                }
            }
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
                    remote_changes.push(RemoteChange::RetargetPr {
                        number: pr.number,
                        branch: branch_name.clone(),
                        old_base: pr.base.clone(),
                        new_base: expected_base.clone(),
                    });
                }
                // No open PR but branch is pushed - could create PR
                // But first check if there's a merged/closed PR
                (None, Some(expected_base)) => {
                    // Skip if this branch had a merged or closed PR
                    if remote.closed_prs.contains_key(branch_name) {
                        continue;
                    }
                    // Generate title from branch name or first commit
                    let title = branch_name.clone();
                    remote_changes.push(RemoteChange::CreatePr {
                        branch: branch_name.clone(),
                        base: expected_base.clone(),
                        title,
                    });
                }
                _ => {}
            }
        }
    }

    SyncPlan {
        local_changes,
        remote_changes,
        warnings,
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
    // Apply local changes first (checkout, mount, update pr_number)
    for change in &plan.local_changes {
        apply_local_change(git_repo, state, repo, change)?;
    }

    // Save state after local changes
    state.save_state()?;

    // Apply remote changes (create PRs, retarget)
    for change in &plan.remote_changes {
        apply_remote_change(git_repo, state, repo, client, repo_id, change)?;
    }

    // Save state again if PR numbers were updated
    state.save_state()?;

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
            // First, find all children of this branch and repoint them
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
                state.mount(git_repo, repo, &child, Some(repoint_children_to.clone()))?;
            }

            // Now delete the branch from the tree
            state.delete_branch(repo, name)?;
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
        LocalChange::CheckoutBranch { name } => {
            println!("  Checking out '{}'", name.yellow());
            // Check if branch already exists locally
            if git_repo.branch_exists(name) {
                // Branch exists, just checkout
                run_git(&["checkout", name])?;
            } else {
                // Checkout the branch from remote
                run_git(&[
                    "checkout",
                    "-b",
                    name,
                    &format!("{}/{}", DEFAULT_REMOTE, name),
                ])?;
            }
        }
    }
    Ok(())
}

/// Apply a single remote change
fn apply_remote_change(
    git_repo: &GitRepo,
    state: &mut State,
    repo: &str,
    client: &GitHubClient,
    repo_id: &RepoIdentifier,
    change: &RemoteChange,
) -> Result<()> {
    match change {
        RemoteChange::CreatePr {
            branch,
            base,
            title,
        } => {
            println!(
                "  Creating PR for '{}' (base: '{}')",
                branch.yellow(),
                base.green()
            );

            // Get a better title from the first commit
            // Use --no-show-signature to avoid GPG signature output polluting the title
            let commit_title =
                run_git(&["log", "--no-show-signature", "--format=%s", "-1", branch])
                    .ok()
                    .and_then(|r| r.output())
                    .unwrap_or_else(|| title.clone());

            let pr = client
                .create_pr(
                    repo_id,
                    CreatePrRequest {
                        title: &commit_title,
                        body: "",
                        head: branch,
                        base,
                        draft: Some(true),
                    },
                )
                .map_err(|e| anyhow!("{}", e))?;

            println!(
                "    Created PR #{}: {}",
                pr.number.to_string().green(),
                pr.html_url.blue()
            );

            // Update the cached PR number
            if let Some(tree) = state.get_tree_mut(repo)
                && let Some(b) = find_branch_by_name_mut(tree, branch)
            {
                b.pr_number = Some(pr.number);
            }
        }
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
                LocalChange::CheckoutBranch { name } => {
                    println!("    - Checkout '{}'", name.yellow());
                }
            }
        }
    }

    if !plan.remote_changes.is_empty() {
        println!("  Remote changes:");
        for change in &plan.remote_changes {
            match change {
                RemoteChange::CreatePr { branch, base, .. } => {
                    println!(
                        "    - Create PR for '{}' (base: '{}')",
                        branch.yellow(),
                        base.green()
                    );
                }
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
