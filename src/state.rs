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
            println!("AAA");
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
    fn get_tree_branch_mut<'a>(
        &'a mut self,
        repo: &str,
        branch_name: &str,
    ) -> Option<&'a mut Branch> {
        self.trees
            .get_mut(repo)
            .and_then(|tree| get_branch_mut(tree, branch_name))
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

fn get_branch_mut<'a>(tree: &'a mut Branch, name: &str) -> Option<&'a mut Branch> {
    if tree.name == name {
        Some(tree)
    } else {
        for child_branch in tree.branches.iter_mut() {
            let result = get_branch_mut(child_branch, name);
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

pub fn save_state(state: &State) -> anyhow::Result<()> {
    let config_path = get_xdg_path()?;
    tracing::info!(?state, ?config_path, "Saving state to config file");
    Ok(fs::write(config_path, serde_yaml::to_string(&state)?)?)
}

fn get_xdg_path() -> anyhow::Result<PathBuf> {
    let base_dirs = xdg::BaseDirectories::with_prefix(env!("CARGO_PKG_NAME"));
    base_dirs
        .get_state_file("state.yaml")
        .ok_or_else(|| anyhow::anyhow!("Failed to find state file"))
}
