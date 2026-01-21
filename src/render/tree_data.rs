//! Tree data computation and flattening for rendering.

use std::collections::HashMap;

use crate::{
    git::get_local_status,
    git2_ops::GitRepo,
    github::{PrDisplayState, PullRequest},
    state::Branch,
};

/// Status information for a branch's relationship to its parent.
#[derive(Debug, Clone)]
pub struct BranchRenderStatus {
    pub exists: bool,
    pub is_descendent: bool,
    pub sha: String,
    pub parent_branch: String,
    pub upstream_synced: Option<bool>,
    pub upstream_name: Option<String>,
}

/// PR information for rendering.
#[derive(Debug, Clone)]
pub struct PrRenderInfo {
    pub number: u64,
    pub state: PrDisplayState,
    pub author: String,
}

/// Diff statistics (additions, deletions).
#[derive(Debug, Clone, Default)]
pub struct DiffStats {
    pub additions: usize,
    pub deletions: usize,
    /// Whether the diff stats are from a reliable source (LKG parent vs merge-base guess)
    pub reliable: bool,
}

/// Local working tree status (for current branch only).
#[derive(Debug, Clone, Default)]
pub struct LocalStatus {
    pub staged: usize,
    pub unstaged: usize,
    pub untracked: usize,
}

impl LocalStatus {
    pub fn is_clean(&self) -> bool {
        self.staged == 0 && self.unstaged == 0 && self.untracked == 0
    }
}

/// Verbose details for a branch (shown in verbose mode).
#[derive(Debug, Clone)]
pub struct VerboseDetails {
    pub stacked_on: String,
    pub is_diverged: bool,
    pub upstream_status: Option<(String, bool)>, // (name, synced)
    pub lkg_parent: Option<String>,
    pub stack_method: String,
}

/// A flattened branch entry for rendering (shared by CLI and TUI).
#[derive(Debug, Clone)]
pub struct RenderableBranch {
    /// The branch name.
    pub name: String,
    /// Depth in the tree (for indentation).
    pub depth: usize,
    /// Whether this is the currently checked-out branch.
    pub is_current: bool,
    /// Whether this branch should be rendered dimmed (filtered by display_authors).
    pub is_dimmed: bool,
    /// Whether this branch only exists on remote (not locally).
    pub is_remote_only: bool,
    /// Branch status relative to parent.
    pub status: Option<BranchRenderStatus>,
    /// Diff statistics.
    pub diff_stats: Option<DiffStats>,
    /// Local working tree status (only for current branch).
    pub local_status: Option<LocalStatus>,
    /// PR information if available.
    pub pr_info: Option<PrRenderInfo>,
    /// First line of branch note (if any).
    pub note_preview: Option<String>,
    /// Verbose details (populated when verbose mode is requested).
    pub verbose: Option<VerboseDetails>,
    /// Index in the flattened list (for TUI cursor navigation).
    pub index: usize,
}

/// A flattened tree ready for rendering.
#[derive(Debug, Clone)]
pub struct RenderableTree {
    /// Flattened list of branches in display order.
    pub branches: Vec<RenderableBranch>,
    /// Index of the current branch (if in the tree).
    pub current_branch_index: Option<usize>,
}

/// Check if a branch subtree contains the target branch or a display_author PR.
/// Returns (has_target_branch, has_display_author_pr).
fn subtree_contains(
    branch: &Branch,
    target_branch: &str,
    display_authors: &[String],
    pr_cache: Option<&HashMap<String, PullRequest>>,
) -> (bool, bool) {
    let is_target = branch.name == target_branch;
    let has_author = pr_cache
        .and_then(|cache| cache.get(&branch.name))
        .map(|pr| display_authors.contains(&pr.user.login))
        .unwrap_or(false);

    let (child_has_target, child_has_author) = branch
        .branches
        .iter()
        .map(|b| subtree_contains(b, target_branch, display_authors, pr_cache))
        .fold((false, false), |(t1, a1), (t2, a2)| (t1 || t2, a1 || a2));

    (
        is_target || child_has_target,
        has_author || child_has_author,
    )
}

/// Compute a renderable tree from the branch tree.
pub fn compute_renderable_tree(
    git_repo: &GitRepo,
    tree: &Branch,
    current_branch: &str,
    verbose: bool,
    pr_cache: Option<&HashMap<String, PullRequest>>,
    display_authors: &[String],
) -> RenderableTree {
    let mut branches = Vec::new();
    let mut current_branch_index = None;

    flatten_tree(
        git_repo,
        tree,
        None,
        0,
        current_branch,
        verbose,
        pr_cache,
        display_authors,
        &mut branches,
        &mut current_branch_index,
    );

    RenderableTree {
        branches,
        current_branch_index,
    }
}

