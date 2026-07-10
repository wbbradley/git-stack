use std::{
    cell::{Cell, Ref, RefCell},
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    default, fs,
    io::IsTerminal,
    path::{Path, PathBuf},
    process::Command,
    rc::Rc,
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use colored::Colorize;
use serde::{Deserialize, Serialize};

use crate::{
    git::{GitTrunk, after_text, git_branch_exists, git_trunk},
    git2_ops::{DEFAULT_REMOTE, GitRepo},
    run_git,
};

/// Write a file with secure permissions (0600 on Unix).
/// This ensures sensitive config and state files are only readable by the owner.
#[cfg(unix)]
pub fn write_file_secure(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::write(path, contents)?;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(0o600);
    fs::set_permissions(path, perms)?;
    Ok(())
}

/// Write a file (non-Unix platforms don't have the same permission model).
#[cfg(not(unix))]
pub fn write_file_secure(path: &Path, contents: &str) -> std::io::Result<()> {
    fs::write(path, contents)
}

#[derive(Clone, Copy, Default, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum StackMethod {
    /// Uses `git format-patch` and `git am` to restack branches.
    #[default]
    ApplyMerge,
    /// Uses `git merge` to pull in changes from the parent branch.
    Merge,
}

/// Which restack mechanic was in progress when a conflict interrupted it. Determines the
/// `--abort`/`--continue` mechanics the handlers run.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum RestackMethod {
    /// `format-patch` + `git am` fast path.
    Am,
    /// Fallback `git rebase`.
    Rebase,
    /// `git merge` (the `Merge` stack method).
    Merge,
    /// `git merge --squash` into a temp branch (`-s`/`--squash`).
    Squash,
}

/// Enough of the original `restack` invocation to resume the remaining plan after a conflict.
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct RestackResume {
    /// The user's target branch for this restack.
    pub restack_branch: String,
    /// The branch to return to at the end.
    pub orig_branch: String,
    /// Whether the original invocation restacked the whole ancestor chain.
    pub ancestors: bool,
    /// Whether the original invocation pushed after each branch.
    pub push: bool,
    /// Whether the original invocation was a squash restack.
    pub squash: bool,
}

