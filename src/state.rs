use std::{
    cell::{Cell, Ref, RefCell},
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    default,
    fs,
    path::{Path, PathBuf},
    process::Command,
    rc::Rc,
};

use anyhow::{Result, anyhow, bail, ensure};
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
}

impl RepoState {
    pub fn new(tree: Branch) -> Self {
        Self {
            tree,
            seen_remote_shas: HashSet::new(),
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
        let config_path = get_xdg_path()?;
        let mut used_existing_config = true;
        let data = match fs::read_to_string(&config_path) {
            Ok(data) => data,
            Err(error) => {
                tracing::warn!(
                    "Failed to read config file at {}: {}",
                    config_path.display(),
                    error
                );
                tracing::warn!("Using default (empty) config");
                used_existing_config = false;
                "".to_string()
            }
        };
        let state: Self = serde_yaml::from_str(&data)?;
        fs::create_dir_all(config_path.parent().unwrap())
            .inspect_err(|error| tracing::warn!("Failed to create config directory: {}", error))?;
        if !used_existing_config {
            tracing::info!("No existing config file found, creating a new one.");
            state
                .save_state()
                .inspect_err(|error| tracing::warn!("Failed to save config file: {}", error))?;
        }
        Ok(state)
    }

    pub fn save_state(&self) -> Result<()> {
        let config_path = get_xdg_path()?;
        tracing::trace!(?self, ?config_path, "Saving state to config file");
        Ok(write_file_secure(&config_path, &serde_yaml::to_string(&self)?)?)
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
    /// If there is an existing git-stack branch with the same name, check it out. If there isn't,
    /// then check whether the branch exists in the git repo. If it does, then let the user know
    /// that they need to use `git checkout` to check it out. If it doesn't, then create a new
    /// branch.
    pub fn checkout(
        &mut self,
        git_repo: &GitRepo,
        repo: &str,
        current_branch: String,
        current_upstream: Option<String>,
        branch_name: String,
    ) -> Result<()> {
        let trunk = git_trunk(git_repo)?;
        // Ensure the main branch is in the git-stack tree for this repo if we haven't
        // added it yet.
        self.repos
            .entry(repo.to_string())
            .or_insert_with(|| RepoState::new(Branch::new(trunk.main_branch.clone(), None)));
        self.save_state()?;

        let branch_exists_in_tree = self.branch_exists_in_tree(repo, &branch_name);

        if git_branch_exists(git_repo, &branch_name) {
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
    ) -> Result<Vec<RestackStep<'_>>> {
        tracing::debug!("Planning restack for {starting_branch}");
        let trunk = git_trunk(git_repo)?;
        // Find all the descendents of the starting branch.
        // Traverse the tree from the starting branch to the root,
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

    pub(crate) fn cleanup_missing_branches(
        &mut self,
        git_repo: &GitRepo,
        repo: &str,
        dry_run: bool,
        all: bool,
    ) -> Result<()> {
        if all {
            self.cleanup_all_trees(dry_run)
        } else {
            self.cleanup_single_tree(git_repo, repo, dry_run)
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

    fn cleanup_single_tree(&mut self, git_repo: &GitRepo, repo: &str, dry_run: bool) -> Result<()> {
        let Some(repo_state) = self.repos.get_mut(repo) else {
            println!("No stack tree found for repo {}", repo.yellow());
            return Ok(());
        };

        let mut removed_branches = Vec::new();
        let mut remounted_branches = Vec::new();

        // Recursively cleanup the tree
        cleanup_tree_recursive(
            git_repo,
            &mut repo_state.tree,
            &mut removed_branches,
            &mut remounted_branches,
        );

        if removed_branches.is_empty() {
            println!("No missing branches found. Tree is clean.");
            return Ok(());
        }

        // Print summary
        println!("Cleanup summary for {}:", repo.yellow());
        println!();
        println!("Removed branches (no longer exist locally):");
        for branch_name in &removed_branches {
            println!("  - {}", branch_name.red());
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
        } else {
            self.save_state()?;
            println!();
            println!("{}", "Changes saved.".green());
        }

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

    pub(crate) fn ensure_trunk(&mut self, git_repo: &GitRepo, repo: &str) -> Result<GitTrunk> {
        let trunk = git_trunk(git_repo)?;
        // The branch might not exist in git, let's create it, and add it to the tree.
        // Ensure the main branch is in the git-stack tree for this repo if we haven't
        // added it yet.
        self.repos
            .entry(repo.to_string())
            .or_insert_with(|| RepoState::new(Branch::new(trunk.main_branch.clone(), None)));
        Ok(trunk)
    }

    pub(crate) fn mount(
        &mut self,
        git_repo: &GitRepo,
        repo: &str,
        branch_name: &str,
        parent_branch: Option<String>,
    ) -> Result<()> {
        let trunk = self.ensure_trunk(git_repo, repo)?;

        if trunk.main_branch == branch_name {
            bail!(
                "Branch {branch_name} cannot be stacked on anything else.",
                branch_name = branch_name.red()
            );
        }

        let parent_branch = parent_branch.unwrap_or(trunk.main_branch);

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

    pub(crate) fn refresh_lkgs(&mut self, git_repo: &GitRepo, repo: &str) -> Result<()> {
        // For each sub-branch in the tree, check if the parent

        tracing::debug!("Refreshing lkgs for all branches...");
        let trunk = git_trunk(git_repo)?;

        let mut parent_lkgs: HashMap<String, Option<String>> = HashMap::default();

        // BFS Traverse the tree from the root to the leaves, and update the lkgs as we go.
        let mut queue: VecDeque<(Option<String>, String)> = VecDeque::new();
        queue.push_back((None, trunk.main_branch.clone()));
        while let Some((parent, branch)) = queue.pop_front() {
            // Skip branches that don't exist locally
            if !git_branch_exists(git_repo, &branch) {
                tracing::debug!("Skipping non-existent branch {} in refresh_lkgs", branch);
                continue;
            }

            if let Some(parent) = parent {
                let tree_branch = self.get_tree_branch(repo, &branch).unwrap();
                if let Some(lkg_parent) = tree_branch.lkg_parent.as_deref() {
                    if git_repo.is_ancestor(lkg_parent, &branch)? {
                        parent_lkgs.insert(tree_branch.name.clone(), Some(lkg_parent.to_string()));
                    } else {
                        parent_lkgs.insert(tree_branch.name.clone(), None);
                    }
                }
                if git_repo.is_ancestor(&parent, &branch).unwrap_or(false)
                    && let Ok(new_lkg_parent) = git_repo.sha(&parent)
                {
                    tracing::debug!(
                        lkg_parent = ?new_lkg_parent,
                        "Branch {} is a descendent of {}",
                        branch.yellow(),
                        parent.yellow(),
                    );
                    // Save the LKG parent for the branch.
                    parent_lkgs.insert(branch.clone(), Some(new_lkg_parent));
                }
            }
            if let Some(branch) = self.get_tree_branch(repo, &branch) {
                for child_branch in &branch.branches {
                    queue.push_back((Some(branch.name.clone()), child_branch.name.clone()));
                }
            }
        }
        // Update the LKGs in the tree.
        for (branch, lkg_parent) in parent_lkgs {
            let branch = self
                .get_tree_branch_mut(repo, &branch)
                .ok_or_else(|| anyhow!("Branch {branch} not found in the git-stack tree."))?;
            branch.lkg_parent = lkg_parent;
        }
        self.save_state()?;
        Ok(())
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
        // Ensure the tree exists for this repo
        self.ensure_trunk(git_repo, repo)?;

        // Check if the branch is already in the tree
        if self.branch_exists_in_tree(repo, branch_name) {
            return Ok(false);
        }

        // Check if this branch exists in git
        if !git_branch_exists(git_repo, branch_name) {
            bail!("Branch {branch_name} does not exist in git");
        }

        // Get the tree for this repo (guaranteed to exist after ensure_trunk)
        let tree = self.get_tree(repo).expect("tree exists after ensure_trunk");

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
            let trunk = git_trunk(git_repo)?;
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
        if !git_branch_exists(git_repo, &child.name) {
            // This branch doesn't exist locally, mark it for removal
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
}
