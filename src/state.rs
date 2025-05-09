use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, hash_map::Entry},
    fs,
    path::PathBuf,
};

use crate::run_git_status;

#[derive(Debug, Serialize, Deserialize)]
pub struct State {
    #[serde(flatten, default)]
    pub directories: HashMap<String, Vec<Vec<String>>>,
}

impl State {
    pub fn get_stacks(&self, dir_key: &str) -> Vec<Vec<String>> {
        self.directories
            .get(dir_key)
            .map(|dir| dir.to_vec())
            .unwrap_or_default()
    }
    /// Adds a new stack to the repo starting with the given branch name.
    pub fn create_new_stack_with_existing_branch(
        &mut self,
        dir_key: &str,
        branch_name: &str,
    ) -> Result<()> {
        match self.directories.entry(dir_key.to_string()) {
            Entry::Occupied(mut e) => {
                tracing::debug!("create stack in an existing directory: {}", dir_key);
                let branch_name = branch_name.to_string();
                let d = e.get_mut();
                ensure!(
                    !d.iter().any(|s| s.contains(&branch_name)),
                    "a stack with a branch named '{}' already exists",
                    branch_name
                );
                d.push(vec![branch_name]);
                Ok(())
            }
            Entry::Vacant(vacant_entry) => {
                tracing::debug!("create stack in a new directory: {}", dir_key);
                vacant_entry.insert(vec![vec![branch_name.to_string()]]);
                Ok(())
            }
        }
    }
    /// Adds a new branch to the stack of the current branch, and returns the Git SHA where the new
    /// branch should be created.
    pub fn add_to_stack(
        &mut self,
        dir_key: &str,
        current_branch: &str,
        branch_name: &str,
    ) -> Result<String> {
        let branch_name = branch_name.to_string();
        match self.directories.entry(dir_key.to_string()) {
            Entry::Occupied(mut entry) => {
                tracing::debug!("adding to existing directory: {}", dir_key);
                let stacks = entry.get_mut();
                check_branch_does_not_exist(&branch_name, stacks)?;

                let stack = find_stack_with_branch(stacks, current_branch)?;
                assert!(!stack.is_empty());
                let top_branch = stack.last().unwrap().clone();
                stack.push(branch_name);
                Ok(top_branch)
            }
            Entry::Vacant(_) => {
                bail!("No stack found for directory {}", dir_key);
            }
        }
    }
    //    let stacks: Vec<Stack> = state.get_stacks(&dir_key);
    //    if stacks.is_empty() {
    //        state.add_stack(dir_key.clone(), name);
    //    }
    //    state.directories.insert(
    //        _dir_key.clone(),
    //        state::DirectoryConfig {
    //            stacks: vec![Stack {
    //                branches: vec![_name],
    //            }],
    //        },
    //    );
    //}
}

fn check_branch_does_not_exist(branch_name: &String, stacks: &mut [Vec<String>]) -> Result<()> {
    if run_git_status(&["rev-parse", branch_name])?.success() {
        bail!("branch '{}' already exists", branch_name);
    }
    // Check whether this name is already in use.
    if stacks.iter().any(|s| s.contains(branch_name)) {
        bail!("Stack with name {} already exists", branch_name);
    }
    Ok(())
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