/// A restack operation interrupted by a conflict, awaiting `--continue`/`--abort`.
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct PendingRestackOperation {
    /// Which git mechanic was in progress when the conflict hit.
    pub method: RestackMethod,
    /// The conflicting branch.
    pub branch_name: String,
    /// The new target parent branch/ref the branch was being restacked onto.
    pub parent: String,
    /// The branch tip before the restack moved it (recovery anchor).
    pub original_sha: String,
    /// Squash-only: temp branch used during `merge --squash`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmp_branch_name: Option<String>,
    /// Squash-only: concatenated commit messages for the squash commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub squash_message: Option<String>,
    /// The original invocation parameters, so `--continue` can resume the remaining plan.
    pub resume: RestackResume,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Branch {
    /// The name of the branch or ref.
    pub name: String,
    /// The method used to stack the branch. When sharing a branch with others, it is recommended
    /// to use `Merge` and to avoid `git push -f` to prevent accidental data loss. When coding solo
    /// in a branch, `ApplyMerge` is recommended to keep the history clean.
    pub stack_method: StackMethod,
    /// Notes associated with the branch. This is a free-form field that can be used to store
    /// anything.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The last-known-good parent of the branch. For use in restacking or moving branches.
    pub lkg_parent: Option<String>,
    /// The GitHub PR number associated with this branch, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<u64>,
    /// The upstream branch reference.
    pub branches: Vec<Branch>,
}

impl Branch {
    pub fn new(name: String, lkg_parent: Option<String>) -> Self {
        Self {
            name,
            note: None,
            stack_method: StackMethod::default(),
            lkg_parent,
            pr_number: None,
            branches: vec![],
        }
    }
}

/// Per-repository state including the branch tree and seen remote SHAs.
#[derive(Debug, Serialize, Deserialize)]
pub struct RepoState {
    /// The root of the branch tree (usually trunk/main).
    #[serde(flatten)]
    pub tree: Branch,
    /// SHAs that have been seen on the remote. Used to safely determine if a local
    /// branch can be deleted (no unpushed work would be lost).
    #[serde(default, skip_serializing_if = "HashSet::is_empty")]
    pub seen_remote_shas: HashSet<String>,
    /// Pending restack operation that needs to be resumed/aborted after conflict resolution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_restack: Option<PendingRestackOperation>,
}

impl RepoState {
    pub fn new(tree: Branch) -> Self {
        Self {
            tree,
            seen_remote_shas: HashSet::new(),
            pending_restack: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct State {
    /// The directory name is the key, and the value is the repo state.
    #[serde(flatten, default)]
    pub repos: BTreeMap<String, RepoState>,
}

impl State {
    pub fn load_state() -> Result<Self> {
        let state_path = get_xdg_path()?;
        let mut used_existing_state = true;
        let data = match fs::read_to_string(&state_path) {
            Ok(data) => data,
            Err(error) => {
                tracing::warn!(
                    "Failed to read config file at {}: {}",
                    state_path.display(),
                    error
                );
                tracing::warn!("Using default (empty) config");
                used_existing_state = false;
                "".to_string()
            }
        };
        let state: Self = serde_yaml::from_str(&data)
            .with_context(|| format!("parsing state file: {:?}", state_path))?;
        fs::create_dir_all(state_path.parent().unwrap())
            .inspect_err(|error| tracing::warn!("Failed to create config directory: {}", error))?;
        if !used_existing_state {
            tracing::info!("No existing config file found, creating a new one.");
            state
                .save_state()
                .inspect_err(|error| tracing::warn!("Failed to save config file: {}", error))?;
        }
        Ok(state)
    }

    pub fn save_state(&self) -> Result<()> {
        let state_path = get_xdg_path()?;
        tracing::trace!(?self, ?state_path, "Saving state to config file");
        Ok(write_file_secure(
            &state_path,
            &serde_yaml::to_string(&self)?,
        )?)
    }

    pub fn get_tree(&self, repo: &str) -> Option<&Branch> {
        self.repos.get(repo).map(|r| &r.tree)
    }
    pub fn get_tree_mut(&mut self, repo: &str) -> Option<&mut Branch> {
        self.repos.get_mut(repo).map(|r| &mut r.tree)
    }
    pub fn get_repo_state(&self, repo: &str) -> Option<&RepoState> {
        self.repos.get(repo)
    }
    pub fn get_repo_state_mut(&mut self, repo: &str) -> Option<&mut RepoState> {
        self.repos.get_mut(repo)
    }
    /// Record a SHA as having been seen on the remote for a given repo.
    pub fn add_seen_sha(&mut self, repo: &str, sha: String) {
        if let Some(repo_state) = self.repos.get_mut(repo) {
            repo_state.seen_remote_shas.insert(sha);
        }
    }
    /// Get all seen SHAs for a repo.
    pub fn get_seen_shas(&self, repo: &str) -> Option<&HashSet<String>> {
        self.repos.get(repo).map(|r| &r.seen_remote_shas)
    }
    /// Clear all seen SHAs for a repo.
    pub fn clear_seen_shas(&mut self, repo: &str) {
        if let Some(repo_state) = self.repos.get_mut(repo) {
            repo_state.seen_remote_shas.clear();
        }
    }
    /// Get the pending restack operation for a repo, if any.
    pub fn get_pending_restack(&self, repo: &str) -> Option<&PendingRestackOperation> {
        self.repos
            .get(repo)
            .and_then(|r| r.pending_restack.as_ref())
    }
    /// Set or clear the pending restack operation for a repo.
    pub fn set_pending_restack(&mut self, repo: &str, pending: Option<PendingRestackOperation>) {
        if let Some(repo_state) = self.repos.get_mut(repo) {
            repo_state.pending_restack = pending;
        }
    }
    /// Check if there is a pending restack operation for a repo.
    pub fn has_pending_restack(&self, repo: &str) -> bool {
        self.repos
            .get(repo)
            .map(|r| r.pending_restack.is_some())
            .unwrap_or(false)
    }
    /// If there is an existing git-stack branch with the same name, check it out. If there isn't,
    /// then check whether the branch exists in the git repo. If it does, then let the user know
    /// that they need to use `git checkout` to check it out. If it doesn't, then create a new
    /// branch.
    ///
    /// For branches tracked in git-stack but not existing locally, this will create the local
    /// branch from the remote ref (origin/branch_name) on-demand.
    pub fn checkout(
        &mut self,
        git_repo: &GitRepo,
        repo: &str,
        current_branch: String,
        current_upstream: Option<String>,
        branch_name: String,
    ) -> Result<()> {
        // Ensure the main branch is in the git-stack tree for this repo if we haven't
        // added it yet (only if we have a remote configured).
        if let Some(trunk) = git_trunk(git_repo) {
            self.repos
                .entry(repo.to_string())
                .or_insert_with(|| RepoState::new(Branch::new(trunk.main_branch.clone(), None)));
            self.save_state()?;
        }

        let branch_exists_in_tree = self.branch_exists_in_tree(repo, &branch_name);
        let branch_exists_locally = git_branch_exists(git_repo, &branch_name);

        // Case 1: Branch exists locally - just check it out
        if branch_exists_locally {
            if !branch_exists_in_tree {
                tracing::warn!(
                    "Branch {branch_name} exists in the git repo but is not tracked by git-stack. \
                    If you'd like to add it to the git-stack, please run `git-stack mount \
                    <parent-branch>` to stack {branch_name} on top of the parent branch.",
                );
            }
            run_git(&["checkout", &branch_name])?;
            return Ok(());
        }

        // Case 2: Branch is in tree but doesn't exist locally - create from remote
        if branch_exists_in_tree {
            let remote_ref = format!("origin/{}", branch_name);
            if git_repo.ref_exists(&remote_ref) {
                // Create local branch from remote ref
                run_git(&["checkout", "-b", &branch_name, &remote_ref])?;
                println!(
                    "Branch {branch_name} created from remote and checked out.",
                    branch_name = branch_name.yellow()
                );
                return Ok(());
            } else {
                bail!(
                    "Branch {branch_name} is tracked by git-stack but doesn't exist locally or on remote.",
                    branch_name = branch_name.red()
                );
            }
        }

        // Case 3: Branch doesn't exist anywhere - create a new branch from current
        let branch = self
            .get_tree_branch_mut(repo, &current_branch)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Branch '{current_branch}' is not being tracked in the git-stack tree."
                )
            })?;

        branch.branches.push(Branch::new(
            branch_name.clone(),
            git_repo.sha(&current_branch).ok(),
        ));

        // Actually create the git branch.
        run_git(&["checkout", "-b", &branch_name, &current_branch])?;

        println!(
            "Branch {branch_name} created and checked out.",
            branch_name = branch_name.yellow()
        );

        // Save the state after modifying it.
        self.save_state()?;

        Ok(())
    }

    pub fn branch_exists_in_tree(&self, repo: &str, branch_name: &str) -> bool {
        let Some(repo_state) = self.repos.get(repo) else {
            return false;
        };
        is_branch_mentioned_in_tree(branch_name, &repo_state.tree)
    }

    pub fn get_tree_branch<'a>(&'a self, repo: &str, branch_name: &str) -> Option<&'a Branch> {
        self.repos
            .get(repo)
            .and_then(|r| find_branch_by_name(&r.tree, branch_name))
    }

    fn get_tree_branch_mut<'a>(
        &'a mut self,
        repo: &str,
        branch_name: &str,
    ) -> Option<&'a mut Branch> {
        self.repos
            .get_mut(repo)
            .and_then(|r| find_branch_by_name_mut(&mut r.tree, branch_name))
    }

    pub(crate) fn plan_restack(
        &'_ self,
        git_repo: &GitRepo,
        repo: &str,
        starting_branch: &str,
        ancestors: bool,
    ) -> Result<Vec<RestackStep<'_>>> {
        tracing::debug!("Planning restack for {starting_branch} (ancestors={ancestors})");

        // Single-step mode: only restack the target branch onto its immediate parent
        if !ancestors {
            let parent = self
                .get_parent_branch_of(repo, starting_branch)
                .ok_or_else(|| {
                    anyhow!("Branch {starting_branch} not found in the git-stack tree.")
                })?;

            let branch = self.get_tree_branch(repo, starting_branch).ok_or_else(|| {
                anyhow!("Branch {starting_branch} not found in the git-stack tree.")
            })?;

            // Resolve parent ref to local or origin/branch
            let parent_ref = git_repo
                .resolve_branch_ref(&parent.name)
                .unwrap_or_else(|| parent.name.clone());

            return Ok(vec![RestackStep {
                parent: parent_ref,
                branch,
            }]);
        }

        // All-parents mode: restack the entire ancestry chain
        let mut path: Vec<&Branch> = vec![];
        let repo_state = self
            .repos
            .get(repo)
            .ok_or_else(|| anyhow!("Repo not found"))?;
        if !get_path(&repo_state.tree, starting_branch, &mut path) {
            bail!("Branch {starting_branch} not found in the git-stack tree.");
        }
        Ok(path
            .iter()
            .zip(path.iter().skip(1))
            .map(|(parent, child)| RestackStep {
                parent: parent.name.to_string(),
                branch: child,
            })
            .collect::<Vec<_>>())
    }

    pub(crate) fn delete_branch(&mut self, repo: &str, branch_name: &str) -> Result<()> {
        let Some(parent) = self
            .repos
            .get_mut(repo)
            .and_then(|r| find_parent_of_branch_mut(&mut r.tree, branch_name))
        else {
            bail!("Branch {branch_name} not found in the git-stack tree.");
        };
        parent.branches.retain(|branch| branch.name != branch_name);
        println!(
            "Branch {branch_name} removed from git-stack tree.",
            branch_name = branch_name.yellow()
        );

        self.save_state()?;

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn cleanup_missing_branches(
        &mut self,
        git_repo: &GitRepo,
        repo: &str,
        dry_run: bool,
        all: bool,
        current_branch: &str,
        authors_filter: &[String],
        pr_authors: &HashMap<String, String>,
    ) -> Result<()> {
        if all {
            // The `--all` sweep has no per-repo current-branch/author context, so it does
            // missing-branch removal only (ignoring the author-prune args).
            self.cleanup_all_trees(dry_run)
        } else {
            self.cleanup_single_tree(
                git_repo,
                repo,
                dry_run,
                current_branch,
                authors_filter,
                pr_authors,
            )
        }
    }

    /// Auto-cleanup missing branches silently during status display.
    /// Returns true if any branches were cleaned up.
    pub(crate) fn auto_cleanup_missing_branches(
        &mut self,
        git_repo: &GitRepo,
        repo: &str,
    ) -> Result<bool> {
        let Some(repo_state) = self.repos.get_mut(repo) else {
            return Ok(false);
        };

        let mut removed_branches = Vec::new();
        let mut remounted_branches = Vec::new();

        cleanup_tree_recursive(
            git_repo,
            &mut repo_state.tree,
            &mut removed_branches,
            &mut remounted_branches,
        );

        if removed_branches.is_empty() {
            return Ok(false);
        }

        // Print brief summary of auto-cleanup
        for branch_name in &removed_branches {
            println!(
                "{} {} (branch no longer exists)",
                "Auto-removed:".truecolor(90, 90, 90),
                branch_name.red()
            );
        }
        for (branch_name, new_parent) in &remounted_branches {
            println!(
                "{} {} {} {}",
                "Auto-remounted:".truecolor(90, 90, 90),
                branch_name.yellow(),
                "→".truecolor(90, 90, 90),
                new_parent.green()
            );
        }
        println!();

        Ok(true)
    }

    fn cleanup_single_tree(
        &mut self,
        git_repo: &GitRepo,
        repo: &str,
        dry_run: bool,
        current_branch: &str,
        authors_filter: &[String],
        pr_authors: &HashMap<String, String>,
    ) -> Result<()> {
        let Some(repo_state) = self.repos.get_mut(repo) else {
            println!("No stack tree found for repo {}", repo.yellow());
            return Ok(());
        };

        // Phase 1: remove branches that no longer exist locally or on the remote.
        let mut removed_branches = Vec::new();
        let mut remounted_branches = Vec::new();
        cleanup_tree_recursive(
            git_repo,
            &mut repo_state.tree,
            &mut removed_branches,
            &mut remounted_branches,
        );

        // Phase 2: prune branches confidently attributed to an author outside `authors_filter` —
        // exactly the set the render hides via `compute_hidden_branches`, so cleanup and render
        // stay consistent. Protected branches (current branch, its ancestors, trunk) and branches
        // with no author data are never selected. Empty when `authors_filter` is unset, so this
        // is a no-op for users who don't filter by author.
        let to_prune = crate::render::tree_data::compute_hidden_branches(
            &repo_state.tree,
            current_branch,
            authors_filter,
            pr_authors,
            false,
        );
        let (pruned_branches, prune_remounts) = apply_prune(&mut repo_state.tree, &to_prune);
        remounted_branches.extend(prune_remounts);

        if removed_branches.is_empty() && pruned_branches.is_empty() {
            println!("No missing or out-of-scope branches found. Tree is clean.");
            return Ok(());
        }

        // Print combined preview.
        println!("Cleanup summary for {}:", repo.yellow());
        if !removed_branches.is_empty() {
            println!();
            println!("Removed (no longer exist):");
            for branch_name in &removed_branches {
                println!("  - {}", branch_name.red());
            }
        }
        if !pruned_branches.is_empty() {
            println!();
            println!("Pruned (out of scope — author not in authors_filter):");
            for branch_name in &pruned_branches {
                println!("  - {}", branch_name.red());
            }
        }
        if !remounted_branches.is_empty() {
            println!();
            println!("Re-mounted branches (moved to grandparent):");
            for (branch_name, new_parent) in &remounted_branches {
                println!(
                    "  - {} {} {}",
                    branch_name.yellow(),
                    "→".truecolor(90, 90, 90),
                    new_parent.green()
                );
            }
        }

        if dry_run {
            println!();
            println!("{}", "Dry run mode: no changes were saved.".bright_blue());
            return Ok(());
        }

        // The prune phase is destructive in a way the missing-branch removal isn't: those pruned
        // branches still exist in git, we're only dropping them from git-stack's tree. Confirm
        // before persisting whenever anything was pruned. Missing-only cleanup keeps today's
        // no-confirm save.
        if !pruned_branches.is_empty() {
            if !std::io::stdin().is_terminal() {
                bail!(
                    "Pruning out-of-scope branches requires confirmation. Use --dry-run to \
                     preview or run interactively to confirm."
                );
            }
            if !confirm_prune() {
                println!("\n{}", "Aborted.".yellow());
                return Ok(());
            }
        }

        self.save_state()?;
        println!();
        println!("{}", "Changes saved.".green());
        Ok(())
    }

    fn cleanup_all_trees(&mut self, dry_run: bool) -> Result<()> {
        let mut repos_to_remove = Vec::new();
        let mut total_removed_branches = 0;
        let mut total_remounted_branches = 0;

        // Collect all repo paths first to avoid borrow checker issues
        let repo_paths: Vec<String> = self.repos.keys().cloned().collect();

        println!("Scanning {} repositories...", repo_paths.len());
        println!();

        for repo_path in &repo_paths {
            // Check if the directory exists
            if !std::path::Path::new(repo_path).exists() {
                println!(
                    "{}: {}",
                    repo_path.yellow(),
                    "directory does not exist".red()
                );
                repos_to_remove.push(repo_path.clone());
                continue;
            }

            // Check if git works in this directory
            let original_dir = std::env::current_dir()?;
            let git_works = std::env::set_current_dir(repo_path).is_ok()
                && run_git(&["rev-parse", "--git-dir"]).is_ok();

            // Always restore the original directory
            std::env::set_current_dir(original_dir)?;

            if !git_works {
                println!("{}: {}", repo_path.yellow(), "git is not working".red());
                repos_to_remove.push(repo_path.clone());
                continue;
            }

            // Change to the repo directory and clean up branches
            let original_dir = std::env::current_dir()?;
            std::env::set_current_dir(repo_path)?;

            // Open a GitRepo for this specific repository
            let Ok(repo_git) = GitRepo::open(repo_path) else {
                println!("{}: {}", repo_path.yellow(), "failed to open repo".red());
                std::env::set_current_dir(original_dir)?;
                repos_to_remove.push(repo_path.clone());
                continue;
            };

            let repo_state = self.repos.get_mut(repo_path).unwrap();
            let mut removed_branches = Vec::new();
            let mut remounted_branches = Vec::new();

            cleanup_tree_recursive(
                &repo_git,
                &mut repo_state.tree,
                &mut removed_branches,
                &mut remounted_branches,
            );

            std::env::set_current_dir(original_dir)?;

            if !removed_branches.is_empty() || !remounted_branches.is_empty() {
                println!(
                    "{}: cleaned up {} branches, re-mounted {}",
                    repo_path.yellow(),
                    removed_branches.len().to_string().red(),
                    remounted_branches.len().to_string().green()
                );
                total_removed_branches += removed_branches.len();
                total_remounted_branches += remounted_branches.len();
            } else {
                println!("{}: {}", repo_path.yellow(), "clean".green());
            }
        }

        // Remove invalid repos
        if !repos_to_remove.is_empty() {
            println!();
            println!("Removing {} invalid repositories:", repos_to_remove.len());
            for repo_path in &repos_to_remove {
                println!("  - {}", repo_path.red());
                self.repos.remove(repo_path);
            }
        }

        println!();
        println!("Summary:");
        println!("  Repositories scanned: {}", repo_paths.len());
        println!("  Invalid repositories removed: {}", repos_to_remove.len());
        println!("  Branches removed: {}", total_removed_branches);
        println!("  Branches re-mounted: {}", total_remounted_branches);

        if dry_run {
            println!();
            println!("{}", "Dry run mode: no changes were saved.".bright_blue());
        } else {
            self.save_state()?;
            println!();
            println!("{}", "Changes saved.".green());
        }

        Ok(())
    }

    pub(crate) fn ensure_trunk(&mut self, git_repo: &GitRepo, repo: &str) -> Option<GitTrunk> {
        let trunk = git_trunk(git_repo)?;
        // The branch might not exist in git, let's create it, and add it to the tree.
        // Ensure the main branch is in the git-stack tree for this repo if we haven't
        // added it yet.
        self.repos
            .entry(repo.to_string())
            .or_insert_with(|| RepoState::new(Branch::new(trunk.main_branch.clone(), None)));
        Some(trunk)
    }

    pub(crate) fn mount(
        &mut self,
        git_repo: &GitRepo,
        repo: &str,
        branch_name: &str,
        parent_branch: Option<String>,
    ) -> Result<()> {
        let trunk = self.ensure_trunk(git_repo, repo);

        if let Some(ref trunk) = trunk
            && trunk.main_branch == branch_name
        {
            bail!(
                "Branch {branch_name} cannot be stacked on anything else.",
                branch_name = branch_name.red()
            );
        }

        let parent_branch = parent_branch
            .or_else(|| trunk.map(|t| t.main_branch))
            .ok_or_else(|| anyhow!("No parent branch specified and no remote configured"))?;

        if branch_name == parent_branch {
            bail!(
                "Branch {branch_name} cannot be mounted on itself.",
                branch_name = branch_name.red()
            );
        }

        tracing::debug!("Mounting {branch_name} on {parent_branch:?}");

        // Make sure the parent branch is actually changing.
        if let (Some(Branch { name: name_a, .. }), Some(Branch { name: name_b, .. })) = (
            self.get_parent_branch_of(repo, branch_name),
            self.get_tree_branch(repo, &parent_branch),
        ) && name_a == name_b
        {
            tracing::warn!("Branch {branch_name} is already mounted on {name_a}");
            return Ok(());
        }

        // First, extract the existing branch from its current parent (preserving metadata)
        let mut existing_branch: Option<Branch> = None;
        if let Some(current_parent_branch) = self.get_parent_branch_of_mut(repo, branch_name) {
            // Find and remove the branch, preserving it
            if let Some(pos) = current_parent_branch
                .branches
                .iter()
                .position(|b| b.name == branch_name)
            {
                existing_branch = Some(current_parent_branch.branches.remove(pos));
            }
        }

        // Get the branch to add (either preserved or new)
        let mut branch_to_add = existing_branch.unwrap_or_else(|| {
            Branch::new(branch_name.to_string(), git_repo.sha(&parent_branch).ok())
        });

        // Update the lkg_parent to the new parent
        branch_to_add.lkg_parent = git_repo.sha(&parent_branch).ok();

        // Add the branch to the new parent
        let new_parent_branch = self.get_tree_branch_mut(repo, &parent_branch);
        if let Some(new_parent_branch) = new_parent_branch {
            new_parent_branch.branches.push(branch_to_add);
        } else {
            bail!("Parent branch {parent_branch} not found in the git-stack tree.");
        }
        println!(
            "Branch {branch_name} stacked on {parent_branch}.",
            branch_name = branch_name.yellow(),
            parent_branch = parent_branch.yellow()
        );

        self.save_state()?;
        Ok(())
    }
    pub fn get_parent_branch_of(&self, repo: &str, branch_name: &str) -> Option<&Branch> {
        self.repos
            .get(repo)
            .and_then(|r| find_parent_of_branch(&r.tree, branch_name))
    }
    pub fn get_parent_branch_of_mut(
        &mut self,
        repo: &str,
        branch_name: &str,
    ) -> Option<&mut Branch> {
        self.repos
            .get_mut(repo)
            .and_then(|r| find_parent_of_branch_mut(&mut r.tree, branch_name))
    }

    /// Compute the lkg_parent updates for every branch in the tree, without applying or
    /// persisting them. Split out from `refresh_lkgs` so tests can exercise the BFS logic
    /// without triggering a `save_state()` write to the real XDG state file.
    fn compute_lkg_updates(
        &self,
        git_repo: &GitRepo,
        repo: &str,
        scope: Option<&HashSet<String>>,
    ) -> Result<HashMap<String, Option<String>>> {
        let Some(trunk) = git_trunk(git_repo) else {
            return Ok(HashMap::default());
        };

        let mut parent_lkgs: HashMap<String, Option<String>> = HashMap::default();

        // BFS Traverse the tree from the root to the leaves, and update the lkgs as we go.
        let mut queue: VecDeque<(Option<String>, String)> = VecDeque::new();
        queue.push_back((None, trunk.main_branch.clone()));
        while let Some((parent, branch)) = queue.pop_front() {
            // Only recompute the per-branch LKG for in-scope branches, but always enqueue
            // children below so in-scope descendants of an out-of-scope branch are still reached.
            if scope.is_none_or(|s| s.contains(&branch)) {
                // Resolve branch to local or remote ref
                let Some(branch_ref) = git_repo.resolve_branch_ref(&branch) else {
                    tracing::debug!("Skipping non-existent branch {} in refresh_lkgs", branch);
                    // Preserve today's behavior: a nonexistent in-scope branch enqueues no
                    // children (matching the full-refresh path's `continue`).
                    continue;
                };

                if let Some(parent) = parent {
                    // Resolve parent to local or remote ref
                    let parent_ref = git_repo
                        .resolve_branch_ref(&parent)
                        .unwrap_or_else(|| parent.clone());

                    let tree_branch = self.get_tree_branch(repo, &branch).unwrap();
                    if let Some(lkg_parent) = tree_branch.lkg_parent.as_deref() {
                        if git_repo.is_ancestor(lkg_parent, &branch_ref)? {
                            parent_lkgs
                                .insert(tree_branch.name.clone(), Some(lkg_parent.to_string()));
                        } else {
                            parent_lkgs.insert(tree_branch.name.clone(), None);
                        }
                    }
                    if git_repo
                        .is_ancestor(&parent_ref, &branch_ref)
                        .unwrap_or(false)
                        && let Ok(new_lkg_parent) = git_repo.sha(&parent_ref)
                    {
                        tracing::debug!(
                            lkg_parent = ?new_lkg_parent,
                            "Branch {} is a descendent of {}",
                            branch.yellow(),
                            parent.yellow(),
                        );
                        // Save the LKG parent for the branch.
                        parent_lkgs.insert(branch.clone(), Some(new_lkg_parent));
                    } else if !matches!(parent_lkgs.get(&branch), Some(Some(_)))
                        && let Ok(merge_base) = git_repo.merge_base(&parent_ref, &branch_ref)
                    {
                        tracing::debug!(
                            lkg_parent = ?merge_base,
                            "Branch {} has no fast-forward lkg parent; backfilling from merge-base \
                            with {}",
                            branch.yellow(),
                            parent.yellow(),
                        );
                        parent_lkgs.insert(branch.clone(), Some(merge_base));
                    }
                }
            }
            if let Some(branch_node) = self.get_tree_branch(repo, &branch) {
                for child_branch in &branch_node.branches {
                    queue.push_back((Some(branch_node.name.clone()), child_branch.name.clone()));
                }
            }
        }
        Ok(parent_lkgs)
    }

    /// Apply computed `lkg_parent` updates to the tree. Returns whether any branch's value
    /// actually changed, so callers can skip a `save_state()` write when the BFS just recomputed
    /// the same values the tree already had.
    fn apply_lkg_updates(
        &mut self,
        repo: &str,
        parent_lkgs: HashMap<String, Option<String>>,
    ) -> Result<bool> {
        let mut changed = false;
        for (branch, lkg_parent) in parent_lkgs {
            let branch = self
                .get_tree_branch_mut(repo, &branch)
                .ok_or_else(|| anyhow!("Branch {branch} not found in the git-stack tree."))?;
            if branch.lkg_parent != lkg_parent {
                branch.lkg_parent = lkg_parent;
                changed = true;
            }
        }
        Ok(changed)
    }

    pub(crate) fn refresh_lkgs(&mut self, git_repo: &GitRepo, repo: &str) -> Result<()> {
        let _bench = crate::stats::GitBenchmark::start("state:refresh-lkgs");
        tracing::debug!("Refreshing lkgs for all branches...");
        let parent_lkgs = self.compute_lkg_updates(git_repo, repo, None)?;
        if self.apply_lkg_updates(repo, parent_lkgs)? {
            self.save_state()?;
        }
        Ok(())
    }

    /// Refresh `lkg_parent` for only the branches in `scope`. Out-of-scope branches keep their
    /// existing (possibly stale/missing) values; the consumers all tolerate that via merge-base
    /// fallbacks, and `refresh_lkg_for_branch` refreshes on demand right before consumption.
    pub(crate) fn refresh_lkgs_scoped(
        &mut self,
        git_repo: &GitRepo,
        repo: &str,
        scope: &HashSet<String>,
    ) -> Result<()> {
        let _bench = crate::stats::GitBenchmark::start("state:refresh-lkgs");
        let parent_lkgs = self.compute_lkg_updates(git_repo, repo, Some(scope))?;
        if self.apply_lkg_updates(repo, parent_lkgs)? {
            self.save_state()?;
        }
        Ok(())
    }

    /// Branches to eagerly LKG-refresh: the current branch's ancestor chain to trunk (always),
    /// plus every branch NOT confidently attributable (via the closed-PR cache) to an author
    /// outside `authors_filter`. Branches with no closed-PR entry stay in the set.
    ///
    /// Pure (no git/network) so it can be unit-tested: it walks the in-memory tree and consults
    /// only the caller-supplied `closed_pr_authors` map.
    pub(crate) fn eager_lkg_scope(
        &self,
        repo: &str,
        current_branch: &str,
        authors_filter: &[String],
        closed_pr_authors: &HashMap<String, String>,
    ) -> HashSet<String> {
        let mut scope: HashSet<String> = HashSet::default();
        let Some(tree) = self.get_tree(repo) else {
            return scope;
        };

        let mut all_branches = Vec::new();
        collect_all_branches(tree, &mut all_branches);
        for branch in all_branches {
            // Only exclude a branch when the closed-PR cache confidently attributes it to an
            // author outside `authors_filter`. Missing data ⇒ keep (never guess "not mine").
            let foreign = closed_pr_authors
                .get(&branch)
                .is_some_and(|login| !crate::github::author_in_filter(authors_filter, login));
            if !foreign {
                scope.insert(branch);
            }
        }

        // The current branch's ancestor chain must always be refreshed, even if a foreign
        // closed-PR author would otherwise exclude one of the ancestors.
        let mut path: Vec<&Branch> = Vec::new();
        if get_path(tree, current_branch, &mut path) {
            for branch in path {
                scope.insert(branch.name.clone());
            }
        }

        scope
    }

    /// Lazily refresh `lkg_parent` for `branch` and its ancestor chain, right before a subcommand
    /// consumes it. Cheap (a handful of branches) and covers both `diff` (needs the target) and
    /// `restack` (needs target + ancestors). Falls back to `{branch}` when the branch isn't in the
    /// tree yet.
    pub(crate) fn refresh_lkg_for_branch(
        &mut self,
        git_repo: &GitRepo,
        repo: &str,
        branch: &str,
    ) -> Result<()> {
        let mut scope: HashSet<String> = HashSet::default();
        if let Some(tree) = self.get_tree(repo) {
            let mut path: Vec<&Branch> = Vec::new();
            if get_path(tree, branch, &mut path) {
                for b in path {
                    scope.insert(b.name.clone());
                }
            }
        }
        if scope.is_empty() {
            scope.insert(branch.to_string());
        }
        self.refresh_lkgs_scoped(git_repo, repo, &scope)
    }

    pub(crate) fn edit_note(&mut self, repo: &str, branch: &str) -> Result<()> {
        let Some(branch) = self.get_tree_branch_mut(repo, branch) else {
            bail!("Branch {branch} not found in the git-stack tree.");
        };
        let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
        // Create a temporary file.
        let temp_file = tempfile::NamedTempFile::new()?;

        fs::write(temp_file.path(), branch.note.as_deref().unwrap_or(""))?;

        // Invoke the user's editor.
        if !Command::new(editor)
            .arg(temp_file.path().to_str().unwrap())
            .status()?
            .success()
        {
            eprintln!("Changes discarded.");
        }
        let text = fs::read(temp_file.path())?;
        let buf = std::str::from_utf8(&text)?.trim().to_string();
        branch.note = Some(buf);
        self.save_state()?;
        Ok(())
    }

    pub(crate) fn show_note(&self, repo: &str, branch: &str) -> Result<()> {
        let Some(branch) = self.get_tree_branch(repo, branch) else {
            bail!("Branch {branch} not found in the git-stack tree.");
        };

        let note = branch
            .note
            .as_deref()
            .unwrap_or(&format!(
                "No note set for branch '{branch}'.",
                branch = branch.name.as_str().yellow()
            ))
            .to_string();
        print!("{}{}", note, if !note.ends_with("\n") { "\n" } else { "" });
        Ok(())
    }

    pub(crate) fn edit_config(&self) -> Result<()> {
        let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());

        // TODO: edit only the config for the current repo.
        // Invoke the user's editor.
        let _ = Command::new(editor)
            .arg(get_xdg_path()?.to_str().unwrap())
            .status()?;
        Ok(())
    }

    /// Try to auto-mount the current branch if it's not in the tree.
    /// Returns Ok(true) if the branch was auto-mounted, Ok(false) if it was already in the tree,
    /// or Err if auto-mount failed.
    pub(crate) fn try_auto_mount(
        &mut self,
        git_repo: &GitRepo,
        repo: &str,
        branch_name: &str,
    ) -> Result<bool> {
        // Ensure the tree exists for this repo (no-op if no remote)
        self.ensure_trunk(git_repo, repo);

        // Check if the branch is already in the tree
        if self.branch_exists_in_tree(repo, branch_name) {
            return Ok(false);
        }

        // Check if this branch exists in git
        if !git_branch_exists(git_repo, branch_name) {
            bail!("Branch {branch_name} does not exist in git");
        }

        // Get the tree for this repo (may not exist if no remote configured)
        let Some(tree) = self.get_tree(repo) else {
            return Ok(false);
        };

        // Collect all mounted branches
        let mut all_branches = Vec::new();
        collect_all_branches(tree, &mut all_branches);

        // Find all mounted branches that are ancestors of the current branch
        let mut ancestor_branches = Vec::new();
        for mounted_branch in &all_branches {
            if let Ok(true) = git_repo.is_ancestor(mounted_branch, branch_name) {
                ancestor_branches.push(mounted_branch.clone());
            }
        }

        // Determine the parent branch
        let parent_branch = if ancestor_branches.is_empty() {
            // No ancestors found, default to the trunk/main branch
            let Some(trunk) = git_trunk(git_repo) else {
                // No remote configured and no ancestor branches - can't auto-mount
                return Ok(false);
            };
            tracing::info!(
                "No mounted ancestor branches found for {}. Defaulting to trunk branch {}.",
                branch_name,
                trunk.main_branch
            );
            trunk.main_branch
        } else {
            // Find the deepest ancestor branch (highest depth in the tree)
            let mut deepest_branch = None;
            let mut max_depth = 0;

            for ancestor in &ancestor_branches {
                if let Some(depth) = get_branch_depth(tree, ancestor, 0)
                    && depth >= max_depth
                {
                    max_depth = depth;
                    deepest_branch = Some(ancestor.clone());
                }
            }

            deepest_branch
                .ok_or_else(|| anyhow!("Failed to determine parent branch for auto-mount"))?
        };

        tracing::info!("Auto-mounting branch {} on {}", branch_name, parent_branch);
        println!(
            "Auto-mounting branch {} on {}...",
            branch_name.yellow(),
            parent_branch.yellow()
        );

        // Mount the branch
        self.mount(git_repo, repo, branch_name, Some(parent_branch))?;

        Ok(true)
    }
}

