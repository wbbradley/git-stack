//! git2 wrapper module for fast read-only git operations.
//!
//! This module provides a `GitRepo` struct that wraps git2::Repository
//! for fast read-only operations without spawning git processes.

use std::{path::Path, time::Instant};

use anyhow::{Context, Result, anyhow};
use git2::{BranchType, Repository};

use crate::stats::GitBenchmark;

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
}

impl GitRepo {
    /// Open a repository at the given path.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let _bench = GitBenchmark::start("git2:open");
        let repo = Repository::open(path.as_ref())
            .with_context(|| format!("Failed to open repository at {:?}", path.as_ref()))?;
        Ok(Self { repo })
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
        let _bench = GitBenchmark::start("git2:merge-base");
        let ancestor_obj = self
            .repo
            .revparse_single(ancestor)
            .with_context(|| format!("Failed to resolve ancestor ref: {}", ancestor))?;
        let descendant_obj = self
            .repo
            .revparse_single(descendant)
            .with_context(|| format!("Failed to resolve descendant ref: {}", descendant))?;

        // A commit is considered an ancestor of itself (matches git behavior)
        Ok(ancestor_obj.id() == descendant_obj.id()
            || self
                .repo
                .graph_descendant_of(descendant_obj.id(), ancestor_obj.id())
                .unwrap_or(false))
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

    pub fn branch_status(
        &self,
        parent_branch: Option<&str>,
        branch: &str,
    ) -> Result<GitBranchStatus> {
        let exists = self.branch_exists(branch);
        let parent_branch = match parent_branch {
            Some(parent_branch) => parent_branch.to_string(),
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
            let upstream_status =
                upstream_symbolic_name.map(|symbolic_name| UpstreamStatus {
                    symbolic_name,
                    synced: upstream_synced,
                });
            (sha, is_descendent, upstream_status)
        } else {
            // Branch doesn't exist - use placeholder values
            (String::new(), false, None)
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
