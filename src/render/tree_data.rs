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
    pr_authors: &HashMap<String, String>,
) -> (bool, bool) {
    let is_target = branch.name == target_branch;
    let has_author = pr_authors
        .get(&branch.name)
        .map(|author| display_authors.contains(author))
        .unwrap_or(false);

    let (child_has_target, child_has_author) = branch
        .branches
        .iter()
        .map(|b| subtree_contains(b, target_branch, display_authors, pr_authors))
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
/// `pr_authors`) are never hidden.
///
/// `pr_authors` is expected to span open, closed, and merged PRs (unlike the open-only
/// `pr_cache` used elsewhere for rendering PR badges) — otherwise a branch whose PR was already
/// merged or closed by someone else would look indistinguishable from a branch that never had a
/// PR, and would wrongly stay visible.
fn compute_hidden_branches(
    tree: &Branch,
    current_branch: &str,
    display_authors: &[String],
    pr_authors: &HashMap<String, String>,
    show_all: bool,
) -> HashSet<String> {
    let mut hidden = HashSet::new();
    if show_all || display_authors.is_empty() {
        return hidden;
    }

    let mut protected = HashSet::new();
    mark_ancestor_path(tree, current_branch, &mut protected);
    protected.insert(tree.name.clone()); // defensive: never hide trunk (e.g. stale current_branch)

    mark_hidden(tree, display_authors, pr_authors, &protected, &mut hidden);
    hidden
}

fn mark_hidden(
    branch: &Branch,
    display_authors: &[String],
    pr_authors: &HashMap<String, String>,
    protected: &HashSet<String>,
    hidden: &mut HashSet<String>,
) {
    if !protected.contains(&branch.name) {
        let pr_author = pr_authors.get(&branch.name).map(|s| s.as_str());
        if pr_author.is_some_and(|author| !display_authors.contains(&author.to_string())) {
            hidden.insert(branch.name.clone());
        }
    }
    for child in &branch.branches {
        mark_hidden(child, display_authors, pr_authors, protected, hidden);
    }
}

/// A visible branch after ordering/hiding/depth are resolved, before its git-derived fields
/// (`status`, `diff_stats`, etc.) are computed. Produced by the pure `plan_tree` pass; consumed
/// in parallel by `compute_branches_parallel`.
struct PlannedBranch<'a> {
    branch: &'a Branch,
    parent_branch: Option<&'a str>,
    depth: usize,
    is_current: bool,
    is_dimmed: bool,
    pr_info: Option<PrRenderInfo>,
    note_preview: Option<String>,
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
    pr_authors: &HashMap<String, String>,
    show_all: bool,
) -> RenderableTree {
    let hidden =
        compute_hidden_branches(tree, current_branch, display_authors, pr_authors, show_all);

    let mut planned = Vec::new();
    let mut current_branch_index = None;
    plan_tree(
        tree,
        None,
        0,
        current_branch,
        pr_cache,
        display_authors,
        pr_authors,
        &hidden,
        &mut planned,
        &mut current_branch_index,
    );

    let branches = compute_branches_parallel(git_repo, &planned, verbose);

    RenderableTree {
        branches,
        current_branch_index,
    }
}

