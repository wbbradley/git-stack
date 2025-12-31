//! git2 wrapper module for fast read-only git operations.
//!
//! This module provides git2-based implementations of common read-only git operations
//! to avoid the overhead of spawning git processes.

use std::cell::RefCell;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result, anyhow};
use git2::{BranchType, Repository};

use crate::stats::record_git_command;

// Thread-local repository instance - opened once per invocation
thread_local! {
    static REPO: RefCell<Option<Repository>> = const { RefCell::new(None) };
}

/// Initialize the git2 repository for the given path.
/// Must be called early in main() before using other git2_ops functions.
pub fn init_repo(path: &str) -> Result<()> {
    let start = Instant::now();
    REPO.with(|r| {
        let repo = Repository::open(path).context("Failed to open repository with git2")?;
        *r.borrow_mut() = Some(repo);
        record_git_command(&["git2:open"], start.elapsed());
        Ok(())
    })
}

/// Check if git2 repo is initialized
pub fn is_initialized() -> bool {
    REPO.with(|r| r.borrow().is_some())
}

/// Get the SHA of a reference (branch name, tag, or other ref).
/// Equivalent to `git rev-parse <ref>`
pub fn git_sha(ref_name: &str) -> Result<String> {
    let start = Instant::now();
    REPO.with(|r| {
        let binding = r.borrow();
        let repo = binding
            .as_ref()
            .ok_or_else(|| anyhow!("git2 repo not initialized"))?;

        let obj = repo
            .revparse_single(ref_name)
            .with_context(|| format!("Failed to resolve ref: {}", ref_name))?;

        let sha = obj.id().to_string();
        record_git_command(&["git2:rev-parse", ref_name], start.elapsed());
        Ok(sha)
    })
}

/// Check if ancestor_ref is an ancestor of descendant_ref.
/// Equivalent to `git merge-base --is-ancestor <ancestor> <descendant>`
pub fn is_ancestor(ancestor: &str, descendant: &str) -> Result<bool> {
    let start = Instant::now();
    REPO.with(|r| {
        let binding = r.borrow();
        let repo = binding
            .as_ref()
            .ok_or_else(|| anyhow!("git2 repo not initialized"))?;

        let ancestor_obj = repo
            .revparse_single(ancestor)
            .with_context(|| format!("Failed to resolve ancestor ref: {}", ancestor))?;
        let descendant_obj = repo
            .revparse_single(descendant)
            .with_context(|| format!("Failed to resolve descendant ref: {}", descendant))?;

        let result = repo
            .graph_descendant_of(descendant_obj.id(), ancestor_obj.id())
            .unwrap_or(false);

        record_git_command(&["git2:merge-base", ancestor, descendant], start.elapsed());
        Ok(result)
    })
}

/// Check if a local branch exists.
/// Equivalent to `git rev-parse --verify <branch>`
pub fn branch_exists(branch: &str) -> Result<bool> {
    let start = Instant::now();
    REPO.with(|r| {
        let binding = r.borrow();
        let repo = binding
            .as_ref()
            .ok_or_else(|| anyhow!("git2 repo not initialized"))?;

        // First try as a local branch
        let exists = repo.find_branch(branch, BranchType::Local).is_ok()
            // Also try direct ref resolution (handles detached HEAD, tags, etc.)
            || repo.revparse_single(branch).is_ok();

        record_git_command(&["git2:branch-exists", branch], start.elapsed());
        Ok(exists)
    })
}

/// Get the remote main branch name (e.g., "origin/main").
/// Equivalent to `git symbolic-ref refs/remotes/<remote>/HEAD`
pub fn remote_main(remote: &str) -> Result<String> {
    let start = Instant::now();
    REPO.with(|r| {
        let binding = r.borrow();
        let repo = binding
            .as_ref()
            .ok_or_else(|| anyhow!("git2 repo not initialized"))?;

        let ref_name = format!("refs/remotes/{}/HEAD", remote);
        let reference = repo
            .find_reference(&ref_name)
            .with_context(|| format!("Failed to find remote HEAD: {}", ref_name))?;

        // Resolve symbolic reference to its target
        let target = reference
            .symbolic_target()
            .ok_or_else(|| anyhow!("{} is not a symbolic reference", ref_name))?;

        // Strip "refs/remotes/" prefix
        let result = target
            .strip_prefix("refs/remotes/")
            .unwrap_or(target)
            .to_string();

        record_git_command(&["git2:symbolic-ref", remote], start.elapsed());
        Ok(result)
    })
}