fn get_path<'a>(branch: &'a Branch, target_branch: &str, path: &mut Vec<&'a Branch>) -> bool {
    if branch.name == target_branch {
        path.insert(0, branch);
        return true;
    }
    for child_branch in &branch.branches {
        if get_path(child_branch, target_branch, path) {
            path.insert(0, branch);
            return true;
        }
    }
    false
}

#[derive(Debug)]
pub(crate) struct RestackStep<'a> {
    pub(crate) parent: String,
    pub(crate) branch: &'a Branch,
}

fn find_branch_by_name<'a>(tree: &'a Branch, name: &str) -> Option<&'a Branch> {
    find_branch(tree, &|branch| branch.name == name)
}

fn find_branch_by_name_mut<'a>(tree: &'a mut Branch, name: &str) -> Option<&'a mut Branch> {
    find_branch_mut(tree, &|branch| branch.name == name)
}

fn find_parent_of_branch_mut<'a>(tree: &'a mut Branch, name: &str) -> Option<&'a mut Branch> {
    find_branch_mut(tree, &|branch| {
        branch.branches.iter().any(|branch| branch.name == name)
    })
}

fn find_parent_of_branch<'a>(tree: &'a Branch, name: &str) -> Option<&'a Branch> {
    find_branch(tree, &|branch| {
        branch.branches.iter().any(|branch| branch.name == name)
    })
}

