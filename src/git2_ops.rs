//! git2 wrapper module for fast read-only git operations.
//!
//! This module provides a `GitRepo` struct that wraps git2::Repository
//! for fast read-only operations without spawning git processes.

use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use git2::{BranchType, Repository};

use crate::stats::record_git_command;

/// Wrapper around git2::Repository for fast read-only git operations.
pub struct GitRepo {
    repo: Repository,
}

impl GitRepo {
    /// Open a repository at the given path.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let start = Instant::now();
        let repo = Repository::open(path.as_ref())
            .with_context(|| format!("Failed to open repository at {:?}", path.as_ref()))?;
        record_git_command(&["git2:open"], start.elapsed());
        Ok(Self { repo })
    }

    /// Get the SHA of a reference (branch name, tag, or other ref).
    /// Equivalent to `git rev-parse <ref>`
    pub fn sha(&self, ref_name: &str) -> Result<String> {
        let start = Instant::now();
        let obj = self
            .repo
            .revparse_single(ref_name)
            .with_context(|| format!("Failed to resolve ref: {}", ref_name))?;
        let sha = obj.id().to_string();
        record_git_command(&["git2:rev-parse", ref_name], start.elapsed());
        Ok(sha)
    }

    /// Check if ancestor_ref is an ancestor of descendant_ref.
    /// Equivalent to `git merge-base --is-ancestor <ancestor> <descendant>`
    pub fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
        let start = Instant::now();
        let ancestor_obj = self
            .repo
            .revparse_single(ancestor)
            .with_context(|| format!("Failed to resolve ancestor ref: {}", ancestor))?;
        let descendant_obj = self
            .repo
            .revparse_single(descendant)
            .with_context(|| format!("Failed to resolve descendant ref: {}", descendant))?;
        let result = self
            .repo
            .graph_descendant_of(descendant_obj.id(), ancestor_obj.id())
            .unwrap_or(false);
        record_git_command(&["git2:merge-base", ancestor, descendant], start.elapsed());
        Ok(result)
    }

    /// Check if a local branch exists.
    /// Equivalent to `git rev-parse --verify <branch>`
    pub fn branch_exists(&self, branch: &str) -> bool {
        let start = Instant::now();
        // First try as a local branch, then try direct ref resolution
        let exists = self.repo.find_branch(branch, BranchType::Local).is_ok()
            || self.repo.revparse_single(branch).is_ok();
        record_git_command(&["git2:branch-exists", branch], start.elapsed());
        exists
    }

    /// Get the remote main branch name (e.g., "origin/main").
    /// Equivalent to `git symbolic-ref refs/remotes/<remote>/HEAD`
    pub fn remote_main(&self, remote: &str) -> Result<String> {
        let start = Instant::now();
        let ref_name = format!("refs/remotes/{}/HEAD", remote);
        let reference = self
            .repo
            .find_reference(&ref_name)
            .with_context(|| format!("Failed to find remote HEAD: {}", ref_name))?;

        let target = reference
            .symbolic_target()
            .ok_or_else(|| anyhow!("{} is not a symbolic reference", ref_name))?;

        let result = target
            .strip_prefix("refs/remotes/")
            .unwrap_or(target)
            .to_string();

        record_git_command(&["git2:symbolic-ref", remote], start.elapsed());
        Ok(result)
    }

    /// Check if two refs point to the same commit.
    /// Equivalent to comparing `git rev-parse <ref1>` and `git rev-parse <ref2>`
    pub fn shas_match(&self, ref1: &str, ref2: &str) -> bool {
        let start = Instant::now();
        let result = (|| {
            let obj1 = self.repo.revparse_single(ref1).ok()?;
            let obj2 = self.repo.revparse_single(ref2).ok()?;
            Some(obj1.id() == obj2.id())
        })()
        .unwrap_or(false);
        record_git_command(&["git2:shas-match", ref1, ref2], start.elapsed());
        result
    }

    /// Get the repo root path.
    /// Equivalent to `git rev-parse --show-toplevel`
    pub fn root(&self) -> Result<String> {
        let start = Instant::now();
        let workdir = self
            .repo
            .workdir()
            .ok_or_else(|| anyhow!("Repository has no working directory"))?;
        let result = workdir
            .to_str()
            .ok_or_else(|| anyhow!("Invalid path encoding"))?
            .trim_end_matches('/')
            .to_string();
        record_git_command(&["git2:show-toplevel"], start.elapsed());
        Ok(result)
    }

    /// Get current branch name.
    /// Equivalent to `git rev-parse --abbrev-ref HEAD`
    pub fn current_branch(&self) -> Result<String> {
        let start = Instant::now();
        let head = self.repo.head().context("Failed to get HEAD")?;
        let branch_name = if head.is_branch() {
            head.shorthand()
                .ok_or_else(|| anyhow!("HEAD has no shorthand name"))?
                .to_string()
        } else {
            // Detached HEAD - return the SHA
            head.target()
                .ok_or_else(|| anyhow!("HEAD has no target"))?
                .to_string()
        };
        record_git_command(&["git2:current-branch"], start.elapsed());
        Ok(branch_name)
    }

    /// Get the upstream tracking branch for a local branch.
    /// Equivalent to `git rev-parse --abbrev-ref --symbolic-full-name <branch>@{upstream}`
    pub fn get_upstream(&self, branch: &str) -> Option<String> {
        let start = Instant::now();
        let result = (|| {
            let local_branch = self.repo.find_branch(branch, BranchType::Local).ok()?;
            let upstream = local_branch.upstream().ok()?;
            let name = upstream.name().ok()??;
            Some(name.to_string())
        })();
        record_git_command(&["git2:get-upstream", branch], start.elapsed());
        result
    }

    /// Get diff stats (additions, deletions) between two commits.
    /// Equivalent to parsing `git log --numstat --pretty="" <base>..<head>`
    pub fn diff_stats(&self, base: &str, head: &str) -> Result<(usize, usize)> {
        let start = Instant::now();

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
        let additions = stats.insertions();
        let deletions = stats.deletions();

        record_git_command(&["git2:diff-stats", base, head], start.elapsed());
        Ok((additions, deletions))
    }
}
