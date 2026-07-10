//! Tree data computation and flattening for rendering.

use std::collections::{HashMap, HashSet};

use crate::{
    git::get_local_status,
    git2_ops::GitRepo,
    github::{PrDisplayState, PullRequest},
    state::Branch,
};

/// Memoization of diff-stat results within a single render walk, keyed by
/// `(base_sha, head_sha)`. Value is the raw `(additions, deletions)` result of
/// `git_repo.diff_stats`, or `None` when that call failed (also cached so a failing
/// pair isn't retried per branch). The `reliable` flag is intentionally *not* keyed —
/// it depends on how the base was chosen (LKG vs merge-base), not on the sha pair — so
/// it stays computed per-branch outside the cache.
type DiffStatsCache = HashMap<(String, String), Option<(usize, usize)>>;

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
    /// Whether this branch should be rendered dimmed (filtered by authors_filter).
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

/// Check if a branch subtree contains the target branch or a PR by a filtered author.
/// Returns (has_target_branch, has_filtered_author_pr).
fn subtree_contains(
    branch: &Branch,
    target_branch: &str,
    authors_filter: &[String],
    pr_authors: &HashMap<String, String>,
) -> (bool, bool) {
    let is_target = branch.name == target_branch;
    let has_author = pr_authors
        .get(&branch.name)
        .map(|author| crate::github::author_in_filter(authors_filter, author))
        .unwrap_or(false);

    let (child_has_target, child_has_author) = branch
        .branches
        .iter()
        .map(|b| subtree_contains(b, target_branch, authors_filter, pr_authors))
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
/// isn't in `authors_filter`. A branch is protected from hiding if it is `current_branch`, one
/// of its ancestors, or the tree root — regardless of author. Branches with no PR (no entry in
/// `pr_authors`) are never hidden.
///
/// `pr_authors` is expected to span open, closed, and merged PRs (unlike the open-only
/// `pr_cache` used elsewhere for rendering PR badges) — otherwise a branch whose PR was already
/// merged or closed by someone else would look indistinguishable from a branch that never had a
/// PR, and would wrongly stay visible.
pub(crate) fn compute_hidden_branches(
    tree: &Branch,
    current_branch: &str,
    authors_filter: &[String],
    pr_authors: &HashMap<String, String>,
    show_all: bool,
) -> HashSet<String> {
    let mut hidden = HashSet::new();
    if show_all || authors_filter.is_empty() {
        return hidden;
    }

    let protected = compute_protected_branches(tree, current_branch);
    mark_hidden(tree, authors_filter, pr_authors, &protected, &mut hidden);
    hidden
}

/// Compute the set of branches protected from author-based hiding: `current_branch`, all of its
/// ancestors up to the root, and the tree root itself. Their author is never consulted by hiding,
/// so callers can also skip resolving it (e.g. the commit-author fallback in main.rs).
pub fn compute_protected_branches(tree: &Branch, current_branch: &str) -> HashSet<String> {
    let mut protected = HashSet::new();
    mark_ancestor_path(tree, current_branch, &mut protected);
    protected.insert(tree.name.clone()); // defensive: never hide trunk (e.g. stale current_branch)
    protected
}

fn mark_hidden(
    branch: &Branch,
    authors_filter: &[String],
    pr_authors: &HashMap<String, String>,
    protected: &HashSet<String>,
    hidden: &mut HashSet<String>,
) {
    if !protected.contains(&branch.name) {
        let pr_author = pr_authors.get(&branch.name).map(|s| s.as_str());
        if pr_author.is_some_and(|author| !crate::github::author_in_filter(authors_filter, author))
        {
            hidden.insert(branch.name.clone());
        }
    }
    for child in &branch.branches {
        mark_hidden(child, authors_filter, pr_authors, protected, hidden);
    }
}

/// Compute a renderable tree from the branch tree. PR badge info (`pr_info`) is not populated
/// here — call `apply_pr_cache` afterward. This split lets callers overlap the PR fetch (network)
/// with this local git walk when `pr_authors` doesn't depend on the fetch (see
/// `build_renderable_tree` in main.rs).
#[allow(clippy::too_many_arguments)]
pub fn compute_renderable_tree(
    git_repo: &GitRepo,
    tree: &Branch,
    current_branch: &str,
    verbose: bool,
    authors_filter: &[String],
    pr_authors: &HashMap<String, String>,
    show_all: bool,
) -> RenderableTree {
    let mut branches = Vec::new();
    let mut current_branch_index = None;
    let hidden =
        compute_hidden_branches(tree, current_branch, authors_filter, pr_authors, show_all);
    let mut diff_cache = DiffStatsCache::new();

    flatten_tree(
        git_repo,
        tree,
        None,
        0,
        current_branch,
        verbose,
        authors_filter,
        pr_authors,
        &hidden,
        &mut branches,
        &mut current_branch_index,
        &mut diff_cache,
    );

    RenderableTree {
        branches,
        current_branch_index,
    }
}

/// Populate PR badge info (`pr_info`) on an already-computed tree, by branch-name lookup. Split
/// out from `flatten_tree` so the PR fetch (network) and the local git walk can run concurrently
/// when hiding/dimming don't need the fetch's author data first (see `build_renderable_tree` in
/// main.rs).
pub fn apply_pr_cache(tree: &mut RenderableTree, pr_cache: Option<&HashMap<String, PullRequest>>) {
    let Some(cache) = pr_cache else { return };
    for branch in &mut tree.branches {
        branch.pr_info = cache.get(&branch.name).map(|pr| PrRenderInfo {
            number: pr.number,
            state: pr.display_state(),
            author: pr.user.login.clone(),
            html_url: pr.html_url.clone(),
        });
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
    authors_filter: &[String],
    pr_authors: &HashMap<String, String>,
    hidden: &HashSet<String>,
    result: &mut Vec<RenderableBranch>,
    current_branch_index: &mut Option<usize>,
    cache: &mut DiffStatsCache,
) {
    let is_current = branch.name == current_branch;
    let is_hidden = hidden.contains(&branch.name);

    if !is_hidden {
        let index = result.len();

        if is_current {
            *current_branch_index = Some(index);
        }

        // Check if this branch should be dimmed (filtered by authors_filter)
        let pr_author = pr_authors.get(&branch.name).map(|s| s.as_str());
        let is_dimmed = if authors_filter.is_empty() {
            false
        } else {
            pr_author.is_some_and(|author| !crate::github::author_in_filter(authors_filter, author))
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
            compute_diff_stats(git_repo, branch, status, cache)
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

        // PR badge info is filled in afterward by `apply_pr_cache`.
        let pr_info = None;

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

    // Sort children: current subtree first, authors_filter second, alphabetical third
    let mut children: Vec<&Branch> = branch.branches.iter().collect();

    // Pre-compute subtree properties for sorting
    let subtree_cache: HashMap<&str, (bool, bool)> = children
        .iter()
        .map(|b| {
            (
                b.name.as_str(),
                subtree_contains(b, current_branch, authors_filter, pr_authors),
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
        // Priority 2: subtree contains a filtered-author PR
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
            authors_filter,
            pr_authors,
            hidden,
            result,
            current_branch_index,
            cache,
        );
    }
}

/// Return the cached diff-stat result for `(base, head)`, computing and storing it via
/// `compute` on the first request for that pair.
fn memoized_diff_stats(
    cache: &mut DiffStatsCache,
    base: &str,
    head: &str,
    compute: impl FnOnce() -> Option<(usize, usize)>,
) -> Option<(usize, usize)> {
    if let Some(&cached) = cache.get(&(base.to_string(), head.to_string())) {
        return cached;
    }
    let result = compute();
    cache.insert((base.to_string(), head.to_string()), result);
    result
}

fn compute_diff_stats(
    git_repo: &GitRepo,
    branch: &Branch,
    status: &BranchRenderStatus,
    cache: &mut DiffStatsCache,
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
        memoized_diff_stats(cache, &base, &status.sha, || {
            git_repo.diff_stats(&base, &status.sha).ok()
        })
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
    use crate::state::StackMethod;

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

    /// The `pr_authors` map is state-agnostic by design: the caller (`main.rs`) merges authors
    /// from open, closed, and merged PRs before hiding decisions are made, so a branch whose PR
    /// was merged by an unlisted author is hidden exactly like one with a still-open PR from
    /// that author. These fixtures don't distinguish PR state for that reason.
    fn fixture_pr_authors() -> HashMap<String, String> {
        let mut authors = HashMap::new();
        authors.insert("alice-1".to_string(), "alice".to_string());
        authors.insert("bob-1".to_string(), "bob".to_string());
        authors.insert("carol-1".to_string(), "carol".to_string());
        authors.insert("carol-1-child".to_string(), "alice".to_string());
        authors.insert("eve-1".to_string(), "eve".to_string());
        authors
    }

    #[test]
    fn compute_protected_branches_returns_current_ancestors_and_root() {
        let tree = fixture_tree();
        // bob-1's ancestor path up to the root is {bob-1, alice-1, main}; carol-1 and its
        // descendants are below bob-1 and dave-1/eve-1 are on a different branch, so neither is
        // protected.
        let protected = compute_protected_branches(&tree, "bob-1");
        let expected: HashSet<String> = ["bob-1", "alice-1", "main"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(protected, expected);
    }

    #[test]
    fn compute_protected_branches_includes_root_when_current_is_stale() {
        let tree = fixture_tree();
        // A stale/absent current_branch resolves no ancestor path, but the root is always
        // protected defensively.
        let protected = compute_protected_branches(&tree, "gone-branch");
        let expected: HashSet<String> = ["main"].iter().map(|s| s.to_string()).collect();
        assert_eq!(protected, expected);
    }

    #[test]
    fn hides_branches_with_unlisted_pr_author() {
        let tree = fixture_tree();
        let pr_authors = fixture_pr_authors();
        let authors_filter = vec!["alice".to_string()];

        let hidden = compute_hidden_branches(&tree, "bob-1", &authors_filter, &pr_authors, false);

        let expected: HashSet<String> =
            ["carol-1", "eve-1"].iter().map(|s| s.to_string()).collect();
        assert_eq!(hidden, expected);
    }

    #[test]
    fn protects_current_branch_and_its_ancestors() {
        let tree = fixture_tree();
        let pr_authors = fixture_pr_authors();
        let authors_filter = vec!["alice".to_string()];

        let hidden = compute_hidden_branches(&tree, "bob-1", &authors_filter, &pr_authors, false);

        assert!(!hidden.contains("bob-1"));
        assert!(!hidden.contains("alice-1"));
    }

    #[test]
    fn never_hides_branches_without_a_pr() {
        let tree = fixture_tree();
        let pr_authors = fixture_pr_authors();
        let authors_filter = vec!["alice".to_string()];

        let hidden = compute_hidden_branches(&tree, "bob-1", &authors_filter, &pr_authors, false);

        assert!(!hidden.contains("main"));
        assert!(!hidden.contains("dave-1"));
    }

    #[test]
    fn hidden_branch_does_not_hide_its_own_listed_author_child() {
        let tree = fixture_tree();
        let pr_authors = fixture_pr_authors();
        let authors_filter = vec!["alice".to_string()];

        let hidden = compute_hidden_branches(&tree, "bob-1", &authors_filter, &pr_authors, false);

        assert!(hidden.contains("carol-1"));
        assert!(!hidden.contains("carol-1-child"));
    }

    #[test]
    fn empty_authors_filter_hides_nothing() {
        let tree = fixture_tree();
        let pr_authors = fixture_pr_authors();

        let hidden = compute_hidden_branches(&tree, "bob-1", &[], &pr_authors, false);

        assert!(hidden.is_empty());
    }

    #[test]
    fn show_all_disables_hiding() {
        let tree = fixture_tree();
        let pr_authors = fixture_pr_authors();
        let authors_filter = vec!["alice".to_string()];

        let hidden = compute_hidden_branches(&tree, "bob-1", &authors_filter, &pr_authors, true);

        assert!(hidden.is_empty());
    }

    #[test]
    fn tree_root_is_never_hidden() {
        let mut pr_authors = HashMap::new();
        pr_authors.insert("main".to_string(), "mallory".to_string());
        let tree = branch("main", vec![]);
        let authors_filter = vec!["alice".to_string()];

        // "does-not-exist" simulates stale current_branch state that matches nothing in the tree.
        let hidden =
            compute_hidden_branches(&tree, "does-not-exist", &authors_filter, &pr_authors, false);

        assert!(!hidden.contains(&tree.name));
    }

    #[test]
    fn hides_branch_whose_pr_author_came_only_from_the_merged_or_closed_lookup() {
        // Regression test: a branch whose PR was merged/closed by an unlisted author must be
        // hidden even though it would never appear in an open-PRs-only cache. `pr_authors` is
        // exactly the data the caller builds by merging open + closed/merged author lookups, so
        // this is indistinguishable at this layer from `hides_branches_with_unlisted_pr_author`
        // — the fix lives in what main.rs feeds into `pr_authors`, not in this function.
        let tree = branch("main", vec![branch("mallory-1", vec![])]);
        let mut pr_authors = HashMap::new();
        pr_authors.insert("mallory-1".to_string(), "mallory".to_string());
        let authors_filter = vec!["alice".to_string()];

        let hidden = compute_hidden_branches(&tree, "main", &authors_filter, &pr_authors, false);

        assert!(hidden.contains("mallory-1"));
    }

    #[test]
    fn case_insensitive_author_match() {
        // A config entry differing only in case from the PR author's login must still match:
        // GitHub logins are case-insensitive, so `mallory` in the filter matches a `Mallory` login
        // and the branch is not hidden.
        let tree = branch("main", vec![branch("mallory-1", vec![])]);
        let mut pr_authors = HashMap::new();
        pr_authors.insert("mallory-1".to_string(), "Mallory".to_string());
        let authors_filter = vec!["mallory".to_string()];

        let hidden = compute_hidden_branches(&tree, "main", &authors_filter, &pr_authors, false);

        assert!(!hidden.contains("mallory-1"));
    }

    #[test]
    fn descendant_missing_from_scoped_fetch_stays_visible() {
        // The scoped/offline case: a descendant branch that the stack-scoped open-PR fetch
        // returned nothing for (and had no cached entry) has no `pr_authors` entry, so it must
        // never be hidden even with `authors_filter` active — "missing data" ⇒ stays visible.
        let tree = branch(
            "main",
            vec![branch("alice-1", vec![branch("scoped-miss", vec![])])],
        );
        let mut pr_authors = HashMap::new();
        pr_authors.insert("alice-1".to_string(), "alice".to_string());
        // `scoped-miss` deliberately has no entry.
        let authors_filter = vec!["alice".to_string()];

        let hidden = compute_hidden_branches(&tree, "main", &authors_filter, &pr_authors, false);

        assert!(!hidden.contains("scoped-miss"));
    }

    fn sample_pr(number: u64, login: &str) -> PullRequest {
        use crate::github::{PrBranchRef, PrState, PrUser};

        PullRequest {
            number,
            state: PrState::Open,
            title: "test PR".to_string(),
            html_url: format!("https://github.com/example/repo/pull/{number}"),
            base: PrBranchRef {
                ref_name: "main".to_string(),
                sha: "deadbeef".to_string(),
                repo: None,
            },
            head: PrBranchRef {
                ref_name: "feature".to_string(),
                sha: "cafebabe".to_string(),
                repo: None,
            },
            user: PrUser {
                login: login.to_string(),
            },
            draft: false,
            merged: false,
            merged_at: None,
            updated_at: "2024-01-01T00:00:00Z".to_string(),
        }
    }

    fn sample_renderable_branch(name: &str, index: usize) -> RenderableBranch {
        RenderableBranch {
            name: name.to_string(),
            depth: 0,
            is_current: false,
            is_dimmed: false,
            is_remote_only: false,
            status: None,
            diff_stats: None,
            local_status: None,
            pr_info: None,
            note_preview: None,
            verbose: None,
            index,
        }
    }

    #[test]
    fn apply_pr_cache_sets_pr_info_only_for_matching_branches() {
        let mut tree = RenderableTree {
            branches: vec![
                sample_renderable_branch("alice-1", 0),
                sample_renderable_branch("no-pr-branch", 1),
            ],
            current_branch_index: None,
        };

        let mut pr_cache = HashMap::new();
        pr_cache.insert("alice-1".to_string(), sample_pr(42, "alice"));

        apply_pr_cache(&mut tree, Some(&pr_cache));

        let pr_info = tree.branches[0]
            .pr_info
            .as_ref()
            .expect("alice-1 should have pr_info");
        assert_eq!(pr_info.number, 42);
        assert_eq!(pr_info.author, "alice");
        assert!(tree.branches[1].pr_info.is_none());
    }

    #[test]
    fn memoized_diff_stats_computes_once_per_key() {
        use std::cell::Cell;
        let calls = Cell::new(0);
        let mut cache = DiffStatsCache::new();
        let mut run = |base, head| {
            memoized_diff_stats(&mut cache, base, head, || {
                calls.set(calls.get() + 1);
                Some((1, 2))
            })
        };

        assert_eq!(run("a", "b"), Some((1, 2)));
        assert_eq!(run("a", "b"), Some((1, 2))); // cache hit
        assert_eq!(calls.get(), 1);

        assert_eq!(run("a", "c"), Some((1, 2))); // distinct head -> recompute
        assert_eq!(calls.get(), 2);
    }

    #[test]
    fn memoized_diff_stats_caches_failure() {
        use std::cell::Cell;
        let calls = Cell::new(0);
        let mut cache = DiffStatsCache::new();
        let r1 = memoized_diff_stats(&mut cache, "a", "b", || {
            calls.set(calls.get() + 1);
            None
        });
        let r2 = memoized_diff_stats(&mut cache, "a", "b", || {
            calls.set(calls.get() + 1);
            None
        });
        assert_eq!(r1, None);
        assert_eq!(r2, None);
        assert_eq!(calls.get(), 1); // failure cached, not retried
    }

    #[test]
    fn apply_pr_cache_none_leaves_all_pr_info_none() {
        let mut tree = RenderableTree {
            branches: vec![sample_renderable_branch("alice-1", 0)],
            current_branch_index: None,
        };

        apply_pr_cache(&mut tree, None);

        assert!(tree.branches[0].pr_info.is_none());
    }
}
