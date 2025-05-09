use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, hash_map::Entry},
    fs,
    path::PathBuf,
};

#[derive(Debug, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub directories: HashMap<String, Directory>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Directory {
    pub stacks: Vec<Stack>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Stack {
    pub branches: Vec<String>,
}

impl State {
    pub fn get_stacks(&self, dir_key: &str) -> Vec<Stack> {
        self.directories
            .get(dir_key)
            .map(|dir| dir.stacks.to_vec())
            .unwrap_or_default()
    }
    pub fn add_stack(&mut self, dir_key: &str, branch_name: &str) -> Result<()> {
        match self.directories.entry(dir_key.to_string()) {
            Entry::Occupied(mut e) => {
                tracing::info!("Adding stack to existing directory: {}", dir_key);
                let branch_name = branch_name.to_string();
                let d = e.get_mut();
                if d.stacks.iter().any(|s| s.branches.contains(&branch_name)) {
                    bail!("Stack with name {} already exists", branch_name);
                }
                d.stacks.push(Stack {
                    branches: vec![branch_name],
                });
                Ok(())
            }
            Entry::Vacant(vacant_entry) => {
                vacant_entry.insert(Directory {
                    stacks: vec![Stack {
                        branches: vec![branch_name.to_string()],
                    }],
                });
                Ok(())
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
    let state: State = toml::from_str(&data)?;
    fs::create_dir_all(config_path.parent().unwrap())
        .inspect_err(|error| tracing::warn!("Failed to create config directory: {}", error))?;
    if !used_existing_config {
        tracing::info!("No existing config file found, creating a new one.");
    }
    save_state(&state)
        .inspect_err(|error| tracing::warn!("Failed to save config file: {}", error))?;
    Ok(state)
}

pub fn save_state(state: &State) -> anyhow::Result<()> {
    let config_path = get_xdg_path()?;
    tracing::info!(?state, ?config_path, "Saving state to config file");
    Ok(fs::write(config_path, toml::to_string(&state)?)?)
}

fn get_xdg_path() -> anyhow::Result<PathBuf> {
    let base_dirs = xdg::BaseDirectories::with_prefix(env!("CARGO_PKG_NAME"));
    base_dirs
        .get_state_file("state.toml")
        .ok_or_else(|| anyhow::anyhow!("Failed to find state file"))
}
