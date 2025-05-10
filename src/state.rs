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
    pub fn checkout(
        &mut self,
        repo: &str,
        current_branch: String,
        current_upstream: Option<String>,
        branch_name: String,
    ) -> Result<()> {
        // Check if the branch name already exists.
        if self.is_branch_mentioned_already(repo, &branch_name) {
            run_git(&["checkout", &branch_name])?;
            return Ok(());
        }
        let remote_main = git_remote_main(DEFAULT_REMOTE)?;
        // Figure out the branch name.
        let main_branch = after_text(&remote_main, format!("{DEFAULT_REMOTE}/"))
            .ok_or(anyhow!("no branch?"))?
            .to_string();
        if current_branch == main_branch {
            // Allow creation of the main branch for this repo if we haven't added it yet.
            self.trees
                .entry(repo.to_string())
                .or_insert_with(|| Branch::new(current_branch.clone()));
        }
        let branch = self
            .get_tree_branch_mut(repo, &current_branch)
            .ok_or_else(|| anyhow::anyhow!("Branch '{current_branch}' is not being tracked."))?;
        if !self.trees.contains_key(repo) {
            self.trees
                .insert(repo.to_string(), Branch::new(current_branch.clone()));
        }

        // Save the state after modifying it.
        save_state(self)?;

        // Checkout the new branch.
        run_git(&["checkout", &branch_name])?;

        Ok(())
    }

    fn is_branch_mentioned_already(&self, repo: &str, branch_name: &str) -> bool {
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