/// Check if two refs point to the same commit.
/// Equivalent to comparing `git rev-parse <ref1>` and `git rev-parse <ref2>`
pub fn shas_match(ref1: &str, ref2: &str) -> bool {
    let start = Instant::now();
    let result = REPO.with(|r| {
        let binding = r.borrow();
        let repo = binding.as_ref()?;

        let obj1 = repo.revparse_single(ref1).ok()?;
        let obj2 = repo.revparse_single(ref2).ok()?;

        Some(obj1.id() == obj2.id())
    });
    record_git_command(&["git2:shas-match", ref1, ref2], start.elapsed());
    result.unwrap_or(false)
}

/// Get the repo root path.
/// Equivalent to `git rev-parse --show-toplevel`
pub fn repo_root() -> Result<String> {
    let start = Instant::now();
    REPO.with(|r| {
        let binding = r.borrow();
        let repo = binding
            .as_ref()
            .ok_or_else(|| anyhow!("git2 repo not initialized"))?;

        let workdir = repo
            .workdir()
            .ok_or_else(|| anyhow!("Repository has no working directory"))?;

        let result = workdir
            .to_str()
            .ok_or_else(|| anyhow!("Invalid path encoding"))?
            .trim_end_matches('/')
            .to_string();

        record_git_command(&["git2:show-toplevel"], start.elapsed());
        Ok(result)
    })
}

/// Get current branch name.
/// Equivalent to `git rev-parse --abbrev-ref HEAD`
pub fn current_branch() -> Result<String> {
    let start = Instant::now();
    REPO.with(|r| {
        let binding = r.borrow();
        let repo = binding
            .as_ref()
            .ok_or_else(|| anyhow!("git2 repo not initialized"))?;

        let head = repo.head().context("Failed to get HEAD")?;

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
    })
}

/// Get the upstream tracking branch for a local branch.
/// Equivalent to `git rev-parse --abbrev-ref --symbolic-full-name <branch>@{upstream}`
pub fn get_upstream(branch: &str) -> Option<String> {
    let start = Instant::now();
    let result = REPO.with(|r| {
        let binding = r.borrow();
        let repo = binding.as_ref()?;

        let local_branch = repo.find_branch(branch, BranchType::Local).ok()?;
        let upstream = local_branch.upstream().ok()?;
        let name = upstream.name().ok()??;

        Some(name.to_string())
    });
    record_git_command(&["git2:get-upstream", branch], start.elapsed());
    result
}

/// Get diff stats (additions, deletions) between two commits.
/// Equivalent to parsing `git log --numstat --pretty="" <base>..<head>`
pub fn diff_stats(base: &str, head: &str) -> Result<(usize, usize)> {
    let start = Instant::now();
    REPO.with(|r| {
        let binding = r.borrow();
        let repo = binding
            .as_ref()
            .ok_or_else(|| anyhow!("git2 repo not initialized"))?;

        // Resolve refs to commits
        let base_obj = repo
            .revparse_single(base)
            .with_context(|| format!("Failed to resolve base ref: {}", base))?;
        let head_obj = repo
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

        // Create diff between the two trees
        let diff = repo.diff_tree_to_tree(Some(&base_tree), Some(&head_tree), None)?;

        // Get diff stats
        let stats = diff.stats()?;
        let additions = stats.insertions();
        let deletions = stats.deletions();

        record_git_command(&["git2:diff-stats", base, head], start.elapsed());
        Ok((additions, deletions))
    })
}