fn find_branch<'a, F>(tree: &'a Branch, pred: &F) -> Option<&'a Branch>
where
    F: Fn(&Branch) -> bool,
{
    if pred(tree) {
        Some(tree)
    } else {
        for child_branch in tree.branches.iter() {
            let result = find_branch(child_branch, pred);
            if result.is_some() {
                return result;
            }
        }
        None
    }
}

fn find_branch_mut<'a, F>(tree: &'a mut Branch, pred: &F) -> Option<&'a mut Branch>
where
    F: Fn(&Branch) -> bool,
{
    if pred(tree) {
        Some(tree)
    } else {
        for child_branch in tree.branches.iter_mut() {
            let result = find_branch_mut(child_branch, pred);
            if result.is_some() {
                return result;
            }
        }
        None
    }
}

// Linear walk through the tree to find the branch.
fn is_branch_mentioned_in_tree(branch_name: &str, branch: &Branch) -> bool {
    if branch.name == branch_name {
        return true;
    }

    for child_branch in &branch.branches {
        if is_branch_mentioned_in_tree(branch_name, child_branch) {
            return true;
        }
    }
    false
}

/// Recursively cleans up missing branches from the tree.
/// Returns the number of branches cleaned up at this level.
fn cleanup_tree_recursive(
    git_repo: &GitRepo,
    branch: &mut Branch,
    removed_branches: &mut Vec<String>,
    remounted_branches: &mut Vec<(String, String)>,
) {
    // First, recursively process all children
    for child in &mut branch.branches {
        cleanup_tree_recursive(git_repo, child, removed_branches, remounted_branches);
    }

    // Collect branches to remove and their children to adopt
    let mut branches_to_adopt: Vec<Branch> = Vec::new();
    let mut indices_to_remove = Vec::new();

    for (index, child) in branch.branches.iter().enumerate() {
        let remote_ref = format!("origin/{}", child.name);
        if !git_branch_exists(git_repo, &child.name) && !git_repo.ref_exists(&remote_ref) {
            // This branch doesn't exist locally or on remote, mark it for removal
            removed_branches.push(child.name.clone());

            // Collect its children to be adopted by the current branch
            for grandchild in &child.branches {
                branches_to_adopt.push(grandchild.clone());
                remounted_branches.push((grandchild.name.clone(), branch.name.clone()));
            }

            indices_to_remove.push(index);
        }
    }

    // Remove missing branches (in reverse order to maintain indices)
    for &index in indices_to_remove.iter().rev() {
        branch.branches.remove(index);
    }

    // Add adopted branches
    branch.branches.extend(branches_to_adopt);
}

