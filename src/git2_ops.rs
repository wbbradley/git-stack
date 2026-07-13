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

    /// Build the patch series to replay `branch` onto `parent`, exactly as
    /// `git rebase`'s `am` backend does: the symmetric-difference range
    /// `parent...branch` with `--cherry-pick --right-only`, so commits already
    /// reachable from `parent` — and commits whose change is already present
    /// upstream by patch-id (e.g. pulled in by a `Merge branch 'main'` commit, or
    /// belonging to a parent that was itself just rebased) — are dropped instead of
    /// replayed. `format-patch` already skips merge commits. Returns `None` when the
    /// series is empty (no branch-only work remains). Fixes the stale-base bug where
    /// a cached LKG parent behind the merge-base replayed already-merged commits
    /// (see restack-problem.md).
    ///
    /// `lkg_parent` is the branch's recorded last-known-good parent tip (the old parent
    /// tip the branch was built on), if known. When the parent was **rewritten with new
    /// content** (e.g. a conflict resolution against trunk changed one of its commits),
    /// `parent...branch`'s boundary falls back to `merge-base(parent, branch)` — a trunk
    /// commit — and the branch's now-superseded *old parent* commits (no longer
    /// patch-equivalent to the rewritten ones, so `--cherry-pick` can't drop them) get
    /// re-replayed, manufacturing `add/add`/content conflicts. Excluding `^<lkg_parent>`
    /// drops exactly those old-parent commits while still keeping the `parent...` boundary
    /// that lets `--right-only` shed commits a `Merge branch 'main'` pulled in (a stale
    /// `lkg` alone can't). Callers pass this only when `lkg_parent` is an ancestor of
    /// `branch`, so the exclude is always meaningful.
    pub fn restack_patch_series(
        &self,
        parent: &str,
        branch: &str,
        lkg_parent: Option<&str>,
    ) -> Result<Option<String>> {
        let workdir = self
            .repo
            .workdir()
            .ok_or_else(|| anyhow!("Repository has no working directory"))?;
        let range = format!("{parent}...{branch}");
        let mut args = vec![
            "format-patch",
            "--stdout",
            "--cherry-pick",
            "--right-only",
            &range,
        ];
        // Exclude the old parent tip so a rewritten parent's superseded commits aren't
        // replayed (see doc comment above).
        let lkg_exclude = lkg_parent.map(|lkg| format!("^{lkg}"));
        if let Some(lkg_exclude) = lkg_exclude.as_deref() {
            args.push(lkg_exclude);
        }
        let out = std::process::Command::new("git")
            .args(&args)
            .current_dir(workdir)
            .output()
            .with_context(|| format!("git format-patch {range}"))?;
        if !out.status.success() {
            anyhow::bail!(
                "git format-patch {range} failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        let patch = String::from_utf8_lossy(&out.stdout);
        let patch = patch.trim();
        Ok((!patch.is_empty()).then(|| patch.to_string()))
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

    /// True if the staged index is identical to HEAD's tree — i.e. there are no staged changes.
    ///
    /// During a conflicted `git am`, once the user resolves the conflict and `git add`s the
    /// result, this means the replayed patch is **empty**: its changes are already present in the
    /// new parent (a superseded/duplicated base commit resolved by keeping the parent's version).
    /// `git am --continue` refuses to advance an empty patch ("No changes - did you forget to use
    /// 'git add'?"), so the caller must `git am --skip` it instead.
    ///
    /// Reloads the on-disk index so an external `git add` is visible. Returns `false` if the index
    /// still has unresolved conflicts (the caller handles those separately).
    pub fn staged_matches_head(&self) -> Result<bool> {
        let _bench = GitBenchmark::start("git2:staged-matches-head");
        let mut index = self.repo.index().context("Failed to open index")?;
        // Pick up `git add`s performed by the CLI git process, not just this handle's cache.
        index.read(true).context("Failed to reload index")?;
        if index.has_conflicts() {
            return Ok(false);
        }
        let index_tree = index.write_tree().context("Failed to write index tree")?;
        let head_tree = self
            .repo
            .head()
            .context("Failed to resolve HEAD")?
            .peel_to_tree()
            .context("Failed to peel HEAD to tree")?
            .id();
        Ok(index_tree == head_tree)
    }

    /// True if git has a `git am` (mailbox apply) operation in progress.
    ///
    /// Used by `restack --continue`/`--skip` to detect an am the user already finished by hand
    /// (e.g. `git am --skip` after an empty/superseded patch). When false, there is nothing left
    /// to advance, so the caller should just resume the remaining plan instead of running — and
    /// erroring on — `git am --continue`. `ApplyMailboxOrRebase` (git couldn't disambiguate) is
    /// treated as in-progress so an ambiguous state is never mistaken for finished.
    pub fn am_in_progress(&self) -> bool {
        matches!(
            self.repo.state(),
            git2::RepositoryState::ApplyMailbox | git2::RepositoryState::ApplyMailboxOrRebase
        )
    }

    /// True if git has a `git rebase` operation in progress (any backend). Mirror of
    /// [`Self::am_in_progress`] for the rebase-fallback restack path.
    pub fn rebase_in_progress(&self) -> bool {
        matches!(
            self.repo.state(),
            git2::RepositoryState::Rebase
                | git2::RepositoryState::RebaseInteractive
                | git2::RepositoryState::RebaseMerge
                | git2::RepositoryState::ApplyMailboxOrRebase
        )
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

    /// Disable git's background auto-maintenance and gc. Without this, `git commit` spawns a
    /// detached `git maintenance run --auto --detach` that inherits the test process's
    /// stdout/stderr pipe and keeps running after the test exits — nextest flags the still-open
    /// pipe as a leaked handle.
    fn disable_auto_maintenance(dir: &Path) {
        git(dir, &["config", "maintenance.auto", "false"]);
        git(dir, &["config", "gc.auto", "0"]);
    }

    /// Init a repo with `main` checked out and a committer identity configured.
    fn init_repo(dir: &Path) {
        git(dir, &["init", "-q", "-b", "main"]);
        git(dir, &["config", "user.email", "test@example.com"]);
        git(dir, &["config", "user.name", "Test"]);
        disable_auto_maintenance(dir);
    }

    /// Write `content` to `file` and commit it with message `msg`.
    fn commit_file(dir: &Path, file: &str, content: &str, msg: &str) {
        std::fs::write(dir.join(file), content).unwrap();
        git(dir, &["add", file]);
        git(dir, &["commit", "-q", "-m", msg]);
    }

    /// Apply a `format-patch` series via `git am --3way`, feeding it on stdin. When
    /// `committer_date` is set it pins `GIT_COMMITTER_DATE` so the replayed commit's SHA is
    /// deterministic (the patch carries the *author* date, so the committer date is the only free
    /// variable in the SHA).
    fn git_am_stdin(dir: &Path, patch: &str, committer_date: Option<&str>) {
        use std::io::Write;
        let mut cmd = Command::new("git");
        cmd.args(["am", "--3way"])
            .current_dir(dir)
            .stdin(std::process::Stdio::piped());
        if let Some(date) = committer_date {
            cmd.env("GIT_COMMITTER_DATE", date);
        }
        let mut child = cmd.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(patch.as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success());
    }

    /// Extract the `Subject:` lines from a `format-patch` series (one per patch).
    fn patch_subjects(patch: &str) -> Vec<String> {
        patch
            .lines()
            .filter(|l| l.starts_with("Subject:"))
            .map(|l| l.trim_start_matches("Subject:").trim().to_string())
            .collect()
    }

    /// The `restack-problem.md` minimal repro: a feature branch that merged `main` into
    /// itself pulls an already-upstream commit (`U2`) into its history. The old
    /// `lkg..branch` range replayed it; the fixed symmetric-difference range must drop it
    /// and replay only the branch's own commits (`F1`, `F2`).
    #[test]
    fn restack_patch_series_drops_already_upstream_commits() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let cache_path = dir.path().join("mb_cache.redb");

        // main: U1, U2
        commit_file(dir.path(), "base.txt", "u1", "U1");
        commit_file(dir.path(), "u2.txt", "u2", "U2");

        // feature forks at U1, commits F1, then merges main (pulling U2 in), then commits F2.
        git(dir.path(), &["checkout", "-q", "-b", "feature", "main~1"]);
        commit_file(dir.path(), "f1.txt", "f1", "F1");
        git(dir.path(), &["merge", "-q", "--no-edit", "main"]);
        commit_file(dir.path(), "f2.txt", "f2", "F2");

        // main advances to U3.
        git(dir.path(), &["checkout", "-q", "main"]);
        commit_file(dir.path(), "u3.txt", "u3", "U3");

        let git_repo = GitRepo::open_with_cache_at(dir.path(), &cache_path).unwrap();
        let patch = git_repo
            .restack_patch_series("main", "feature", None)
            .unwrap()
            .expect("branch has unique work to replay");
        let subjects = patch_subjects(&patch);

        assert!(
            subjects.iter().any(|s| s.contains("F1")),
            "expected F1 in {subjects:?}"
        );
        assert!(
            subjects.iter().any(|s| s.contains("F2")),
            "expected F2 in {subjects:?}"
        );
        assert!(
            !subjects.iter().any(|s| s.contains("U2")),
            "already-upstream U2 must be dropped, got {subjects:?}"
        );
    }

    /// Nested stack: the parent branch `B` was itself rebased onto a new `main`, so its
    /// commit has a new SHA but an unchanged patch-id. Replaying `A` onto `B` must drop the
    /// parent's patch-equivalent commit and replay only `A`'s own commit.
    #[test]
    fn restack_patch_series_drops_patch_equivalent_parent_commits() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let cache_path = dir.path().join("mb_cache.redb");

        // main: U1
        commit_file(dir.path(), "base.txt", "u1", "U1");

        // B forks off main with one commit; A forks off B with one commit.
        git(dir.path(), &["checkout", "-q", "-b", "B"]);
        commit_file(dir.path(), "b.txt", "b1", "B1");
        git(dir.path(), &["checkout", "-q", "-b", "A"]);
        commit_file(dir.path(), "a.txt", "a1", "A1");

        // main advances, and B is rebased onto it: B1 gets a new SHA but the same change.
        git(dir.path(), &["checkout", "-q", "main"]);
        commit_file(dir.path(), "u2.txt", "u2", "U2");
        git(dir.path(), &["checkout", "-q", "B"]);
        git(dir.path(), &["rebase", "-q", "main"]);

        // A still points at the old B1; replay A onto the rebased B.
        let git_repo = GitRepo::open_with_cache_at(dir.path(), &cache_path).unwrap();
        let patch = git_repo
            .restack_patch_series("B", "A", None)
            .unwrap()
            .expect("A has unique work to replay");
        let subjects = patch_subjects(&patch);

        assert_eq!(
            subjects.len(),
            1,
            "only A's own commit should replay, got {subjects:?}"
        );
        assert!(subjects[0].contains("A1"), "expected A1, got {subjects:?}");
    }

    /// A branch whose sole commit was already cherry-picked onto the parent is fully merged
    /// by patch-id: the series is empty and the helper returns `None`.
    #[test]
    fn restack_patch_series_empty_when_fully_merged() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let cache_path = dir.path().join("mb_cache.redb");

        // main: U1
        commit_file(dir.path(), "base.txt", "u1", "U1");

        // feature adds one commit.
        git(dir.path(), &["checkout", "-q", "-b", "feature"]);
        commit_file(dir.path(), "x.txt", "content", "F1");

        // The same change lands on main via cherry-pick (new SHA, same patch-id).
        git(dir.path(), &["checkout", "-q", "main"]);
        git(dir.path(), &["cherry-pick", "feature"]);

        let git_repo = GitRepo::open_with_cache_at(dir.path(), &cache_path).unwrap();
        assert!(
            git_repo
                .restack_patch_series("main", "feature", None)
                .unwrap()
                .is_none(),
            "fully patch-equivalent branch should yield an empty series"
        );
    }

    /// The PLAN.md rewritten-parent bug: the parent branch `p01` is rebuilt with **changed
    /// content** (a conflict resolution rewrote its migration commit). The descendant `env`
    /// was built on the *old* `p01` tip (its `lkg_parent`). The bare `new_parent...env` range
    /// re-replays the superseded old-`p01` commit (not patch-equivalent to the rewritten one),
    /// manufacturing an `add/add` conflict; excluding `^lkg_parent` must drop it so only `env`'s
    /// own commit replays.
    #[test]
    fn restack_patch_series_drops_superseded_rewritten_parent_commits() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let cache_path = dir.path().join("mb_cache.redb");

        // trunk M0; parent p01 adds a migration commit with the OLD content.
        commit_file(dir.path(), "base.txt", "m0", "M0");
        git(dir.path(), &["checkout", "-q", "-b", "p01"]);
        commit_file(
            dir.path(),
            "migration.txt",
            "down_revision=OLD",
            "P01 migration",
        );
        let lkg = git_rev_parse(dir.path(), "p01"); // old parent tip == env's lkg_parent

        // env forks off p01 with its own commit.
        git(dir.path(), &["checkout", "-q", "-b", "env"]);
        commit_file(dir.path(), "env.txt", "env-work", "ENV work");

        // trunk advances; p01 is rebuilt onto it with DIFFERENT migration content.
        git(dir.path(), &["checkout", "-q", "main"]);
        commit_file(dir.path(), "other.txt", "adv", "M1");
        git(dir.path(), &["checkout", "-q", "-B", "p01", "main"]);
        commit_file(
            dir.path(),
            "migration.txt",
            "down_revision=NEW",
            "P01 migration",
        );

        let git_repo = GitRepo::open_with_cache_at(dir.path(), &cache_path).unwrap();

        // Without the exclude, the old-p01 migration commit is wrongly re-introduced.
        let bare = git_repo
            .restack_patch_series("p01", "env", None)
            .unwrap()
            .expect("series is non-empty");
        assert!(
            patch_subjects(&bare)
                .iter()
                .any(|s| s.contains("migration")),
            "sanity: bare range should exhibit the bug by including the superseded parent commit"
        );

        // With `^lkg`, only env's own commit replays.
        let patch = git_repo
            .restack_patch_series("p01", "env", Some(&lkg))
            .unwrap()
            .expect("env has unique work to replay");
        let subjects = patch_subjects(&patch);
        assert_eq!(
            subjects.len(),
            1,
            "only env's own commit should replay, got {subjects:?}"
        );
        assert!(
            subjects[0].contains("ENV work"),
            "expected ENV work, got {subjects:?}"
        );
    }

    /// Churn guard (PLAN "don't churn a branch already correctly stacked on its parent and in sync
    /// with origin"): once a descendant has been restacked onto its (rewritten) parent, `restack`'s
    /// `is_ancestor(parent, branch)` skip must fire on the next invocation so the branch is NOT
    /// re-applied. Re-applying would mint a fresh SHA and sever descendants' descent, re-triggering
    /// the replay-anchor cascade. This reproduces the rewritten-parent scenario, performs the
    /// ApplyMerge fast-path replay of the descendant, and asserts (a) the replay is clean (no
    /// superseded parent commit re-introduced), (b) the skip predicate holds for the whole chain, (c)
    /// an in-sync branch is a true no-op (origin matches, so the loop would not even push), and (d)
    /// the skip is load-bearing — re-applying instead of skipping would churn the SHA.
    #[test]
    fn already_stacked_branch_is_skipped_not_rechurned() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let cache_path = dir.path().join("mb_cache.redb");

        // trunk M0; parent p01 adds a migration commit with the OLD content.
        commit_file(dir.path(), "base.txt", "m0", "M0");
        git(dir.path(), &["checkout", "-q", "-b", "p01"]);
        commit_file(
            dir.path(),
            "migration.txt",
            "down_revision=OLD",
            "P01 migration",
        );
        let lkg = git_rev_parse(dir.path(), "p01"); // old parent tip == env's lkg_parent

        // env forks off p01 with its own commit.
        git(dir.path(), &["checkout", "-q", "-b", "env"]);
        commit_file(dir.path(), "env.txt", "env-work", "ENV work");

        // trunk advances; p01 is rebuilt onto it with DIFFERENT migration content.
        git(dir.path(), &["checkout", "-q", "main"]);
        commit_file(dir.path(), "other.txt", "adv", "M1");
        git(dir.path(), &["checkout", "-q", "-B", "p01", "main"]);
        commit_file(
            dir.path(),
            "migration.txt",
            "down_revision=NEW",
            "P01 migration",
        );

        let git_repo = GitRepo::open_with_cache_at(dir.path(), &cache_path).unwrap();

        // Perform the ApplyMerge fast-path replay of env onto the rewritten p01, exactly as
        // `restack` does: build the `^lkg`-excluded series, reset the branch onto the new parent,
        // then `git am` the series. Pin the committer date so the SHA is deterministic.
        let series = git_repo
            .restack_patch_series("p01", "env", Some(&lkg))
            .unwrap()
            .expect("env has unique work to replay");
        git(dir.path(), &["checkout", "-q", "-B", "env", "p01"]);
        git_am_stdin(dir.path(), &series, Some("2005-04-07T22:13:13"));
        let restacked = git_rev_parse(dir.path(), "env");

        // (a) Clean replay: env carries only its own commit over p01 — the superseded OLD migration
        // commit was not re-introduced.
        let range = format!("{}..{}", git_rev_parse(dir.path(), "p01"), "env");
        let log = Command::new("git")
            .args(["log", "--oneline", "--format=%s", &range])
            .current_dir(dir.path())
            .output()
            .unwrap();
        let subjects: Vec<String> = String::from_utf8(log.stdout)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(
            subjects,
            vec!["ENV work".to_string()],
            "env should carry only its own commit over p01"
        );

        // (b) Skip predicate now holds for the whole chain, so a second `restack` re-applies
        // nothing: p01 is an ancestor of env, and main is an ancestor of p01 (the trunk child is
        // likewise skippable).
        assert!(
            git_repo.is_ancestor("p01", "env").unwrap(),
            "restacked descendant must be recognized as already stacked → skipped, not churned"
        );
        assert!(
            git_repo.is_ancestor("main", "p01").unwrap(),
            "trunk child must be recognized as already stacked → skipped, not churned"
        );

        // (c) In-sync with origin: with origin/env matching env, the skip path's push guard is a
        // true no-op (it only pushes when the branch differs from origin).
        git(dir.path(), &["update-ref", "refs/remotes/origin/env", "env"]);
        assert!(
            git_repo.shas_match("refs/remotes/origin/env", "env"),
            "an in-sync branch must not be pushed by the skip path"
        );

        // (d) The skip is load-bearing: re-running the same replay (a needless re-application) mints
        // a fresh SHA. `restack`'s `is_ancestor` guard short-circuits before this replay, which is
        // exactly what prevents the churn.
        git(dir.path(), &["checkout", "-q", "-B", "env", "p01"]);
        git_am_stdin(dir.path(), &series, Some("2006-04-07T22:13:13"));
        let rechurned = git_rev_parse(dir.path(), "env");
        assert_ne!(
            restacked, rechurned,
            "re-applying instead of skipping churns the SHA — the is_ancestor skip is required"
        );
    }

    /// The `^lkg` exclude must not regress `e6a84ce`: with a **stale** `lkg` that sits behind
    /// `merge-base(branch, main)`, a branch that merged `main` into itself still relies on the
    /// `parent...branch` boundary (not `lkg`) to shed the already-upstream commit. Passing the
    /// stale `lkg` as the exclude must leave `F1`/`F2` and still drop `U2`.
    #[test]
    fn restack_patch_series_lkg_exclude_keeps_merged_main_dropped() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let cache_path = dir.path().join("mb_cache.redb");

        // main: U1 (stale lkg sits here), then U2.
        commit_file(dir.path(), "base.txt", "u1", "U1");
        let stale_lkg = git_rev_parse(dir.path(), "main");
        commit_file(dir.path(), "u2.txt", "u2", "U2");

        // feature forks at U1, commits F1, merges main (pulling U2 in), then commits F2.
        git(dir.path(), &["checkout", "-q", "-b", "feature", "main~1"]);
        commit_file(dir.path(), "f1.txt", "f1", "F1");
        git(dir.path(), &["merge", "-q", "--no-edit", "main"]);
        commit_file(dir.path(), "f2.txt", "f2", "F2");

        // main advances to U3.
        git(dir.path(), &["checkout", "-q", "main"]);
        commit_file(dir.path(), "u3.txt", "u3", "U3");

        let git_repo = GitRepo::open_with_cache_at(dir.path(), &cache_path).unwrap();
        let patch = git_repo
            .restack_patch_series("main", "feature", Some(&stale_lkg))
            .unwrap()
            .expect("branch has unique work to replay");
        let subjects = patch_subjects(&patch);

        assert!(
            subjects.iter().any(|s| s.contains("F1")) && subjects.iter().any(|s| s.contains("F2")),
            "expected F1 and F2 in {subjects:?}"
        );
        assert!(
            !subjects.iter().any(|s| s.contains("U2")),
            "already-upstream U2 must still be dropped with a stale lkg exclude, got {subjects:?}"
        );
    }

    /// Two divergent branches: `feature` is NOT an ancestor of `main`, and their only common
    /// ancestor is the root commit.
    fn init_divergent_repo(dir: &Path) {
        git(dir, &["init", "-q", "-b", "main"]);
        git(dir, &["config", "user.email", "test@example.com"]);
        git(dir, &["config", "user.name", "Test"]);
        disable_auto_maintenance(dir);
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

    /// Run a git command, returning whether it succeeded (does not assert). Used for commands
    /// that are expected to fail, e.g. `git am --3way` hitting a conflict.
    fn git_ok(dir: &Path, args: &[&str]) -> bool {
        Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap()
            .status
            .success()
    }

    /// A clean working tree with no staged changes: the index tree equals HEAD.
    #[test]
    fn staged_matches_head_true_when_index_equals_head() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let cache_path = dir.path().join("mb_cache.redb");
        commit_file(dir.path(), "a.txt", "1", "a1");

        let git_repo = GitRepo::open_with_cache_at(dir.path(), &cache_path).unwrap();
        assert!(git_repo.staged_matches_head().unwrap());
    }

    /// A staged content change makes the index differ from HEAD.
    #[test]
    fn staged_matches_head_false_with_staged_change() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let cache_path = dir.path().join("mb_cache.redb");
        commit_file(dir.path(), "a.txt", "1", "a1");

        std::fs::write(dir.path().join("a.txt"), "2").unwrap();
        git(dir.path(), &["add", "a.txt"]);

        let git_repo = GitRepo::open_with_cache_at(dir.path(), &cache_path).unwrap();
        assert!(!git_repo.staged_matches_head().unwrap());

        // Re-staging the committed content restores index == HEAD.
        std::fs::write(dir.path().join("a.txt"), "1").unwrap();
        git(dir.path(), &["add", "a.txt"]);
        assert!(git_repo.staged_matches_head().unwrap());
    }

    /// The empty-patch scenario: a superseded commit is replayed via `git am --3way`, hits an
    /// add/add conflict, and the user resolves it by keeping the parent's version. The staged
    /// tree then equals HEAD, so `staged_matches_head` reports the patch is empty (must be
    /// `git am --skip`ped, not `--continue`d). While the conflict is unresolved it reports false.
    #[test]
    fn staged_matches_head_after_resolving_am_patch_to_parent() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let cache_path = dir.path().join("mb_cache.redb");

        // base commit shared by both sides.
        commit_file(dir.path(), "base.txt", "base", "base");

        // feature adds x.txt with the "feature" content; capture just that commit as a patch.
        git(dir.path(), &["checkout", "-q", "-b", "feature"]);
        commit_file(dir.path(), "x.txt", "feature version\n", "add x");
        let patch = {
            let out = Command::new("git")
                .args(["format-patch", "--stdout", "main..feature"])
                .current_dir(dir.path())
                .output()
                .unwrap();
            assert!(out.status.success());
            String::from_utf8(out.stdout).unwrap()
        };
        let patch_path = dir.path().join("feature.patch");
        std::fs::write(&patch_path, &patch).unwrap();

        // main independently adds x.txt with different ("parent") content, so replaying the
        // feature patch add/add-conflicts on x.txt.
        git(dir.path(), &["checkout", "-q", "main"]);
        commit_file(dir.path(), "x.txt", "parent version\n", "add x (parent)");

        let applied = git_ok(dir.path(), &["am", "--3way", patch_path.to_str().unwrap()]);
        assert!(!applied, "expected the replayed add to conflict");

        let git_repo = GitRepo::open_with_cache_at(dir.path(), &cache_path).unwrap();
        // Unresolved conflict: not an empty patch.
        assert!(!git_repo.staged_matches_head().unwrap());

        // Resolve by keeping the parent's version (`--ours` == HEAD during `git am`).
        git(dir.path(), &["checkout", "--ours", "x.txt"]);
        git(dir.path(), &["add", "x.txt"]);

        // The patch's only change is now superseded — the staged tree equals HEAD.
        assert!(git_repo.staged_matches_head().unwrap());

        let _ = git_ok(dir.path(), &["am", "--abort"]);
    }

    /// `am_in_progress` tracks git's live state: true while a `git am` sits on a conflict, false
    /// once the user finishes it by hand (`git am --skip` completes the single-patch series). This
    /// is what lets `restack --continue`/`--skip` resume instead of erroring on an already-done am.
    #[test]
    fn am_in_progress_reflects_git_am_state() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let cache_path = dir.path().join("mb_cache.redb");

        commit_file(dir.path(), "base.txt", "base", "base");

        // feature adds x.txt; capture it as a patch.
        git(dir.path(), &["checkout", "-q", "-b", "feature"]);
        commit_file(dir.path(), "x.txt", "feature version\n", "add x");
        let patch_path = dir.path().join("feature.patch");
        {
            let out = Command::new("git")
                .args(["format-patch", "--stdout", "main..feature"])
                .current_dir(dir.path())
                .output()
                .unwrap();
            assert!(out.status.success());
            std::fs::write(&patch_path, out.stdout).unwrap();
        }

        // main adds x.txt with different content so replaying the patch add/add-conflicts.
        git(dir.path(), &["checkout", "-q", "main"]);
        commit_file(dir.path(), "x.txt", "parent version\n", "add x (parent)");

        let git_repo = GitRepo::open_with_cache_at(dir.path(), &cache_path).unwrap();
        assert!(!git_repo.am_in_progress(), "no am before it starts");

        let applied = git_ok(dir.path(), &["am", "--3way", patch_path.to_str().unwrap()]);
        assert!(!applied, "expected the replayed add to conflict");
        assert!(
            git_repo.am_in_progress(),
            "am should be in progress mid-conflict"
        );

        // Finish it by hand: skipping the only patch completes the series.
        git(dir.path(), &["am", "--skip"]);
        assert!(
            !git_repo.am_in_progress(),
            "am should be finished after `git am --skip`"
        );
    }

    /// `rebase_in_progress` tracks git's live state: true while a `git rebase` sits on a conflict,
    /// false after the user aborts (or otherwise finishes) it.
    #[test]
    fn rebase_in_progress_reflects_git_rebase_state() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        let cache_path = dir.path().join("mb_cache.redb");

        // main: base.txt="base". feature forks and edits it; main then edits it differently, so
        // rebasing feature onto main conflicts on base.txt.
        commit_file(dir.path(), "base.txt", "base\n", "base");
        git(dir.path(), &["checkout", "-q", "-b", "feature"]);
        commit_file(dir.path(), "base.txt", "feature\n", "feature edit");
        git(dir.path(), &["checkout", "-q", "main"]);
        commit_file(dir.path(), "base.txt", "main edit\n", "main edit");

        let git_repo = GitRepo::open_with_cache_at(dir.path(), &cache_path).unwrap();
        assert!(!git_repo.rebase_in_progress(), "no rebase before it starts");

        git(dir.path(), &["checkout", "-q", "feature"]);
        let rebased = git_ok(dir.path(), &["rebase", "main"]);
        assert!(!rebased, "expected the rebase to conflict");
        assert!(
            git_repo.rebase_in_progress(),
            "rebase should be in progress mid-conflict"
        );

        git(dir.path(), &["rebase", "--abort"]);
        assert!(
            !git_repo.rebase_in_progress(),
            "rebase should be finished after `git rebase --abort`"
        );
    }
}
