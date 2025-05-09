use serde::{Deserialize, Serialize};
use std::{collections::HashMap, fs, path::PathBuf};

#[derive(Debug, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub directories: HashMap<String, DirectoryConfig>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct DirectoryConfig {
    pub stacks: Vec<Stack>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Stack {
    pub branches: Vec<String>,
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
    Ok(fs::write(config_path, toml::to_string(&state)?)?)
}

fn get_xdg_path() -> anyhow::Result<PathBuf> {
    let base_dirs = xdg::BaseDirectories::with_prefix(env!("CARGO_PKG_NAME"));
    base_dirs
        .get_state_file("state.toml")
        .ok_or_else(|| anyhow::anyhow!("Failed to find state file"))
}
