//! Tree data computation and flattening for rendering.

use std::collections::{HashMap, HashSet};

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
    pub html_url: String,
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

/// Marks `branch` and every ancestor of `target` (inclusive of `target` itself) in `path`.
/// Returns whether `branch`'s subtree contains `target`.
fn mark_ancestor_path(branch: &Branch, target: &str, path: &mut HashSet<String>) -> bool {
    let on_path = branch.name == target
        || branch
            .branches
            .iter()
            .any(|child| mark_ancestor_path(child, target, path));
    if on_path {
        path.insert(branch.name.clone());
    }
    on_path
}

/// Compute the set of branch names to hide entirely from rendering because their PR author
/// isn't in `display_authors`. A branch is protected from hiding if it is `current_branch`, one
/// of its ancestors, or the tree root — regardless of author. Branches with no PR (no entry in
/// `pr_cache`) are never hidden.
fn compute_hidden_branches(
    tree: &Branch,
    current_branch: &str,
    display_authors: &[String],
    pr_cache: Option<&HashMap<String, PullRequest>>,
    show_all: bool,
) -> HashSet<String> {
    let mut hidden = HashSet::new();
    if show_all || display_authors.is_empty() {
        return hidden;
    }

    let mut protected = HashSet::new();
    mark_ancestor_path(tree, current_branch, &mut protected);
    protected.insert(tree.name.clone()); // defensive: never hide trunk (e.g. stale current_branch)

    mark_hidden(tree, display_authors, pr_cache, &protected, &mut hidden);
    hidden
}

fn mark_hidden(
    branch: &Branch,
    display_authors: &[String],
    pr_cache: Option<&HashMap<String, PullRequest>>,
    protected: &HashSet<String>,
    hidden: &mut HashSet<String>,
) {
    if !protected.contains(&branch.name) {
        let pr_author = pr_cache
            .and_then(|cache| cache.get(&branch.name))
            .map(|pr| pr.user.login.as_str());
        if pr_author.is_some_and(|author| !display_authors.contains(&author.to_string())) {
            hidden.insert(branch.name.clone());
        }
    }
    for child in &branch.branches {
        mark_hidden(child, display_authors, pr_cache, protected, hidden);
    }
}

