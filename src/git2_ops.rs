//! git2 wrapper module for fast read-only git operations.
//!
//! This module provides a `GitRepo` struct that wraps git2::Repository
//! for fast read-only operations without spawning git processes.

use std::{path::Path, time::Instant};

use anyhow::{Context, Result, anyhow};
use git2::{BranchType, Repository};

use crate::{lock::RepoLock, merge_base_cache::MergeBaseCacheHandle, stats::GitBenchmark};

pub const DEFAULT_REMOTE: &str = "origin";

#[derive(Debug)]
pub(crate) struct UpstreamStatus {
    pub(crate) symbolic_name: String,
    pub(crate) synced: bool,
}

#[derive(Debug)]
pub(crate) struct GitBranchStatus {
    pub(crate) sha: String,
    pub(crate) exists: bool,
    pub(crate) is_descendent: bool,
    pub(crate) parent_branch: String,
    pub(crate) upstream_status: Option<UpstreamStatus>,
}

/// Wrapper around git2::Repository for fast read-only git operations.
pub struct GitRepo {
    repo: Repository,
    /// Persistent cache for `merge_base` / `is_ancestor` results. `None` when the cache could not
    /// be opened (e.g. another process holds redb's exclusive lock), degrading to uncached.
    merge_base_cache: Option<MergeBaseCacheHandle>,
    /// Canonicalized common git dir, used as the cache scope key.
    repo_scope: String,
}