/// Splice every branch named in `to_remove` out of the tree, adopting each removed node's kept
/// children into its parent (mirroring `cleanup_tree_recursive`'s remount behavior, but keyed on
/// an explicit name set rather than git existence). Bottom-up so a removed branch whose parent is
/// also removed has its children re-parented onto the surviving grandparent. The root is never
/// removed even if named. Returns `(removed, remounted)` where `remounted` pairs each adopted
/// child with its new parent.
fn apply_prune(
    tree: &mut Branch,
    to_remove: &HashSet<String>,
) -> (Vec<String>, Vec<(String, String)>) {
    let mut removed = Vec::new();
    let mut remounted = Vec::new();
    prune_recursive(tree, to_remove, &mut removed, &mut remounted);
    (removed, remounted)
}

fn prune_recursive(
    branch: &mut Branch,
    to_remove: &HashSet<String>,
    removed: &mut Vec<String>,
    remounted: &mut Vec<(String, String)>,
) {
    // First, recursively process all children (bottom-up).
    for child in &mut branch.branches {
        prune_recursive(child, to_remove, removed, remounted);
    }

    let mut branches_to_adopt: Vec<Branch> = Vec::new();
    let mut indices_to_remove = Vec::new();

    for (index, child) in branch.branches.iter().enumerate() {
        if to_remove.contains(&child.name) {
            removed.push(child.name.clone());
            for grandchild in &child.branches {
                branches_to_adopt.push(grandchild.clone());
                remounted.push((grandchild.name.clone(), branch.name.clone()));
            }
            indices_to_remove.push(index);
        }
    }

    for &index in indices_to_remove.iter().rev() {
        branch.branches.remove(index);
    }
    branch.branches.extend(branches_to_adopt);
}

