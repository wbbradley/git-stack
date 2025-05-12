use anyhow::{Result, anyhow, bail, ensure};
use colored::Colorize;
use serde::{Deserialize, Serialize};
use std::{
    cell::{Cell, Ref, RefCell},
    collections::BTreeMap,
    fs,
    path::PathBuf,
    rc::Rc,
};

use crate::{
    git::{DEFAULT_REMOTE, after_text, git_branch_exists, git_remote_main},
    run_git,
};

#[derive(Debug, Serialize, Deserialize)]
pub struct Branch {
    /// The name of the branch or ref.
    pub name: String,
    /// The upstream branch reference.
    pub branches: Vec<Branch>,
}

impl Branch {
    pub fn new(name: String) -> Self {
        Self {
            name,
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
    pub fn get_tree(&self, repo: &str) -> Option<&Branch> {
        self.trees.get(repo)
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
        let branch_exists_in_tree = self.branch_exists_in_tree(repo, &branch_name);
        let current_branch_exists_in_tree = self.branch_exists_in_tree(repo, &current_branch);

        if git_branch_exists(&branch_name) {
            if !branch_exists_in_tree {
                tracing::warn!(
                    "Branch {branch_name} exists in the git repo but is not tracked by git-stack. \
                    If you'd like to add it to the git-stack, please checkout the desired parent \
                    branch, and then run `git-stack add {branch_name}` to stack {branch_name} on \
                    top of the parent branch.",
                );
            }
            run_git(&["checkout", &branch_name])?;
            return Ok(());
        }
        // The branch does not exist in git, let's create it, and add it to the tree.
        let remote_main = git_remote_main(DEFAULT_REMOTE)?;
        let main_branch = after_text(&remote_main, format!("{DEFAULT_REMOTE}/"))
            .ok_or(anyhow!("no branch?"))?
            .to_string();
        // Ensure the main branch is in the git-stack tree for this repo if we haven't
        // added it yet.
        self.trees
            .entry(repo.to_string())
            .or_insert_with(|| Branch::new(main_branch.clone()));

        let branch = self
            .get_tree_branch_mut(repo, &current_branch)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Branch '{current_branch}' is not being tracked in the git-stack tree."
                )
            })?;

        branch.branches.push(Branch::new(branch_name.clone()));

        // Actually create the git branch.
        run_git(&["checkout", "-b", &branch_name, &current_branch])?;

        println!(
            "Branch {branch_name} created and checked out.",
            branch_name = branch_name.yellow()
        );

        // Save the state after modifying it.
        save_state(self)?;

        Ok(())
    }

    fn branch_exists_in_tree(&self, repo: &str, branch_name: &str) -> bool {
        let Some(branch) = self.trees.get(repo) else {
            return false;
        };
        is_branch_mentioned_in_tree(branch_name, branch)
    }
    fn get_tree_branch<'a>(&'a self, repo: &str, branch_name: &str) -> Option<&'a Branch> {
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
        &self,
        repo: &str,
        starting_branch: &str,
    ) -> Result<Vec<RebaseStep>> {
        tracing::debug!("Planning restack for {starting_branch}");
        let remote_main = git_remote_main(DEFAULT_REMOTE)?;
        // Find all the descendents of the starting branch.
        // Traverse the tree from the starting branch to the root,
        let mut path: Vec<&str> = vec![];
        if !get_path(self.trees.get(repo).unwrap(), starting_branch, &mut path) {
            bail!("Branch {starting_branch} not found in the git-stack tree.");
        }
        path.insert(0, &remote_main);
        Ok(path
            .iter()
            .zip(path.iter().skip(1))
            .map(|(parent, child)| RebaseStep {
                parent: parent.to_string(),
                branch: child.to_string(),
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

        save_state(self)?;

        Ok(())
    }

    pub(crate) fn mount(
        &mut self,
        repo: &str,
        branch_name: &str,
        parent_branch: Option<String>,
    ) -> Result<()> {
        let parent_branch = match parent_branch {
            Some(parent_branch) => parent_branch,
            None => {
                tracing::info!("No parent branch specified, using main branch.");
                // The branch might not exist in git, let's create it, and add it to the tree.
                let remote_main = git_remote_main(DEFAULT_REMOTE)?;
                let main_branch = after_text(&remote_main, format!("{DEFAULT_REMOTE}/"))
                    .ok_or(anyhow!("no branch?"))?
                    .to_string();
                // Ensure the main branch is in the git-stack tree for this repo if we haven't
                // added it yet.
                self.trees
                    .entry(repo.to_string())
                    .or_insert_with(|| Branch::new(main_branch.clone()));
                main_branch
            }
        };

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
        ) {
            if name_a == name_b {
                tracing::warn!("Branch {branch_name} is already mounted on {name_a}");
                return Ok(());
            }
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
            new_parent_branch
                .branches
                .push(Branch::new(branch_name.to_string()));
        } else {
            bail!("Parent branch {parent_branch} not found in the git-stack tree.");
        }
        println!(
            "Branch {branch_name} stacked on {parent_branch}.",
            branch_name = branch_name.yellow(),
            parent_branch = parent_branch.yellow()
        );

        save_state(self)?;
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
}

fn get_path<'a>(branch: &'a Branch, target_branch: &str, path: &mut Vec<&'a str>) -> bool {
    if branch.name == target_branch {
        path.insert(0, &branch.name);
        return true;
    }
    for child_branch in &branch.branches {
        if get_path(child_branch, target_branch, path) {
            path.insert(0, &branch.name);
            return true;
        }
    }
    false
}

#[derive(Debug)]
pub(crate) struct RebaseStep {
    pub(crate) parent: String,
    pub(crate) branch: String,
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

pub fn load_state() -> anyhow::Result<State> {
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
    let state: State = serde_yaml::from_str(&data)?;
    fs::create_dir_all(config_path.parent().unwrap())
        .inspect_err(|error| tracing::warn!("Failed to create config directory: {}", error))?;
    if !used_existing_config {
        tracing::info!("No existing config file found, creating a new one.");
        save_state(&state)
            .inspect_err(|error| tracing::warn!("Failed to save config file: {}", error))?;
    }
    Ok(state)
}

pub fn save_state(state: &State) -> Result<()> {
    let config_path = get_xdg_path()?;
    tracing::debug!(?state, ?config_path, "Saving state to config file");
    Ok(fs::write(config_path, serde_yaml::to_string(&state)?)?)
}

fn get_xdg_path() -> anyhow::Result<PathBuf> {
    let base_dirs = xdg::BaseDirectories::with_prefix(env!("CARGO_PKG_NAME"));
    base_dirs
        .get_state_file("state.yaml")
        .ok_or_else(|| anyhow::anyhow!("Failed to find state file"))
}