impl GitRepo {
    /// Open a repository at the given path. Opens the merge-base cache best-effort at its default
    /// XDG path; a cache-open failure degrades this invocation to uncached but never fails.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let cache = match MergeBaseCacheHandle::open() {
            Ok(cache) => Some(cache),
            Err(e) => {
                tracing::debug!("Failed to open merge-base cache, running uncached: {e:#}");
                None
            }
        };
        Self::open_inner(path, cache)
    }

    /// Open a repository with the merge-base cache at an explicit path, keeping tests isolated
    /// from the real user cache.
    #[cfg(test)]
    pub fn open_with_cache_at(path: impl AsRef<Path>, cache_path: &Path) -> Result<Self> {
        let cache = Some(MergeBaseCacheHandle::open_at(cache_path)?);
        Self::open_inner(path, cache)
    }

    fn open_inner(path: impl AsRef<Path>, cache: Option<MergeBaseCacheHandle>) -> Result<Self> {
        let _bench = GitBenchmark::start("git2:open");
        let repo = Repository::open(path.as_ref())
            .with_context(|| format!("Failed to open repository at {:?}", path.as_ref()))?;
        let repo_scope = std::fs::canonicalize(repo.commondir())
            .unwrap_or_else(|_| repo.commondir().to_path_buf())
            .to_string_lossy()
            .into_owned();
        Ok(Self {
            repo,
            merge_base_cache: cache,
            repo_scope,
        })
    }

    /// The cache scope key (canonicalized common git dir). Test-only accessor for priming the
    /// cache with the exact key `GitRepo` uses.
    #[cfg(test)]
    pub fn repo_scope(&self) -> &str {
        &self.repo_scope
    }

    /// Clear the merge-base / is-ancestor cache for this repo's scope. No-op if the cache never
    /// opened.
    pub fn clear_merge_base_cache(&self) -> Result<()> {
        if let Some(cache) = &self.merge_base_cache {
            cache.clear_scope(&self.repo_scope)?;
        }
        Ok(())
    }

    /// Acquire a repo-scoped advisory lock (see [`crate::lock::RepoLock`]).
    ///
    /// Held across mutating operations so two git-stack invocations can't race
    /// on ref updates (e.g. concurrent `fetch --prune`). Scoped to the common
    /// git dir so linked worktrees of the same repo serialize together.
    pub fn lock(&self) -> Result<RepoLock> {
        RepoLock::acquire(self.repo.commondir())
    }

    /// Get the SHA of a reference (branch name, tag, or other ref).
    /// Equivalent to `git rev-parse <ref>`
    pub fn sha(&self, ref_name: &str) -> Result<String> {
        let _bench = GitBenchmark::start("git2:rev-parse");
        let obj = self
            .repo
            .revparse_single(ref_name)
            .with_context(|| format!("Failed to resolve ref: {}", ref_name))?;
        Ok(obj.id().to_string())
    }

    /// Check if ancestor_ref is an ancestor of descendant_ref.
    /// Equivalent to `git merge-base --is-ancestor <ancestor> <descendant>`
    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
        let ancestor_obj = self
            .repo
            .revparse_single(ancestor)
            .with_context(|| format!("Failed to resolve ancestor ref: {}", ancestor))?;
        let descendant_obj = self
            .repo
            .revparse_single(descendant)
            .with_context(|| format!("Failed to resolve descendant ref: {}", descendant))?;

        // A commit is considered an ancestor of itself (matches git behavior)
        if ancestor_obj.id() == descendant_obj.id() {
            return Ok(true);
        }

        let anc_oid = ancestor_obj.id().to_string();
        let desc_oid = descendant_obj.id().to_string();

        // Cache hit skips the graph walk (and its benchmark span) entirely.
        if let Some(cache) = &self.merge_base_cache {
            match cache.get_is_ancestor(&self.repo_scope, &anc_oid, &desc_oid) {
                Ok(Some(val)) => return Ok(val),
                Ok(None) => {}
                Err(e) => tracing::debug!("merge-base cache read failed, computing live: {e:#}"),
            }
        }

        let _bench = GitBenchmark::start("git2:is-ancestor");
        let val = self
            .repo
            .graph_descendant_of(descendant_obj.id(), ancestor_obj.id())
            .unwrap_or(false);

        if let Some(cache) = &self.merge_base_cache
            && let Err(e) = cache.put_is_ancestor(&self.repo_scope, &anc_oid, &desc_oid, val)
        {
            tracing::debug!("merge-base cache write failed: {e:#}");
        }
        Ok(val)
    }

    /// Find the merge-base (common ancestor) of two refs.
    /// Equivalent to `git merge-base <ref1> <ref2>`
    pub fn merge_base(&self, ref1: &str, ref2: &str) -> Result<String> {
        let obj1 = self
            .repo
            .revparse_single(ref1)
            .with_context(|| format!("Failed to resolve ref: {}", ref1))?;
        let obj2 = self
            .repo
            .revparse_single(ref2)
            .with_context(|| format!("Failed to resolve ref: {}", ref2))?;

        let oid1 = obj1.id().to_string();
        let oid2 = obj2.id().to_string();

        // Cache hit skips the graph walk (and its benchmark span) entirely.
        if let Some(cache) = &self.merge_base_cache {
            match cache.get_merge_base(&self.repo_scope, &oid1, &oid2) {
                Ok(Some(base)) => return Ok(base),
                Ok(None) => {}
                Err(e) => tracing::debug!("merge-base cache read failed, computing live: {e:#}"),
            }
        }

        let _bench = GitBenchmark::start("git2:merge-base");
        let oid = self
            .repo
            .merge_base(obj1.id(), obj2.id())
            .with_context(|| format!("Failed to find merge-base between {} and {}", ref1, ref2))?;
        let base = oid.to_string();

        // Only successful results are cached; a real "no merge base" error propagates uncached.
        if let Some(cache) = &self.merge_base_cache
            && let Err(e) = cache.put_merge_base(&self.repo_scope, &oid1, &oid2, &base)
        {
            tracing::debug!("merge-base cache write failed: {e:#}");
        }
        Ok(base)
    }

    /// Check if a local branch exists.
    /// Only checks for local branches, not remote refs.
    pub fn branch_exists(&self, branch: &str) -> bool {
        let _bench = GitBenchmark::start("git2:branch-exists");
        // Only check for local branches to avoid false positives from remote refs
        self.repo.find_branch(branch, BranchType::Local).is_ok()
    }

    /// Check if a ref exists (local branch, remote ref, or any resolvable ref).
    pub fn ref_exists(&self, ref_name: &str) -> bool {
        let _bench = GitBenchmark::start("git2:ref-exists");
        self.repo.revparse_single(ref_name).is_ok()
    }

    /// The set of commit OIDs (lowercase hex) reachable from any of `tips` but not from `exclude`.
    ///
    /// This is the inverted, bounded form of the per-SHA `is_ancestor` probing that `sync`'s
    /// seen-SHA pass used to do. Asking "is this PR-head SHA reachable from a tracked branch and
    /// not yet merged into trunk?" for tens of thousands of SHAs one at a time means a graph walk
    /// (or a refresh-on-miss ODB lookup, for SHAs never fetched locally) per SHA. Instead we walk
    /// once from the stack's own tips, hiding the trunk boundary, and return the commits in
    /// between; membership is then an O(1) set lookup per SHA. Cost scales with the stack size,
    /// not with the repo's closed-PR count.
    ///
    /// `exclude` (typically `origin/<trunk>`) must resolve — it bounds the walk to commits not in
    /// trunk. If it does not resolve (unexpected after a fetch), the walk would be unbounded, so we
    /// log and return an empty set rather than traverse the repo's entire history.
    pub fn commits_reachable_excluding(
        &self,
        tips: &[String],
        exclude: &str,
    ) -> Result<std::collections::HashSet<String>> {
        let _bench = GitBenchmark::start("git2:revwalk-reachable");

        let Some(exclude_commit) = self
            .repo
            .revparse_single(exclude)
            .ok()
            .and_then(|obj| obj.peel_to_commit().ok())
        else {
            tracing::warn!(
                "commits_reachable_excluding: exclude ref {exclude} did not resolve; \
                 skipping seen-SHA reachability this run"
            );
            return Ok(std::collections::HashSet::new());
        };

        let mut walk = self.repo.revwalk().context("creating revwalk")?;
        walk.hide(exclude_commit.id())
            .context("hiding exclude boundary in revwalk")?;

        let mut pushed = false;
        for tip in tips {
            if let Ok(oid) = git2::Oid::from_str(tip)
                && walk.push(oid).is_ok()
            {
                pushed = true;
            }
        }
        if !pushed {
            return Ok(std::collections::HashSet::new());
        }

        let mut set = std::collections::HashSet::new();
        for oid in walk {
            match oid {
                Ok(oid) => {
                    set.insert(oid.to_string());
                }
                Err(e) => {
                    tracing::debug!("revwalk error while collecting reachable commits: {e}");
                    break;
                }
            }
        }
        Ok(set)
    }

    /// Resolve a branch name to a ref that exists.
    /// Tries the branch name first, then falls back to origin/{branch}.
    /// Returns None if neither exists.
    pub fn resolve_branch_ref(&self, branch: &str) -> Option<String> {
        if self.branch_exists(branch) {
            Some(branch.to_string())
        } else {
            let remote_ref = format!("origin/{}", branch);
            if self.ref_exists(&remote_ref) {
                Some(remote_ref)
            } else {
                None
            }
        }
    }

    pub fn branch_status(
        &self,
        parent_branch: Option<&str>,
        branch: &str,
    ) -> Result<GitBranchStatus> {
        let exists = self.branch_exists(branch);

        // Resolve parent branch - use origin/<parent> if local doesn't exist
        let parent_branch = match parent_branch {
            Some(parent_branch) => {
                if self.branch_exists(parent_branch) {
                    parent_branch.to_string()
                } else {
                    let remote_parent = format!("origin/{}", parent_branch);
                    if self.ref_exists(&remote_parent) {
                        remote_parent
                    } else {
                        parent_branch.to_string()
                    }
                }
            }
            None => self.remote_main(DEFAULT_REMOTE)?,
        };

        // Only compute these if the branch exists
        let (sha, is_descendent, upstream_status) = if exists {
            let sha = self.sha(branch)?;
            let is_descendent = self.is_ancestor(&parent_branch, branch)?;
            let upstream_symbolic_name = self.get_upstream(branch);
            let upstream_synced = upstream_symbolic_name
                .as_ref()
                .is_some_and(|upstream| self.shas_match(upstream, branch));
            let upstream_status = upstream_symbolic_name.map(|symbolic_name| UpstreamStatus {
                symbolic_name,
                synced: upstream_synced,
            });
            (sha, is_descendent, upstream_status)
        } else {
            // Local branch doesn't exist - try using origin/<branch> instead
            let remote_ref = format!("origin/{}", branch);
            if self.ref_exists(&remote_ref) {
                let sha = self.sha(&remote_ref).unwrap_or_default();
                let is_descendent = self
                    .is_ancestor(&parent_branch, &remote_ref)
                    .unwrap_or(false);
                (sha, is_descendent, None)
            } else {
                // Neither local nor remote exists - use placeholder values
                (String::new(), false, None)
            }
        };

        Ok(GitBranchStatus {
            sha,
            parent_branch,
            exists,
            is_descendent,
            upstream_status,
        })
    }

    /// Get the remote main branch name (e.g., "origin/main").
    /// Equivalent to `git symbolic-ref refs/remotes/<remote>/HEAD`
    pub fn remote_main(&self, remote: &str) -> Result<String> {
        let _bench = GitBenchmark::start("git2:symbolic-ref");
        let ref_name = format!("refs/remotes/{}/HEAD", remote);
        let reference = self
            .repo
            .find_reference(&ref_name)
            .with_context(|| format!("Failed to find remote HEAD: {}", ref_name))?;

        let target = reference
            .symbolic_target()
            .ok_or_else(|| anyhow!("{} is not a symbolic reference", ref_name))?;

        Ok(target
            .strip_prefix("refs/remotes/")
            .unwrap_or(target)
            .to_string())
    }

    /// Check if two refs point to the same commit.
    /// Equivalent to comparing `git rev-parse <ref1>` and `git rev-parse <ref2>`
    pub fn shas_match(&self, ref1: &str, ref2: &str) -> bool {
        let _bench = GitBenchmark::start("git2:shas-match");
        let Some(obj1) = self.repo.revparse_single(ref1).ok() else {
            return false;
        };
        let Some(obj2) = self.repo.revparse_single(ref2).ok() else {
            return false;
        };
        obj1.id() == obj2.id()
    }

    /// Get the repo root path.
    /// Equivalent to `git rev-parse --show-toplevel`
    pub fn root(&self) -> Result<String> {
        let _bench = GitBenchmark::start("git2:show-toplevel");
        let workdir = self
            .repo
            .workdir()
            .ok_or_else(|| anyhow!("Repository has no working directory"))?;
        Ok(workdir
            .to_str()
            .ok_or_else(|| anyhow!("Invalid path encoding"))?
            .trim_end_matches('/')
            .to_string())
    }

    /// Get current branch name.
    /// Equivalent to `git rev-parse --abbrev-ref HEAD`
    pub fn current_branch(&self) -> Result<String> {
        let _bench = GitBenchmark::start("git2:current-branch");
        let head = self.repo.head().context("Failed to get HEAD")?;
        if head.is_branch() {
            Ok(head
                .shorthand()
                .ok_or_else(|| anyhow!("HEAD has no shorthand name"))?
                .to_string())
        } else {
            // Detached HEAD - return the SHA
            Ok(head
                .target()
                .ok_or_else(|| anyhow!("HEAD has no target"))?
                .to_string())
        }
    }

    /// Get the upstream tracking branch for a local branch.
    /// Equivalent to `git rev-parse --abbrev-ref --symbolic-full-name <branch>@{upstream}`
    pub fn get_upstream(&self, branch: &str) -> Option<String> {
        let _bench = GitBenchmark::start("git2:get-upstream");
        let local_branch = self.repo.find_branch(branch, BranchType::Local).ok()?;
        let upstream = local_branch.upstream().ok()?;
        let name = upstream.name().ok()??;
        Some(name.to_string())
    }

    /// Get diff stats (additions, deletions) between two commits.
    /// Equivalent to parsing `git log --numstat --pretty="" <base>..<head>`
    pub fn diff_stats(&self, base: &str, head: &str) -> Result<(usize, usize)> {
        let _bench = GitBenchmark::start("git2:diff-stats");

        let base_obj = self
            .repo
            .revparse_single(base)
            .with_context(|| format!("Failed to resolve base ref: {}", base))?;
        let head_obj = self
            .repo
            .revparse_single(head)
            .with_context(|| format!("Failed to resolve head ref: {}", head))?;

        let base_commit = base_obj
            .peel_to_commit()
            .with_context(|| format!("Failed to peel base to commit: {}", base))?;
        let head_commit = head_obj
            .peel_to_commit()
            .with_context(|| format!("Failed to peel head to commit: {}", head))?;

        let base_tree = base_commit.tree()?;
        let head_tree = head_commit.tree()?;

        let diff = self
            .repo
            .diff_tree_to_tree(Some(&base_tree), Some(&head_tree), None)?;

        let stats = diff.stats()?;
        Ok((stats.insertions(), stats.deletions()))
    }

    /// Get the URL of a remote.
    /// Equivalent to `git remote get-url <remote>`
    pub fn get_remote_url(&self, remote: &str) -> Result<String> {
        let _bench = GitBenchmark::start("git2:remote-url");
        let remote = self
            .repo
            .find_remote(remote)
            .with_context(|| format!("Failed to find remote: {}", remote))?;
        remote
            .url()
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("Remote has no URL"))
    }
}

