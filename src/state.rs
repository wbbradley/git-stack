use std::{
    cell::{Cell, Ref, RefCell},
    collections::{BTreeMap, HashMap, VecDeque},
    default,
    fs,
    path::PathBuf,
    process::Command,
    rc::Rc,
};

use anyhow::{Result, anyhow, bail, ensure};
use colored::Colorize;
use serde::{Deserialize, Serialize};

use crate::{
    git::{
        DEFAULT_REMOTE,
        GitTrunk,
        after_text,
        git_branch_exists,
        git_remote_main,
        git_sha,
        git_trunk,
        is_ancestor,
    },
    run_git,
};

#[derive(Clone, Copy, Default, Debug, Serialize, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum StackMethod {
    /// Uses `git format-patch` and `git am` to restack branches.
    #[default]
    ApplyMerge,
    /// Uses `git merge` to pull in changes from the parent branch.
    Merge,
}

#[derive(Debug, Serialize, Deserialize)]
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
            branches: vec![],
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct State {
    /// The directory name is the key, and the value is a vector of stacks.
    #[serde(flatten, default)]
    pub trees: BTreeMap<String, Branch>,
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
        tracing::debug!(?self, ?config_path, "Saving state to config file");
        Ok(fs::write(config_path, serde_yaml::to_string(&self)?)?)
    }

    pub fn get_tree(&self, repo: &str) -> Option<&Branch> {
        self.trees.get(repo)
    }
    pub fn get_tree_mut(&mut self, repo: &str) -> Option<&mut Branch> {
        self.trees.get_mut(repo)
    }
    /// If there is an existing git-stack branch with the same name, check it out. If there isn't,
    /// then check whether the branch exists in the git repo. If it does, then let the user know
    /// that they need to use `git checkout` to check it out. If it doesn't, then create a new
    /// branch.
    pub fn checkout(
        &mut self,
        repo: &str,
        current_branch: String,
        current_upstream: Option<String>,
        branch_name: String,
    ) -> Result<()> {
        let trunk = git_trunk()?;
        // Ensure the main branch is in the git-stack tree for this repo if we haven't
        // added it yet.
        self.trees
            .entry(repo.to_string())
            .or_insert_with(|| Branch::new(trunk.main_branch.clone(), None));
        self.save_state()?;

        let branch_exists_in_tree = self.branch_exists_in_tree(repo, &branch_name);

        if git_branch_exists(&branch_name) {
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
            git_sha(&current_branch).ok(),
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
        let Some(branch) = self.trees.get(repo) else {
            return false;
        };
        is_branch_mentioned_in_tree(branch_name, branch)
    }

    pub fn get_tree_branch<'a>(&'a self, repo: &str, branch_name: &str) -> Option<&'a Branch> {
        self.trees
            .get(repo)
            .and_then(|tree| find_branch_by_name(tree, branch_name))
    }

    fn get_tree_branch_mut<'a>(
        &'a mut self,
        repo: &str,
        branch_name: &str,
    ) -> Option<&'a mut Branch> {
        self.trees
            .get_mut(repo)
            .and_then(|tree| find_branch_by_name_mut(tree, branch_name))
    }

    pub(crate) fn plan_restack(
        &'_ self,
        repo: &str,
        starting_branch: &str,
    ) -> Result<Vec<RestackStep<'_>>> {
        tracing::debug!("Planning restack for {starting_branch}");
        let trunk = git_trunk()?;
        // Find all the descendents of the starting branch.
        // Traverse the tree from the starting branch to the root,
        let mut path: Vec<&Branch> = vec![];
        if !get_path(self.trees.get(repo).unwrap(), starting_branch, &mut path) {
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
        let Some(tree) = self
            .trees
            .get_mut(repo)
            .and_then(|tree| find_parent_of_branch_mut(tree, branch_name))
        else {
            bail!("Branch {branch_name} not found in the git-stack tree.");
        };
        tree.branches.retain(|branch| branch.name != branch_name);
        println!(
            "Branch {branch_name} removed from git-stack tree.",
            branch_name = branch_name.yellow()
        );

        self.save_state()?;

        Ok(())
    }

    pub(crate) fn ensure_trunk(&mut self, repo: &str) -> Result<GitTrunk> {
        let trunk = git_trunk()?;
        // The branch might not exist in git, let's create it, and add it to the tree.
        // Ensure the main branch is in the git-stack tree for this repo if we haven't
        // added it yet.
        self.trees
            .entry(repo.to_string())
            .or_insert_with(|| Branch::new(trunk.main_branch.clone(), None));
        Ok(trunk)
    }

    pub(crate) fn mount(
        &mut self,
        repo: &str,
        branch_name: &str,
        parent_branch: Option<String>,
    ) -> Result<()> {
        let trunk = self.ensure_trunk(repo)?;

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

        let current_parent_branch = self.get_parent_branch_of_mut(repo, branch_name);

        if let Some(current_parent_branch) = current_parent_branch {
            // Remove the branch from the current parent branch.
            current_parent_branch
                .branches
                .retain(|branch| branch.name != branch_name);
        }

        let new_parent_branch = self.get_tree_branch_mut(repo, &parent_branch);
        // Add the branch to the new parent branch.
        if let Some(new_parent_branch) = new_parent_branch {
            new_parent_branch.branches.push(Branch::new(
                branch_name.to_string(),
                git_sha(&parent_branch).ok(),
            ));
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
        self.trees
            .get(repo)
            .and_then(|tree| find_parent_of_branch(tree, branch_name))
    }
    pub fn get_parent_branch_of_mut(
        &mut self,
        repo: &str,
        branch_name: &str,
    ) -> Option<&mut Branch> {
        self.trees
            .get_mut(repo)
            .and_then(|tree| find_parent_of_branch_mut(tree, branch_name))
    }

    pub(crate) fn refresh_lkgs(&mut self, repo: &str) -> Result<()> {
        // For each sub-branch in the tree, check if the parent

        tracing::debug!("Refreshing lkgs for all branches...");
        let trunk = git_trunk()?;

        let mut parent_lkgs: HashMap<String, Option<String>> = HashMap::default();

        // BFS Traverse the tree from the root to the leaves, and update the lkgs as we go.
        let mut queue: VecDeque<(Option<String>, String)> = VecDeque::new();
        queue.push_back((None, trunk.main_branch.clone()));
        while let Some((parent, branch)) = queue.pop_front() {
            if let Some(parent) = parent {
                let tree_branch = self.get_tree_branch(repo, &branch).unwrap();
                if let Some(lkg_parent) = tree_branch.lkg_parent.as_deref() {
                    if is_ancestor(lkg_parent, &branch)? {
                        parent_lkgs.insert(tree_branch.name.clone(), Some(lkg_parent.to_string()));
                    } else {
                        parent_lkgs.insert(tree_branch.name.clone(), None);
                    }
                }
                if is_ancestor(&parent, &branch).unwrap_or(false)
                    && let Ok(new_lkg_parent) = git_sha(&parent)
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

fn get_xdg_path() -> anyhow::Result<PathBuf> {
    let base_dirs = xdg::BaseDirectories::with_prefix(env!("CARGO_PKG_NAME"));
    base_dirs
        .get_state_file("state.yaml")
        .ok_or_else(|| anyhow::anyhow!("Failed to find state file"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_state_write() {
        let state = super::State {
            trees: vec![(
                "/tmp/foo".to_string(),
                super::Branch {
                    name: "main".to_string(),
                    stack_method: super::StackMethod::ApplyMerge,
                    note: None,
                    lkg_parent: None,
                    branches: vec![],
                },
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
        let state: super::State = serde_yaml::from_str(state).unwrap();
        assert_eq!(state.trees.len(), 1);
        assert!(state.trees.contains_key("/tmp/foo"));
        let tree = state.trees.get("/tmp/foo").unwrap();
        assert_eq!(tree.name, "main");
        assert_eq!(tree.stack_method, super::StackMethod::ApplyMerge);
    }
}