/// Pure ordering/hiding/depth pass: decides which branches are visible, their display order and
/// depth, and precomputes everything that needs no git calls. No git ops happen here, so this can
/// run entirely on the calling thread before the parallel git-op pass in
/// `compute_branches_parallel`.
#[allow(clippy::too_many_arguments)]
fn plan_tree<'a>(
    branch: &'a Branch,
    parent_branch: Option<&'a str>,
    depth: usize,
    current_branch: &str,
    pr_cache: Option<&HashMap<String, PullRequest>>,
    display_authors: &[String],
    pr_authors: &HashMap<String, String>,
    hidden: &HashSet<String>,
    result: &mut Vec<PlannedBranch<'a>>,
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
        let pr_author = pr_authors.get(&branch.name).map(|s| s.as_str());
        let is_dimmed = if display_authors.is_empty() {
            false
        } else {
            pr_author.is_some_and(|author| !display_authors.contains(&author.to_string()))
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

        result.push(PlannedBranch {
            branch,
            parent_branch,
            depth,
            is_current,
            is_dimmed,
            pr_info,
            note_preview,
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
                subtree_contains(b, current_branch, display_authors, pr_authors),
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
        plan_tree(
            child,
            Some(branch.name.as_str()),
            child_depth,
            current_branch,
            pr_cache,
            display_authors,
            pr_authors,
            hidden,
            result,
            current_branch_index,
        );
    }
}

/// Cap on parallel git-op workers, independent of core count.
///
/// Benchmarked against a 402-branch repo on an 18-core machine: raising worker count past ~4-6
/// stops helping and past ~10 actively regresses wall-clock (18 workers: aggregate git-op time
/// balloons from 5.5s serial to ~49s and involuntary context switches explode ~1000x, while
/// wall-clock barely improves over 1 worker). This is libgit2-level contention from many threads
/// each independently reopening and reading the same on-disk repository (its object/pack-file
/// access paths share process-global state), not something this crate can tune away — increasing
/// libgit2's mmap-window limits (`git2::opts::set_mwindow_mapped_limit`/`set_mwindow_file_limit`)
/// had no measurable effect in testing. 4 workers is the smallest count that captures nearly all
/// of the measured speedup (~2.5-3x on the local-compute portion) with the least contention
/// overhead, so it's a safe fixed cap rather than scaling with `available_parallelism()`.
const MAX_GIT_OP_WORKERS: usize = 4;

/// Git-op pass: computes the git-derived fields (`is_remote_only`, `status`, `diff_stats`,
/// `local_status`, `verbose`) for every planned branch, spread across worker threads. Each worker
/// opens its own `GitRepo` handle since `git2::Repository` is `Send` but not `Sync`.
fn compute_branches_parallel(
    git_repo: &GitRepo,
    planned: &[PlannedBranch<'_>],
    verbose: bool,
) -> Vec<RenderableBranch> {
    if planned.is_empty() {
        return Vec::new();
    }

    let repo_path = git_repo.path().to_path_buf();
    let num_workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
        .min(MAX_GIT_OP_WORKERS)
        .min(planned.len());
    let chunk_size = planned.len().div_ceil(num_workers);

    let mut results = Vec::with_capacity(planned.len());
    std::thread::scope(|scope| {
        let handles: Vec<_> = planned
            .chunks(chunk_size)
            .map(|chunk| {
                let repo_path = repo_path.clone();
                scope.spawn(move || {
                    let worker_repo = GitRepo::open(&repo_path)
                        .expect("git-stack: failed to reopen repository on worker thread");
                    let rendered: Vec<RenderableBranch> = chunk
                        .iter()
                        .map(|planned| compute_branch_render_data(&worker_repo, planned, verbose))
                        .collect();
                    (rendered, crate::stats::get_stats())
                })
            })
            .collect();

        for handle in handles {
            let (rendered, worker_stats) = handle
                .join()
                .expect("git-stack: worker thread panicked computing branch status");
            crate::stats::merge_into_current(&worker_stats);
            results.extend(rendered);
        }
    });

    for (index, branch) in results.iter_mut().enumerate() {
        branch.index = index;
    }
    results
}

fn compute_branch_render_data(
    git_repo: &GitRepo,
    planned: &PlannedBranch<'_>,
    verbose: bool,
) -> RenderableBranch {
    let branch = planned.branch;

    // Check if branch is remote-only (not local)
    let is_remote_only = !git_repo.branch_exists(&branch.name);

    // Get branch status
    let status = git_repo
        .branch_status(planned.parent_branch, &branch.name)
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
    let local_status = if planned.is_current {
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

    RenderableBranch {
        name: branch.name.clone(),
        depth: planned.depth,
        is_current: planned.is_current,
        is_dimmed: planned.is_dimmed,
        is_remote_only,
        status,
        diff_stats,
        local_status,
        pr_info: planned.pr_info.clone(),
        note_preview: planned.note_preview.clone(),
        verbose: verbose_details,
        index: 0, // fixed up in compute_branches_parallel after concatenation
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
    use crate::{state::StackMethod, stats};

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
    fn hides_branches_with_unlisted_pr_author() {
        let tree = fixture_tree();
        let pr_authors = fixture_pr_authors();
        let display_authors = vec!["alice".to_string()];

        let hidden = compute_hidden_branches(&tree, "bob-1", &display_authors, &pr_authors, false);

        let expected: HashSet<String> =
            ["carol-1", "eve-1"].iter().map(|s| s.to_string()).collect();
        assert_eq!(hidden, expected);
    }

    #[test]
    fn protects_current_branch_and_its_ancestors() {
        let tree = fixture_tree();
        let pr_authors = fixture_pr_authors();
        let display_authors = vec!["alice".to_string()];

        let hidden = compute_hidden_branches(&tree, "bob-1", &display_authors, &pr_authors, false);

        assert!(!hidden.contains("bob-1"));
        assert!(!hidden.contains("alice-1"));
    }

    #[test]
    fn never_hides_branches_without_a_pr() {
        let tree = fixture_tree();
        let pr_authors = fixture_pr_authors();
        let display_authors = vec!["alice".to_string()];

        let hidden = compute_hidden_branches(&tree, "bob-1", &display_authors, &pr_authors, false);

        assert!(!hidden.contains("main"));
        assert!(!hidden.contains("dave-1"));
    }

    #[test]
    fn hidden_branch_does_not_hide_its_own_listed_author_child() {
        let tree = fixture_tree();
        let pr_authors = fixture_pr_authors();
        let display_authors = vec!["alice".to_string()];

        let hidden = compute_hidden_branches(&tree, "bob-1", &display_authors, &pr_authors, false);

        assert!(hidden.contains("carol-1"));
        assert!(!hidden.contains("carol-1-child"));
    }

    #[test]
    fn empty_display_authors_hides_nothing() {
        let tree = fixture_tree();
        let pr_authors = fixture_pr_authors();

        let hidden = compute_hidden_branches(&tree, "bob-1", &[], &pr_authors, false);

        assert!(hidden.is_empty());
    }

    #[test]
    fn show_all_disables_hiding() {
        let tree = fixture_tree();
        let pr_authors = fixture_pr_authors();
        let display_authors = vec!["alice".to_string()];

        let hidden = compute_hidden_branches(&tree, "bob-1", &display_authors, &pr_authors, true);

        assert!(hidden.is_empty());
    }

    #[test]
    fn tree_root_is_never_hidden() {
        let mut pr_authors = HashMap::new();
        pr_authors.insert("main".to_string(), "mallory".to_string());
        let tree = branch("main", vec![]);
        let display_authors = vec!["alice".to_string()];

        // "does-not-exist" simulates stale current_branch state that matches nothing in the tree.
        let hidden = compute_hidden_branches(
            &tree,
            "does-not-exist",
            &display_authors,
            &pr_authors,
            false,
        );

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
        let display_authors = vec!["alice".to_string()];

        let hidden = compute_hidden_branches(&tree, "main", &display_authors, &pr_authors, false);

        assert!(hidden.contains("mallory-1"));
    }

    fn commit_with_file(
        repo: &git2::Repository,
        sig: &git2::Signature,
        base_tree: Option<&git2::Tree>,
        file_name: &str,
        file_contents: &str,
        parent: Option<&git2::Commit>,
    ) -> git2::Oid {
        let mut builder = repo.treebuilder(base_tree).unwrap();
        let blob = repo.blob(file_contents.as_bytes()).unwrap();
        builder.insert(file_name, blob, 0o100644).unwrap();
        let tree = repo.find_tree(builder.write().unwrap()).unwrap();
        let parents: Vec<&git2::Commit> = parent.into_iter().collect();
        repo.commit(None, sig, sig, "msg", &tree, &parents).unwrap()
    }

    /// main -> feature-a (lkg_parent set) -> feature-b (no lkg_parent, exercises merge_base
    /// fallback); main -> feature-c (lkg_parent set, sibling of feature-a).
    fn build_fixture_repo() -> (tempfile::TempDir, GitRepo, Branch) {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();

        let main_id = commit_with_file(&repo, &sig, None, "README.md", "root\n", None);
        let main_commit = repo.find_commit(main_id).unwrap();
        repo.branch("main", &main_commit, false).unwrap();

        let a_id = commit_with_file(
            &repo,
            &sig,
            Some(&main_commit.tree().unwrap()),
            "a.txt",
            "a\n",
            Some(&main_commit),
        );
        let a_commit = repo.find_commit(a_id).unwrap();
        repo.branch("feature-a", &a_commit, false).unwrap();

        let b_id = commit_with_file(
            &repo,
            &sig,
            Some(&a_commit.tree().unwrap()),
            "b.txt",
            "b\nb2\n",
            Some(&a_commit),
        );
        repo.branch("feature-b", &repo.find_commit(b_id).unwrap(), false)
            .unwrap();

        let c_id = commit_with_file(
            &repo,
            &sig,
            Some(&main_commit.tree().unwrap()),
            "c.txt",
            "c\n",
            Some(&main_commit),
        );
        repo.branch("feature-c", &repo.find_commit(c_id).unwrap(), false)
            .unwrap();

        let tree = branch(
            "main",
            vec![
                {
                    let mut a = branch("feature-a", vec![branch("feature-b", vec![])]);
                    a.lkg_parent = Some(main_id.to_string());
                    a
                },
                {
                    let mut c = branch("feature-c", vec![]);
                    c.lkg_parent = Some(main_id.to_string());
                    c
                },
            ],
        );

        let git_repo = GitRepo::open(dir.path()).unwrap();
        (dir, git_repo, tree)
    }

    #[test]
    fn parallel_pass_matches_expected_order_depth_and_diff_stats() {
        stats::reset_stats();
        let (_dir, git_repo, tree) = build_fixture_repo();

        let renderable = compute_renderable_tree(
            &git_repo,
            &tree,
            "not-a-real-branch",
            false,
            None,
            &[],
            &HashMap::new(),
            true,
        );

        let names: Vec<&str> = renderable
            .branches
            .iter()
            .map(|b| b.name.as_str())
            .collect();
        assert_eq!(names, ["main", "feature-a", "feature-b", "feature-c"]);
        assert_eq!(
            renderable
                .branches
                .iter()
                .map(|b| b.depth)
                .collect::<Vec<_>>(),
            [0, 1, 2, 1]
        );
        assert_eq!(renderable.current_branch_index, None);

        let by_name: HashMap<&str, &RenderableBranch> = renderable
            .branches
            .iter()
            .map(|b| (b.name.as_str(), b))
            .collect();

        let a_stats = by_name["feature-a"].diff_stats.as_ref().unwrap();
        assert_eq!(
            (a_stats.additions, a_stats.deletions, a_stats.reliable),
            (1, 0, true)
        );

        let b_stats = by_name["feature-b"].diff_stats.as_ref().unwrap();
        assert_eq!(
            (b_stats.additions, b_stats.deletions, b_stats.reliable),
            (2, 0, false)
        );

        let c_stats = by_name["feature-c"].diff_stats.as_ref().unwrap();
        assert_eq!(
            (c_stats.additions, c_stats.deletions, c_stats.reliable),
            (1, 0, true)
        );

        // Regression guard for the thread-local GIT_STATS aggregation: without merging worker
        // stats back into the main thread, `git2:diff-stats`/`git2:open` counts below would be
        // near zero (only whatever ran on the main thread itself).
        let stats = stats::get_stats();
        assert_eq!(
            stats.by_command.get("git2:diff-stats").map(|s| s.count),
            Some(3)
        );
        assert!(
            stats
                .by_command
                .get("git2:open")
                .map(|s| s.count)
                .unwrap_or(0)
                >= 2
        );
    }
}