#[cfg(test)]
mod tests {
    use std::{path::Path, process::Command};

    use super::*;
    use crate::merge_base_cache::MergeBaseCacheHandle;

    fn git(dir: &Path, args: &[&str]) {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(dir)
                .status()
                .unwrap()
                .success()
        );
    }

    fn git_rev_parse(dir: &Path, rev: &str) -> String {
        let output = Command::new("git")
            .args(["rev-parse", rev])
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    /// Two divergent branches: `feature` is NOT an ancestor of `main`, and their only common
    /// ancestor is the root commit.
    fn init_divergent_repo(dir: &Path) {
        git(dir, &["init", "-q", "-b", "main"]);
        git(dir, &["config", "user.email", "test@example.com"]);
        git(dir, &["config", "user.name", "Test"]);
        git(dir, &["commit", "--allow-empty", "-q", "-m", "root"]);
        git(dir, &["checkout", "-q", "-b", "feature"]);
        git(
            dir,
            &["commit", "--allow-empty", "-q", "-m", "feature commit"],
        );
        git(dir, &["checkout", "-q", "main"]);
        git(dir, &["commit", "--allow-empty", "-q", "-m", "main commit"]);
    }

    /// Acceptance criterion: `is_ancestor`'s read path genuinely short-circuits to the cache. We
    /// seed a deliberately *wrong* answer (`true`) for a pair that is not actually ancestor-related
    /// and prove `is_ancestor` returns the cached value rather than doing a live walk.
    #[test]
    fn is_ancestor_short_circuits_to_cache() {
        let dir = tempfile::tempdir().unwrap();
        init_divergent_repo(dir.path());
        let cache_path = dir.path().join("mb_cache.redb");

        let oid_feature = git_rev_parse(dir.path(), "feature");
        let oid_main = git_rev_parse(dir.path(), "main");

        // Grab the scope key from a throwaway GitRepo, then drop it so its exclusive redb lock is
        // released before we prime the cache from a second handle.
        let scope = {
            let git_repo = GitRepo::open_with_cache_at(dir.path(), &cache_path).unwrap();
            // Sanity: `feature` is genuinely not an ancestor of `main` when computed live.
            assert!(!git_repo.is_ancestor("feature", "main").unwrap());
            git_repo.repo_scope().to_string()
        };

        // Overwrite the (correct, `false`) row the throwaway wrote with a bogus `true`.
        {
            let cache = MergeBaseCacheHandle::open_at(&cache_path).unwrap();
            cache
                .put_is_ancestor(&scope, &oid_feature, &oid_main, true)
                .unwrap();
        }

        let git_repo = GitRepo::open_with_cache_at(dir.path(), &cache_path).unwrap();
        // If this returned the live-walk answer it would be `false`; `true` proves the cache hit.
        assert!(git_repo.is_ancestor("feature", "main").unwrap());
    }

    /// Same acceptance check for `merge_base`: seed a bogus base OID and prove it's returned.
    #[test]
    fn merge_base_short_circuits_to_cache() {
        let dir = tempfile::tempdir().unwrap();
        init_divergent_repo(dir.path());
        let cache_path = dir.path().join("mb_cache.redb");

        let oid_feature = git_rev_parse(dir.path(), "feature");
        let oid_main = git_rev_parse(dir.path(), "main");
        let bogus_base = "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef";

        let scope = {
            let git_repo = GitRepo::open_with_cache_at(dir.path(), &cache_path).unwrap();
            git_repo.repo_scope().to_string()
        };

        {
            let cache = MergeBaseCacheHandle::open_at(&cache_path).unwrap();
            cache
                .put_merge_base(&scope, &oid_feature, &oid_main, bogus_base)
                .unwrap();
        }

        let git_repo = GitRepo::open_with_cache_at(dir.path(), &cache_path).unwrap();
        assert_eq!(git_repo.merge_base("feature", "main").unwrap(), bogus_base);
    }

    /// `commits_reachable_excluding` is the bounded revwalk that replaced `sync`'s per-SHA
    /// is_ancestor loop. It must return exactly the commits reachable from the given tips but not
    /// from the exclude boundary — the same set the old "reachable from a tracked branch and not
    /// merged to trunk" condition selected.
    #[test]
    fn commits_reachable_excluding_matches_is_ancestor_condition() {
        let dir = tempfile::tempdir().unwrap();
        init_divergent_repo(dir.path());
        let cache_path = dir.path().join("mb_cache.redb");
        let git_repo = GitRepo::open_with_cache_at(dir.path(), &cache_path).unwrap();

        let oid_feature = git_rev_parse(dir.path(), "feature");
        let oid_main = git_rev_parse(dir.path(), "main");
        let oid_root = git_rev_parse(dir.path(), "feature~1");

        // Walk from `feature`, hiding `main`: `feature`'s own commit is reachable and not in main;
        // the shared root commit is hidden by `main`.
        let set = git_repo
            .commits_reachable_excluding(std::slice::from_ref(&oid_feature), "main")
            .unwrap();
        assert!(set.contains(&oid_feature));
        assert!(!set.contains(&oid_root));
        assert!(!set.contains(&oid_main));

        // A well-formed-but-absent tip contributes nothing (mirrors an unfetched PR head SHA);
        // with no resolvable tips the set is empty.
        let empty = git_repo
            .commits_reachable_excluding(
                &["deadbeefdeadbeefdeadbeefdeadbeefdeadbeef".to_string()],
                "main",
            )
            .unwrap();
        assert!(empty.is_empty());

        // An exclude ref that does not resolve yields an empty set rather than an unbounded walk.
        let unresolved = git_repo
            .commits_reachable_excluding(
                std::slice::from_ref(&oid_feature),
                "origin/does-not-exist",
            )
            .unwrap();
        assert!(unresolved.is_empty());
    }

    /// Two distinct repos with separate cache files don't cross results: a bogus row primed in
    /// repo A's cache must not affect repo B.
    #[test]
    fn caches_are_scoped_per_repo() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        init_divergent_repo(dir_a.path());
        init_divergent_repo(dir_b.path());
        let cache_a = dir_a.path().join("mb_cache.redb");
        let cache_b = dir_b.path().join("mb_cache.redb");

        let oid_feature_a = git_rev_parse(dir_a.path(), "feature");
        let oid_main_a = git_rev_parse(dir_a.path(), "main");

        let scope_a = {
            let git_repo = GitRepo::open_with_cache_at(dir_a.path(), &cache_a).unwrap();
            git_repo.repo_scope().to_string()
        };
        {
            let cache = MergeBaseCacheHandle::open_at(&cache_a).unwrap();
            cache
                .put_is_ancestor(&scope_a, &oid_feature_a, &oid_main_a, true)
                .unwrap();
        }

        // Repo B uses its own cache file, so it must compute the real (`false`) answer.
        let git_repo_b = GitRepo::open_with_cache_at(dir_b.path(), &cache_b).unwrap();
        assert!(!git_repo_b.is_ancestor("feature", "main").unwrap());
    }
}