#[allow(clippy::too_many_arguments)]
fn flatten_tree(
    git_repo: &GitRepo,
    branch: &Branch,
    parent_branch: Option<&str>,
    depth: usize,
    current_branch: &str,
    verbose: bool,
    pr_cache: Option<&HashMap<String, PullRequest>>,
    display_authors: &[String],
    result: &mut Vec<RenderableBranch>,
    current_branch_index: &mut Option<usize>,
) {
    let index = result.len();
    let is_current = branch.name == current_branch;

    if is_current {
        *current_branch_index = Some(index);
    }

    // Check if this branch should be dimmed (filtered by display_authors)
    let pr_author = pr_cache
        .and_then(|cache| cache.get(&branch.name))
        .map(|pr| pr.user.login.as_str());
    let is_dimmed = if display_authors.is_empty() {
        false
    } else {
        pr_author.is_some_and(|author| !display_authors.contains(&author.to_string()))
    };

    // Check if branch is remote-only (not local)
    let is_remote_only = !git_repo.branch_exists(&branch.name);

    // Get branch status
    let status = git_repo
        .branch_status(parent_branch, &branch.name)
        .ok()
        .map(|bs| BranchRenderStatus {
            exists: bs.exists,
            is_descendent: bs.is_descendent,
            sha: bs.sha,
            parent_branch: bs.parent_branch,
            upstream_synced: bs.upstream_status.as_ref().map(|us| us.synced),
            upstream_name: bs.upstream_status.map(|us| us.symbolic_name),
        });

    // Compute diff stats
    let diff_stats = if let Some(ref status) = status {
        compute_diff_stats(git_repo, branch, status)
    } else {
        None
    };

    // Get local status (only for current branch)
    let local_status = if is_current {
        get_local_status()
            .ok()
            .filter(|s| !s.is_clean())
            .map(|s| LocalStatus {
                staged: s.staged,
                unstaged: s.unstaged,
                untracked: s.untracked,
            })
    } else {
        None
    };

    // Get PR info
    let pr_info = pr_cache
        .and_then(|cache| cache.get(&branch.name))
        .map(|pr| PrRenderInfo {
            number: pr.number,
            state: pr.display_state(),
            author: pr.user.login.clone(),
        });

    // Get note preview
    let note_preview = branch
        .note
        .as_ref()
        .and_then(|note| note.lines().next())
        .map(|s| s.to_string());

    // Compute verbose details if requested
    let verbose_details = if verbose {
        status.as_ref().map(|s| VerboseDetails {
            stacked_on: s.parent_branch.clone(),
            is_diverged: !s.is_descendent,
            upstream_status: s
                .upstream_name
                .as_ref()
                .map(|name| (name.clone(), s.upstream_synced.unwrap_or(false))),
            lkg_parent: branch.lkg_parent.as_ref().map(|s| s[..8].to_string()),
            stack_method: match branch.stack_method {
                crate::state::StackMethod::ApplyMerge => "apply-merge".to_string(),
                crate::state::StackMethod::Merge => "merge".to_string(),
            },
        })
    } else {
        None
    };

    result.push(RenderableBranch {
        name: branch.name.clone(),
        depth,
        is_current,
        is_dimmed,
        is_remote_only,
        status,
        diff_stats,
        local_status,
        pr_info,
        note_preview,
        verbose: verbose_details,
        index,
    });

    // Sort children: current subtree first, display_authors second, alphabetical third
    let mut children: Vec<&Branch> = branch.branches.iter().collect();

    // Pre-compute subtree properties for sorting
    let subtree_cache: HashMap<&str, (bool, bool)> = children
        .iter()
        .map(|b| {
            (
                b.name.as_str(),
                subtree_contains(b, current_branch, display_authors, pr_cache),
            )
        })
        .collect();

    children.sort_by(|a, b| {
        let (a_has_current, a_has_author) = subtree_cache
            .get(a.name.as_str())
            .copied()
            .unwrap_or((false, false));
        let (b_has_current, b_has_author) = subtree_cache
            .get(b.name.as_str())
            .copied()
            .unwrap_or((false, false));

        // Priority 1: subtree contains current branch
        match (a_has_current, b_has_current) {
            (true, false) => return std::cmp::Ordering::Less,
            (false, true) => return std::cmp::Ordering::Greater,
            _ => {}
        }
        // Priority 2: subtree contains display_author PR
        match (a_has_author, b_has_author) {
            (true, false) => return std::cmp::Ordering::Less,
            (false, true) => return std::cmp::Ordering::Greater,
            _ => {}
        }
        // Priority 3: alphabetical
        a.name.cmp(&b.name)
    });

    // Recursively process children
    for child in children {
        flatten_tree(
            git_repo,
            child,
            Some(&branch.name),
            depth + 1,
            current_branch,
            verbose,
            pr_cache,
            display_authors,
            result,
            current_branch_index,
        );
    }
}

fn compute_diff_stats(
    git_repo: &GitRepo,
    branch: &Branch,
    status: &BranchRenderStatus,
) -> Option<DiffStats> {
    // Determine base ref and whether it's reliable
    let (base_ref, is_reliable) = if let Some(lkg) = branch.lkg_parent.as_deref() {
        (Some(lkg.to_string()), true)
    } else if let Ok(merge_base) = git_repo.merge_base(&status.parent_branch, &status.sha) {
        (Some(merge_base), false)
    } else {
        (None, false)
    };

    base_ref.and_then(|base| {
        git_repo.diff_stats(&base, &status.sha).ok().map(|(adds, dels)| DiffStats {
            additions: adds,
            deletions: dels,
            reliable: is_reliable,
        })
    })
}