/// Prompt the user to confirm the destructive prune of out-of-scope branches. Modeled on
/// `sync::confirm_remote_changes`.
fn confirm_prune() -> bool {
    use std::io::{self, Write};

    print!("Prune out-of-scope branches from the git-stack tree? [y/N] ");
    io::stdout().flush().unwrap();

    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_err() {
        return false;
    }

    matches!(input.trim().to_lowercase().as_str(), "y" | "yes")
}

fn find_stack_with_branch<'a>(
    stacks: &'a mut [Vec<String>],
    current_branch: &str,
) -> Result<&'a mut Vec<String>> {
    for stack in stacks.iter_mut() {
        if stack.contains(&current_branch.to_string()) {
            return Ok(stack);
        }
    }
    Err(anyhow::anyhow!(
        "No stack found for branch {}",
        current_branch
    ))
}

/// Collect all branch names from the tree recursively.
fn collect_all_branches(branch: &Branch, branches: &mut Vec<String>) {
    branches.push(branch.name.clone());
    for child in &branch.branches {
        collect_all_branches(child, branches);
    }
}

/// Calculate the depth of a branch in the tree. Returns None if the branch is not found.
fn get_branch_depth(tree: &Branch, target: &str, current_depth: usize) -> Option<usize> {
    if tree.name == target {
        return Some(current_depth);
    }
    for child in &tree.branches {
        if let Some(depth) = get_branch_depth(child, target, current_depth + 1) {
            return Some(depth);
        }
    }
    None
}