/// Compute a renderable tree from the branch tree.
#[allow(clippy::too_many_arguments)]
pub fn compute_renderable_tree(
    git_repo: &GitRepo,
    tree: &Branch,
    current_branch: &str,
    verbose: bool,
    pr_cache: Option<&HashMap<String, PullRequest>>,
    display_authors: &[String],
    show_all: bool,
) -> RenderableTree {
    let mut branches = Vec::new();
    let mut current_branch_index = None;
    let hidden = compute_hidden_branches(tree, current_branch, display_authors, pr_cache, show_all);

    flatten_tree(
        git_repo,
        tree,
        None,
        0,
        current_branch,
        verbose,
        pr_cache,
        display_authors,
        &hidden,
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
    hidden: &HashSet<String>,
    result: &mut Vec<RenderableBranch>,
    current_branch_index: &mut Option<usize>,
) {
    let is_current = branch.name == current_branch;
    let is_hidden = hidden.contains(&branch.name);

    if !is_hidden {
        let index = result.len();

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
                html_url: pr.html_url.clone(),
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
    }

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

    // Recursively process children. Hidden branches pass their own depth through unchanged, so
    // a visible descendant renders at the depth it would have if attached directly to the
    // nearest visible ancestor (display-only reparenting; git ancestry via `parent_branch` above
    // is untouched).
    let child_depth = if is_hidden { depth } else { depth + 1 };
    for child in children {
        flatten_tree(
            git_repo,
            child,
            Some(&branch.name),
            child_depth,
            current_branch,
            verbose,
            pr_cache,
            display_authors,
            hidden,
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
        git_repo
            .diff_stats(&base, &status.sha)
            .ok()
            .map(|(adds, dels)| DiffStats {
                additions: adds,
                deletions: dels,
                reliable: is_reliable,
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        github::{PrBranchRef, PrState, PrUser},
        state::StackMethod,
    };

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

    fn make_pr(author: &str) -> PullRequest {
        PullRequest {
            number: 1,
            state: PrState::Open,
            title: "title".to_string(),
            html_url: "https://example.com/pr/1".to_string(),
            base: PrBranchRef {
                ref_name: "main".to_string(),
                sha: "0000000000000000000000000000000000000000".to_string(),
                repo: None,
            },
            head: PrBranchRef {
                ref_name: "head".to_string(),
                sha: "0000000000000000000000000000000000000000".to_string(),
                repo: None,
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

    /// Shared fixture tree:
    ///
    /// ```text
    /// main (no PR)
    /// ├─ alice-1 (PR: alice)
    /// │  └─ bob-1 (PR: bob)              <- current_branch
    /// │     └─ carol-1 (PR: carol)
    /// │        └─ carol-1-child (PR: alice)
    /// └─ dave-1 (no PR)
    ///    └─ eve-1 (PR: eve)
    /// ```
    fn fixture_tree() -> Branch {
        branch(
            "main",
            vec![
                branch(
                    "alice-1",
                    vec![branch(
                        "bob-1",
                        vec![branch("carol-1", vec![branch("carol-1-child", vec![])])],
                    )],
                ),
                branch("dave-1", vec![branch("eve-1", vec![])]),
            ],
        )
    }

    fn fixture_pr_cache() -> HashMap<String, PullRequest> {
        let mut cache = HashMap::new();
        cache.insert("alice-1".to_string(), make_pr("alice"));
        cache.insert("bob-1".to_string(), make_pr("bob"));
        cache.insert("carol-1".to_string(), make_pr("carol"));
        cache.insert("carol-1-child".to_string(), make_pr("alice"));
        cache.insert("eve-1".to_string(), make_pr("eve"));
        cache
    }

    #[test]
    fn hides_branches_with_unlisted_pr_author() {
        let tree = fixture_tree();
        let pr_cache = fixture_pr_cache();
        let display_authors = vec!["alice".to_string()];

        let hidden =
            compute_hidden_branches(&tree, "bob-1", &display_authors, Some(&pr_cache), false);

        let expected: HashSet<String> =
            ["carol-1", "eve-1"].iter().map(|s| s.to_string()).collect();
        assert_eq!(hidden, expected);
    }

    #[test]
    fn protects_current_branch_and_its_ancestors() {
        let tree = fixture_tree();
        let pr_cache = fixture_pr_cache();
        let display_authors = vec!["alice".to_string()];

        let hidden =
            compute_hidden_branches(&tree, "bob-1", &display_authors, Some(&pr_cache), false);

        assert!(!hidden.contains("bob-1"));
        assert!(!hidden.contains("alice-1"));
    }

    #[test]
    fn never_hides_branches_without_a_pr() {
        let tree = fixture_tree();
        let pr_cache = fixture_pr_cache();
        let display_authors = vec!["alice".to_string()];

        let hidden =
            compute_hidden_branches(&tree, "bob-1", &display_authors, Some(&pr_cache), false);

        assert!(!hidden.contains("main"));
        assert!(!hidden.contains("dave-1"));
    }

    #[test]
    fn hidden_branch_does_not_hide_its_own_listed_author_child() {
        let tree = fixture_tree();
        let pr_cache = fixture_pr_cache();
        let display_authors = vec!["alice".to_string()];

        let hidden =
            compute_hidden_branches(&tree, "bob-1", &display_authors, Some(&pr_cache), false);

        assert!(hidden.contains("carol-1"));
        assert!(!hidden.contains("carol-1-child"));
    }

    #[test]
    fn empty_display_authors_hides_nothing() {
        let tree = fixture_tree();
        let pr_cache = fixture_pr_cache();

        let hidden = compute_hidden_branches(&tree, "bob-1", &[], Some(&pr_cache), false);

        assert!(hidden.is_empty());
    }

    #[test]
    fn show_all_disables_hiding() {
        let tree = fixture_tree();
        let pr_cache = fixture_pr_cache();
        let display_authors = vec!["alice".to_string()];

        let hidden =
            compute_hidden_branches(&tree, "bob-1", &display_authors, Some(&pr_cache), true);

        assert!(hidden.is_empty());
    }

    #[test]
    fn tree_root_is_never_hidden() {
        let mut cache = HashMap::new();
        cache.insert("main".to_string(), make_pr("mallory"));
        let tree = branch("main", vec![]);
        let display_authors = vec!["alice".to_string()];

        // "does-not-exist" simulates stale current_branch state that matches nothing in the tree.
        let hidden = compute_hidden_branches(
            &tree,
            "does-not-exist",
            &display_authors,
            Some(&cache),
            false,
        );

        assert!(!hidden.contains(&tree.name));
    }
}
