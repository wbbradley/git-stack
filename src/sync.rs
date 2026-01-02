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
        CreatePrRequest, GitHubClient, PrState, PullRequest, RepoIdentifier, UpdatePrRequest,
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
    /// Map of head branch name -> PR info
    pub prs: HashMap<String, RemotePr>,
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
            state: if pr.merged {
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

#[derive(Debug, Clone, Copy)]
pub struct SyncOptions {
    /// Only push local changes to remote (no pull)
    pub push_only: bool,
    /// Only pull remote changes to local (no push)
    pub pull_only: bool,
    /// Show plan without applying
    pub dry_run: bool,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            push_only: false,
            pull_only: false,
            dry_run: false,
        }
    }
}

// ============== Implementation ==============

/// Main sync entry point
pub fn sync(
    git_repo: &GitRepo,
    state: &mut State,
    repo: &str,
    options: SyncOptions,
) -> Result<()> {
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
    let target_state = build_target_state(&local_state, &remote_state);

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
    let prs_map = client.list_open_prs(repo_id).map_err(|e| anyhow!("{}", e))?;

    let prs: HashMap<String, RemotePr> = prs_map
        .iter()
        .map(|(branch, pr)| (branch.clone(), RemotePr::from(pr)))
        .collect();

    Ok(RemoteState { prs })
}

// ============== Stage 2: Model Functions ==============

/// Build target state by merging local and remote
fn build_target_state(local: &LocalState, remote: &RemoteState) -> TargetState {
    let mut branches = HashMap::new();

    // Process all local branches (local is authoritative for structure)
    for (name, local_branch) in &local.branches {
        let pr_number = remote
            .prs
            .get(name)
            .map(|pr| pr.number)
            .or(local_branch.pr_number);

        let expected_pr_base = local_branch.parent.clone();

        branches.insert(
            name.clone(),
            TargetBranch {
                parent: local_branch.parent.clone(),
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
            // The parent should be the PR's base
            branches.insert(
                branch_name.clone(),
                TargetBranch {
                    parent: Some(pr.base.clone()),
                    pr_number: Some(pr.number),
                    expected_pr_base: Some(pr.base.clone()),
                    exists_locally: false, // We'll need to checkout
                    pushed_to_remote: true,
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
                    local_changes.push(LocalChange::MountBranch {
                        name: branch_name.clone(),
                        parent: parent.clone(),
                    });
                }

                // Check if we need to checkout the branch
                if !target_branch.exists_locally {
                    local_changes.push(LocalChange::CheckoutBranch {
                        name: branch_name.clone(),
                    });
                }
            }

            // Check if we need to update cached PR number
            if let Some(local_branch) = local_branch {
                if let Some(pr_number) = target_branch.pr_number {
                    if local_branch.pr_number != Some(pr_number) {
                        local_changes.push(LocalChange::UpdatePrNumber {
                            branch: branch_name.clone(),
                            pr_number,
                        });
                    }
                }
            }
        }
    }

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
                // No PR but branch is pushed - could create PR
                (None, Some(expected_base)) => {
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
            println!(
                "  Mounting '{}' on '{}'",
                name.yellow(),
                parent.green()
            );
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
            // TODO: Implement unmount with child repointing
            state.delete_branch(repo, name)?;
        }
        LocalChange::UpdatePrNumber { branch, pr_number } => {
            println!(
                "  Updating PR# for '{}' → #{}",
                branch.yellow(),
                pr_number.to_string().green()
            );
            if let Some(tree) = state.get_tree_mut(repo) {
                if let Some(b) = find_branch_by_name_mut(tree, branch) {
                    b.pr_number = Some(*pr_number);
                }
            }
        }
        LocalChange::CheckoutBranch { name } => {
            println!("  Checking out '{}'", name.yellow());
            // Checkout the branch from remote
            run_git(&["checkout", "-b", name, &format!("{}/{}", DEFAULT_REMOTE, name)])?;
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
            let commit_title = run_git(&["log", "--no-show-signature", "--format=%s", "-1", branch])
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
            if let Some(tree) = state.get_tree_mut(repo) {
                if let Some(b) = find_branch_by_name_mut(tree, branch) {
                    b.pr_number = Some(pr.number);
                }
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