fn get_xdg_path() -> anyhow::Result<PathBuf> {
    let base_dirs = xdg::BaseDirectories::with_prefix(env!("CARGO_PKG_NAME"));
    base_dirs
        .get_state_file("state.yaml")
        .ok_or_else(|| anyhow::anyhow!("Failed to find state file"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_state_write() {
        let state = State {
            repos: vec![(
                "/tmp/foo".to_string(),
                RepoState::new(Branch {
                    name: "main".to_string(),
                    stack_method: StackMethod::ApplyMerge,
                    note: None,
                    lkg_parent: None,
                    pr_number: None,
                    branches: vec![],
                }),
            )]
            .into_iter()
            .collect(),
        };
        let serialized = serde_yaml::to_string(&state).unwrap();
        assert_eq!(
            serialized,
            "/tmp/foo:\n  name: main\n  stack_method: apply_merge\n  lkg_parent: null\n  branches: []\n",
        );
    }
    #[test]
    fn test_state_read() {
        let state = "/tmp/foo:\n  name: main\n  stack_method: apply_merge\n  lkg_parent: null\n  branches: []\n";
        let state: State = serde_yaml::from_str(state).unwrap();
        assert_eq!(state.repos.len(), 1);
        assert!(state.repos.contains_key("/tmp/foo"));
        let repo_state = state.repos.get("/tmp/foo").unwrap();
        assert_eq!(repo_state.tree.name, "main");
        assert_eq!(repo_state.tree.stack_method, StackMethod::ApplyMerge);
        assert_eq!(repo_state.tree.pr_number, None);
    }

    fn sample_resume() -> RestackResume {
        RestackResume {
            restack_branch: "feature-b".to_string(),
            orig_branch: "feature-b".to_string(),
            ancestors: true,
            push: false,
            squash: false,
        }
    }

    /// Round-trip a non-squash (`Am`/`Rebase`/`Merge`) pending record: no squash-only fields.
    fn assert_non_squash_round_trip(method: RestackMethod) {
        let pending = PendingRestackOperation {
            method,
            branch_name: "feature-b".to_string(),
            parent: "feature-a".to_string(),
            original_sha: "6815deadbeef".to_string(),
            tmp_branch_name: None,
            squash_message: None,
            resume: sample_resume(),
        };
        let yaml = serde_yaml::to_string(&pending).unwrap();
        let back: PendingRestackOperation = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(pending, back);
        assert_eq!(back.method, method);
        assert_eq!(back.tmp_branch_name, None);
        assert_eq!(back.squash_message, None);
    }

    #[test]
    fn pending_restack_round_trips_am() {
        assert_non_squash_round_trip(RestackMethod::Am);
    }

    #[test]
    fn pending_restack_round_trips_rebase() {
        assert_non_squash_round_trip(RestackMethod::Rebase);
    }

    #[test]
    fn pending_restack_round_trips_merge() {
        assert_non_squash_round_trip(RestackMethod::Merge);
    }

    #[test]
    fn pending_restack_round_trips_squash() {
        let pending = PendingRestackOperation {
            method: RestackMethod::Squash,
            branch_name: "feature-b".to_string(),
            parent: "feature-a".to_string(),
            original_sha: "6815deadbeef".to_string(),
            tmp_branch_name: Some("tmp-feature-b".to_string()),
            squash_message: Some("squashed commit message".to_string()),
            resume: RestackResume {
                squash: true,
                ..sample_resume()
            },
        };
        let yaml = serde_yaml::to_string(&pending).unwrap();
        let back: PendingRestackOperation = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(pending, back);
        assert_eq!(back.method, RestackMethod::Squash);
        assert_eq!(back.tmp_branch_name.as_deref(), Some("tmp-feature-b"));
        assert_eq!(
            back.squash_message.as_deref(),
            Some("squashed commit message")
        );
    }

    #[test]
    fn repo_state_without_pending_restack_still_loads() {
        // An old state file predating the pending_restack field must still parse, with the
        // field defaulting to None.
        let yaml = "/tmp/foo:\n  name: main\n  stack_method: apply_merge\n  lkg_parent: null\n  branches: []\n";
        let state: State = serde_yaml::from_str(yaml).unwrap();
        let repo_state = state.repos.get("/tmp/foo").unwrap();
        assert!(repo_state.pending_restack.is_none());
    }

    /// Initialize a temp repo with a root commit on `main`, plus a fake `origin` remote-tracking
    /// ref so `git_trunk` resolves without a real network remote.
    fn init_test_repo(dir: &Path) {
        let git = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(dir)
                    .status()
                    .unwrap()
                    .success()
            );
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "test@example.com"]);
        git(&["config", "user.name", "Test"]);
        // Disable background auto-maintenance/gc: `git commit` otherwise spawns a detached
        // `git maintenance run --auto --detach` that inherits the test's stdout/stderr pipe and
        // outlives the test, which nextest flags as a leaked handle.
        git(&["config", "maintenance.auto", "false"]);
        git(&["config", "gc.auto", "0"]);
        git(&["commit", "--allow-empty", "-q", "-m", "root"]);
        git(&["update-ref", "refs/remotes/origin/main", "main"]);
        git(&[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ]);
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

    fn git_run(dir: &Path, args: &[&str]) {
        assert!(
            Command::new("git")
                .args(args)
                .current_dir(dir)
                .status()
                .unwrap()
                .success()
        );
    }

    fn repo_key(dir: &Path) -> String {
        dir.canonicalize().unwrap().to_string_lossy().to_string()
    }

    #[test]
    fn compute_lkg_updates_backfills_via_merge_base() {
        let dir = tempfile::tempdir().unwrap();
        init_test_repo(dir.path());
        let sha_a = git_rev_parse(dir.path(), "main");

        // Branch `feature` off of `main` at commit A, then add a commit to `feature` (B) and a
        // divergent commit to `main` (C), so `feature` is no longer a fast-forward descendant of
        // `main`'s current tip.
        git_run(dir.path(), &["checkout", "-b", "feature"]);
        git_run(
            dir.path(),
            &["commit", "--allow-empty", "-q", "-m", "feature commit"],
        );
        git_run(dir.path(), &["checkout", "main"]);
        git_run(
            dir.path(),
            &["commit", "--allow-empty", "-q", "-m", "main commit"],
        );

        let git_repo =
            GitRepo::open_with_cache_at(dir.path(), &dir.path().join("mb_cache.redb")).unwrap();
        let repo = repo_key(dir.path());

        let mut main_branch = Branch::new("main".to_string(), None);
        main_branch
            .branches
            .push(Branch::new("feature".to_string(), None));
        let state = State {
            repos: [(repo.clone(), RepoState::new(main_branch))]
                .into_iter()
                .collect(),
        };

        let updates = state.compute_lkg_updates(&git_repo, &repo, None).unwrap();
        assert_eq!(updates.get("feature"), Some(&Some(sha_a)));
    }

    #[test]
    fn compute_lkg_updates_does_not_overwrite_valid_lkg_parent() {
        let dir = tempfile::tempdir().unwrap();
        init_test_repo(dir.path());
        let sha_a = git_rev_parse(dir.path(), "main");

        git_run(dir.path(), &["checkout", "-b", "feature"]);
        git_run(
            dir.path(),
            &["commit", "--allow-empty", "-q", "-m", "feature commit"],
        );
        git_run(dir.path(), &["checkout", "main"]);
        git_run(
            dir.path(),
            &["commit", "--allow-empty", "-q", "-m", "main commit"],
        );

        let git_repo =
            GitRepo::open_with_cache_at(dir.path(), &dir.path().join("mb_cache.redb")).unwrap();
        let repo = repo_key(dir.path());

        let mut main_branch = Branch::new("main".to_string(), None);
        main_branch
            .branches
            .push(Branch::new("feature".to_string(), Some(sha_a.clone())));
        let state = State {
            repos: [(repo.clone(), RepoState::new(main_branch))]
                .into_iter()
                .collect(),
        };

        let updates = state.compute_lkg_updates(&git_repo, &repo, None).unwrap();
        assert_eq!(updates.get("feature"), Some(&Some(sha_a)));
    }

    #[test]
    fn compute_lkg_updates_leaves_no_entry_when_no_common_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        init_test_repo(dir.path());

        // Create `feature` as an orphan branch with unrelated history, so `main` and `feature`
        // share no common ancestor.
        git_run(dir.path(), &["checkout", "--orphan", "feature"]);
        git_run(
            dir.path(),
            &["commit", "--allow-empty", "-q", "-m", "orphan commit"],
        );
        git_run(dir.path(), &["checkout", "main"]);

        let git_repo =
            GitRepo::open_with_cache_at(dir.path(), &dir.path().join("mb_cache.redb")).unwrap();
        let repo = repo_key(dir.path());

        let mut main_branch = Branch::new("main".to_string(), None);
        main_branch
            .branches
            .push(Branch::new("feature".to_string(), None));
        let state = State {
            repos: [(repo.clone(), RepoState::new(main_branch))]
                .into_iter()
                .collect(),
        };

        let updates = state.compute_lkg_updates(&git_repo, &repo, None).unwrap();
        assert!(matches!(updates.get("feature"), None | Some(None)));
    }

    #[test]
    fn apply_lkg_updates_reports_no_change_when_values_match() {
        let mut main_branch = Branch::new("main".to_string(), None);
        main_branch.branches.push(Branch::new(
            "feature".to_string(),
            Some("abc123".to_string()),
        ));
        let mut state = State {
            repos: [("repo".to_string(), RepoState::new(main_branch))]
                .into_iter()
                .collect(),
        };

        let updates = [("feature".to_string(), Some("abc123".to_string()))]
            .into_iter()
            .collect();
        let changed = state.apply_lkg_updates("repo", updates).unwrap();
        assert!(!changed);
        assert_eq!(
            state.get_tree_branch("repo", "feature").unwrap().lkg_parent,
            Some("abc123".to_string())
        );
    }

    #[test]
    fn apply_lkg_updates_reports_change_when_value_differs() {
        let mut main_branch = Branch::new("main".to_string(), None);
        main_branch.branches.push(Branch::new(
            "feature".to_string(),
            Some("abc123".to_string()),
        ));
        let mut state = State {
            repos: [("repo".to_string(), RepoState::new(main_branch))]
                .into_iter()
                .collect(),
        };

        let updates = [("feature".to_string(), Some("def456".to_string()))]
            .into_iter()
            .collect();
        let changed = state.apply_lkg_updates("repo", updates).unwrap();
        assert!(changed);
        assert_eq!(
            state.get_tree_branch("repo", "feature").unwrap().lkg_parent,
            Some("def456".to_string())
        );
    }

    #[test]
    fn compute_lkg_updates_respects_scope() {
        // Tree `main → a → b`, both needing a merge-base backfill.
        let dir = tempfile::tempdir().unwrap();
        init_test_repo(dir.path());
        let main_sha = git_rev_parse(dir.path(), "main");

        // `a` branches off main at the root, gets a commit, then main diverges so neither `a`
        // nor `b` is a fast-forward descendant of main's tip (forcing merge-base backfill).
        git_run(dir.path(), &["checkout", "-b", "a"]);
        git_run(dir.path(), &["commit", "--allow-empty", "-q", "-m", "a1"]);
        let a_sha = git_rev_parse(dir.path(), "a");
        git_run(dir.path(), &["checkout", "-b", "b"]);
        git_run(dir.path(), &["commit", "--allow-empty", "-q", "-m", "b1"]);
        git_run(dir.path(), &["checkout", "main"]);
        git_run(
            dir.path(),
            &["commit", "--allow-empty", "-q", "-m", "main2"],
        );

        let git_repo =
            GitRepo::open_with_cache_at(dir.path(), &dir.path().join("mb_cache.redb")).unwrap();
        let repo = repo_key(dir.path());

        let mut main_branch = Branch::new("main".to_string(), None);
        let mut a_branch = Branch::new("a".to_string(), None);
        a_branch.branches.push(Branch::new("b".to_string(), None));
        main_branch.branches.push(a_branch);
        let state = State {
            repos: [(repo.clone(), RepoState::new(main_branch))]
                .into_iter()
                .collect(),
        };

        // Scope to just `a`: entry for `a`, none for `b`.
        let scope_a: HashSet<String> = ["a".to_string()].into_iter().collect();
        let updates = state
            .compute_lkg_updates(&git_repo, &repo, Some(&scope_a))
            .unwrap();
        assert_eq!(updates.get("a"), Some(&Some(main_sha.clone())));
        assert!(!updates.contains_key("b"));

        // Scope to just `b`: entry for `b`, none for `a` — proving children of an out-of-scope
        // branch are still traversed and computed.
        let scope_b: HashSet<String> = ["b".to_string()].into_iter().collect();
        let updates = state
            .compute_lkg_updates(&git_repo, &repo, Some(&scope_b))
            .unwrap();
        assert_eq!(updates.get("b"), Some(&Some(a_sha)));
        assert!(!updates.contains_key("a"));
    }

    /// Build a `main → mine → theirs → mychild` tree for the pure `eager_lkg_scope` tests.
    fn author_scope_state(repo: &str) -> State {
        let mut mine = Branch::new("mine".to_string(), None);
        let mut theirs = Branch::new("theirs".to_string(), None);
        theirs
            .branches
            .push(Branch::new("mychild".to_string(), None));
        mine.branches.push(theirs);
        let mut main_branch = Branch::new("main".to_string(), None);
        main_branch.branches.push(mine);
        State {
            repos: [(repo.to_string(), RepoState::new(main_branch))]
                .into_iter()
                .collect(),
        }
    }

    #[test]
    fn eager_lkg_scope_excludes_foreign_closed_pr_authors() {
        let repo = "repo";
        let state = author_scope_state(repo);
        let authors_filter = vec!["alice".to_string()];
        let closed_pr_authors: HashMap<String, String> =
            [("theirs".to_string(), "bob".to_string())]
                .into_iter()
                .collect();

        let scope = state.eager_lkg_scope(repo, "mine", &authors_filter, &closed_pr_authors);
        assert!(scope.contains("main"));
        assert!(scope.contains("mine"));
        assert!(scope.contains("mychild")); // no cache entry ⇒ kept
        assert!(!scope.contains("theirs")); // foreign closed-PR author ⇒ excluded
    }

    #[test]
    fn apply_prune_removes_foreign_and_adopts_children() {
        // Tree: main → mine → theirs → mychild. Pruning `theirs` should remove it and re-parent
        // its child `mychild` onto `mine`.
        let repo = "repo";
        let mut state = author_scope_state(repo);
        let tree = &mut state.repos.get_mut(repo).unwrap().tree;

        let to_remove: HashSet<String> = ["theirs".to_string()].into_iter().collect();
        let (removed, remounted) = apply_prune(tree, &to_remove);

        assert_eq!(removed, vec!["theirs".to_string()]);
        assert_eq!(remounted, vec![("mychild".to_string(), "mine".to_string())]);
        assert!(!is_branch_mentioned_in_tree("theirs", tree));

        // `mychild` is now a direct child of `mine`.
        let mine = find_branch_by_name(tree, "mine").unwrap();
        assert!(mine.branches.iter().any(|b| b.name == "mychild"));
    }

    #[test]
    fn cleanup_prune_set_excludes_protected() {
        // Current branch is `mychild`, so its ancestor `theirs` is protected even though it's
        // authored by `bob` (outside authors_filter) — the prune set must be empty.
        let repo = "repo";
        let state = author_scope_state(repo);
        let tree = state.get_tree(repo).unwrap();
        let authors_filter = vec!["alice".to_string()];
        let pr_authors: HashMap<String, String> = [("theirs".to_string(), "bob".to_string())]
            .into_iter()
            .collect();

        let to_prune = crate::render::tree_data::compute_hidden_branches(
            tree,
            "mychild",
            &authors_filter,
            &pr_authors,
            false,
        );
        assert!(to_prune.is_empty());
    }

    #[test]
    fn cleanup_prune_set_selects_foreign() {
        // Current branch is `mine`, so `theirs` is off the ancestor chain and its foreign author
        // makes it prunable.
        let repo = "repo";
        let state = author_scope_state(repo);
        let tree = state.get_tree(repo).unwrap();
        let authors_filter = vec!["alice".to_string()];
        let pr_authors: HashMap<String, String> = [("theirs".to_string(), "bob".to_string())]
            .into_iter()
            .collect();

        let to_prune = crate::render::tree_data::compute_hidden_branches(
            tree,
            "mine",
            &authors_filter,
            &pr_authors,
            false,
        );
        assert_eq!(
            to_prune,
            ["theirs".to_string()].into_iter().collect::<HashSet<_>>()
        );
    }

    #[test]
    fn cleanup_prune_empty_authors_filter() {
        // No author filter configured ⇒ nothing is ever pruned (no behavior change from before).
        let repo = "repo";
        let state = author_scope_state(repo);
        let tree = state.get_tree(repo).unwrap();
        let authors_filter: Vec<String> = Vec::new();
        let pr_authors: HashMap<String, String> = [("theirs".to_string(), "bob".to_string())]
            .into_iter()
            .collect();

        let to_prune = crate::render::tree_data::compute_hidden_branches(
            tree,
            "mine",
            &authors_filter,
            &pr_authors,
            false,
        );
        assert!(to_prune.is_empty());
    }

    #[test]
    fn eager_lkg_scope_protects_ancestor_chain() {
        let repo = "repo";
        let state = author_scope_state(repo);
        let authors_filter = vec!["alice".to_string()];
        let closed_pr_authors: HashMap<String, String> =
            [("theirs".to_string(), "bob".to_string())]
                .into_iter()
                .collect();

        // Current branch is `mychild`, so `theirs` is on the ancestor chain — always wins.
        let scope = state.eager_lkg_scope(repo, "mychild", &authors_filter, &closed_pr_authors);
        assert!(scope.contains("theirs"));
    }

    #[test]
    fn refresh_lkgs_scoped_skips_out_of_scope_branch() {
        // Redirect the XDG state file so save_state() can't clobber the real user state.
        // Safe under nextest's process-per-test isolation.
        let state_home = tempfile::tempdir().unwrap();
        fs::create_dir_all(state_home.path().join(env!("CARGO_PKG_NAME"))).unwrap();
        unsafe { std::env::set_var("XDG_STATE_HOME", state_home.path()) };

        let dir = tempfile::tempdir().unwrap();
        init_test_repo(dir.path());
        let sha_a = git_rev_parse(dir.path(), "main");

        git_run(dir.path(), &["checkout", "-b", "feature"]);
        git_run(
            dir.path(),
            &["commit", "--allow-empty", "-q", "-m", "feature commit"],
        );
        git_run(dir.path(), &["checkout", "main"]);
        git_run(
            dir.path(),
            &["commit", "--allow-empty", "-q", "-m", "main commit"],
        );

        let git_repo =
            GitRepo::open_with_cache_at(dir.path(), &dir.path().join("mb_cache.redb")).unwrap();
        let repo = repo_key(dir.path());

        let mut main_branch = Branch::new("main".to_string(), None);
        main_branch
            .branches
            .push(Branch::new("feature".to_string(), None));
        let mut state = State {
            repos: [(repo.clone(), RepoState::new(main_branch))]
                .into_iter()
                .collect(),
        };

        // Scope excludes `feature` (only `main` in scope) — its lkg_parent stays None.
        let scope: HashSet<String> = ["main".to_string()].into_iter().collect();
        state.refresh_lkgs_scoped(&git_repo, &repo, &scope).unwrap();
        assert_eq!(
            state.get_tree_branch(&repo, "feature").unwrap().lkg_parent,
            None
        );

        // A subsequent full refresh backfills it from the merge-base — recoverable lazily.
        state.refresh_lkgs(&git_repo, &repo).unwrap();
        assert_eq!(
            state.get_tree_branch(&repo, "feature").unwrap().lkg_parent,
            Some(sha_a)
        );
    }
}
