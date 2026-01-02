//! GitHub API client for git-stack PR integration.
//!
//! This module provides direct GitHub REST API access without
//! depending on the `gh` CLI tool.

use std::{
    fs,
    io::{self, Write},
    path::PathBuf,
    process::Command,
};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::git2_ops::GitRepo;

// ============== Configuration Types ==============

/// GitHub authentication configuration
#[derive(Debug, Clone)]
pub struct GitHubConfig {
    pub token: String,
    pub api_base: String,
    /// GitHub usernames whose PRs should be synced.
    /// When non-empty, only PRs from these authors will be synced.
    /// When empty, PRs from forks will be excluded.
    pub sync_authors: Vec<String>,
}

/// Repository identification (owner/repo extracted from remote URL)
#[derive(Debug, Clone)]
pub struct RepoIdentifier {
    pub owner: String,
    pub repo: String,
    pub host: String,
}

impl RepoIdentifier {
    /// Returns the full repo path as "owner/repo"
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

// ============== API Response Types ==============

/// Minimal PR info for status display
#[derive(Debug, Clone, Deserialize)]
pub struct PullRequest {
    pub number: u64,
    pub state: PrState,
    pub title: String,
    pub html_url: String,
    pub base: PrBranchRef,
    pub head: PrBranchRef,
    /// The user who created this PR
    pub user: PrUser,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub merged: bool,
    /// Timestamp when PR was merged (present in list endpoint, unlike `merged` field)
    #[serde(default)]
    pub merged_at: Option<String>,
}

/// Minimal user info for PR author
#[derive(Debug, Clone, Deserialize)]
pub struct PrUser {
    pub login: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PrBranchRef {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub sha: String,
    /// Repository info (may be null if the fork was deleted)
    pub repo: Option<PrRepoRef>,
}

/// Minimal repo info for PR head/base references
#[derive(Debug, Clone, Deserialize)]
pub struct PrRepoRef {
    /// Full name of the repo (e.g., "owner/repo")
    pub full_name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PrState {
    Open,
    Closed,
}

/// Display-friendly PR state (computed from API fields)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrDisplayState {
    Draft,
    Open,
    Merged,
    Closed,
}

impl std::fmt::Display for PrDisplayState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Draft => write!(f, "draft"),
            Self::Open => write!(f, "open"),
            Self::Merged => write!(f, "merged"),
            Self::Closed => write!(f, "closed"),
        }
    }
}

impl PullRequest {
    /// Check if this PR was merged (handles both `merged` field and `merged_at` field)
    pub fn is_merged(&self) -> bool {
        self.merged || self.merged_at.is_some()
    }

    /// Check if this PR is from a fork (head repo differs from base repo)
    ///
    /// Returns true if:
    /// - The head repo is missing (fork was deleted)
    /// - The head repo full_name differs from the base repo full_name
    pub fn is_from_fork(&self) -> bool {
        match (&self.head.repo, &self.base.repo) {
            // If head repo is missing, the fork was probably deleted - treat as fork PR
            (None, _) => true,
            // If base repo is missing, something is weird but assume not a fork
            (_, None) => false,
            // Compare the full names
            (Some(head_repo), Some(base_repo)) => head_repo.full_name != base_repo.full_name,
        }
    }

    /// Get the display state for this PR
    pub fn display_state(&self) -> PrDisplayState {
        if self.is_merged() {
            PrDisplayState::Merged
        } else if self.state == PrState::Closed {
            PrDisplayState::Closed
        } else if self.draft {
            PrDisplayState::Draft
        } else {
            PrDisplayState::Open
        }
    }
}

/// PR creation request
#[derive(Debug, Serialize)]
pub struct CreatePrRequest<'a> {
    pub title: &'a str,
    pub body: &'a str,
    pub head: &'a str,
    pub base: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub draft: Option<bool>,
}

/// PR update request (for retargeting base)
#[derive(Debug, Serialize)]
pub struct UpdatePrRequest<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<&'a str>,
}

// ============== Error Types ==============

#[derive(Debug)]
pub enum GitHubError {
    /// No auth token configured
    NoToken,
    /// Token is invalid or expired
    Unauthorized,
    /// Rate limited (includes reset timestamp)
    RateLimited { reset_at: u64 },
    /// PR already exists for this head branch
    PrAlreadyExists { pr_number: u64 },
    /// Branch not found on remote
    BranchNotPushed { branch: String },
    /// Network/HTTP error
    Network(String),
    /// API error with message
    Api { status: u16, message: String },
}

impl std::fmt::Display for GitHubError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoToken => write!(f, "No GitHub token configured"),
            Self::Unauthorized => write!(f, "GitHub token is invalid or expired"),
            Self::RateLimited { reset_at } => {
                write!(f, "GitHub API rate limited until {}", reset_at)
            }
            Self::PrAlreadyExists { pr_number } => write!(f, "PR #{} already exists", pr_number),
            Self::BranchNotPushed { branch } => {
                write!(f, "Branch '{}' not pushed to remote", branch)
            }
            Self::Network(msg) => write!(f, "Network error: {}", msg),
            Self::Api { status, message } => {
                write!(f, "GitHub API error ({}): {}", status, message)
            }
        }
    }
}

impl std::error::Error for GitHubError {}

// ============== Client ==============

/// GitHub API client
pub struct GitHubClient {
    config: GitHubConfig,
}

impl GitHubClient {
    pub fn new(config: GitHubConfig) -> Self {
        Self { config }
    }

    /// Load config from environment/git config/config file
    pub fn from_env(repo_id: &RepoIdentifier) -> Result<Self, GitHubError> {
        let (token, sync_authors) = find_github_config(&repo_id.host)?;
        let api_base = if repo_id.host == "github.com" {
            "https://api.github.com".to_string()
        } else {
            format!("https://{}/api/v3", repo_id.host)
        };
        Ok(Self::new(GitHubConfig {
            token,
            api_base,
            sync_authors,
        }))
    }

    /// Get PR by number
    pub fn get_pr(
        &self,
        repo: &RepoIdentifier,
        pr_number: u64,
    ) -> Result<PullRequest, GitHubError> {
        let url = format!(
            "{}/repos/{}/{}/pulls/{}",
            self.config.api_base, repo.owner, repo.repo, pr_number
        );

        let mut response = ureq::get(&url)
            .header("Authorization", &format!("Bearer {}", self.config.token))
            .header("Accept", "application/vnd.github.v3+json")
            .header("User-Agent", "git-stack")
            .call()
            .map_err(|e| self.handle_ureq_error(e))?;

        response
            .body_mut()
            .read_json()
            .map_err(|e| GitHubError::Network(e.to_string()))
    }

    /// Find open PR for a branch (returns None if no PR exists)
    pub fn find_pr_for_branch(
        &self,
        repo: &RepoIdentifier,
        branch: &str,
    ) -> Result<Option<PullRequest>, GitHubError> {
        let url = format!(
            "{}/repos/{}/{}/pulls?head={}:{}&state=open",
            self.config.api_base, repo.owner, repo.repo, repo.owner, branch
        );

        let mut response = ureq::get(&url)
            .header("Authorization", &format!("Bearer {}", self.config.token))
            .header("Accept", "application/vnd.github.v3+json")
            .header("User-Agent", "git-stack")
            .call()
            .map_err(|e| self.handle_ureq_error(e))?;

        let prs: Vec<PullRequest> = response
            .body_mut()
            .read_json()
            .map_err(|e| GitHubError::Network(e.to_string()))?;

        Ok(prs.into_iter().next())
    }

    /// Create a new PR
    pub fn create_pr(
        &self,
        repo: &RepoIdentifier,
        request: CreatePrRequest,
    ) -> Result<PullRequest, GitHubError> {
        let url = format!(
            "{}/repos/{}/{}/pulls",
            self.config.api_base, repo.owner, repo.repo
        );

        let mut response = ureq::post(&url)
            .header("Authorization", &format!("Bearer {}", self.config.token))
            .header("Accept", "application/vnd.github.v3+json")
            .header("User-Agent", "git-stack")
            .send_json(&request)
            .map_err(|e| self.handle_ureq_error(e))?;

        response
            .body_mut()
            .read_json()
            .map_err(|e| GitHubError::Network(e.to_string()))
    }

    /// List PRs for a repository with a given state filter
    /// Returns a map of head branch name -> PullRequest for easy lookup
    pub fn list_prs(
        &self,
        repo: &RepoIdentifier,
        state: &str, // "open", "closed", or "all"
    ) -> Result<std::collections::HashMap<String, PullRequest>, GitHubError> {
        let mut all_prs = Vec::new();
        let mut page = 1;
        let per_page = 100;

        loop {
            let url = format!(
                "{}/repos/{}/{}/pulls?state={}&per_page={}&page={}",
                self.config.api_base, repo.owner, repo.repo, state, per_page, page
            );

            let mut response = ureq::get(&url)
                .header("Authorization", &format!("Bearer {}", self.config.token))
                .header("Accept", "application/vnd.github.v3+json")
                .header("User-Agent", "git-stack")
                .call()
                .map_err(|e| self.handle_ureq_error(e))?;

            let prs: Vec<PullRequest> = response
                .body_mut()
                .read_json()
                .map_err(|e| GitHubError::Network(e.to_string()))?;

            let count = prs.len();
            all_prs.extend(prs);

            // If we got fewer than per_page results, we've reached the end
            if count < per_page {
                break;
            }
            page += 1;
        }

        // Build map of head branch name -> PR, filtering out irrelevant PRs
        let pr_map: std::collections::HashMap<String, PullRequest> = all_prs
            .into_iter()
            .filter(|pr| {
                // If sync_authors is configured, only include PRs from those authors
                if !self.config.sync_authors.is_empty() {
                    let included = self.config.sync_authors.contains(&pr.user.login);
                    if !included {
                        tracing::debug!(
                            "Skipping PR #{} '{}' - author '{}' not in sync_authors",
                            pr.number,
                            pr.title,
                            pr.user.login
                        );
                    }
                    return included;
                }

                // Otherwise, filter out PRs from forks
                if pr.is_from_fork() {
                    tracing::debug!(
                        "Skipping PR #{} '{}' - from fork (head: {:?})",
                        pr.number,
                        pr.title,
                        pr.head.repo.as_ref().map(|r| &r.full_name)
                    );
                    return false;
                }

                true
            })
            .map(|pr| (pr.head.ref_name.clone(), pr))
            .collect();

        Ok(pr_map)
    }

    /// List all open PRs for a repository (convenience wrapper)
    pub fn list_open_prs(
        &self,
        repo: &RepoIdentifier,
    ) -> Result<std::collections::HashMap<String, PullRequest>, GitHubError> {
        self.list_prs(repo, "open")
    }

    /// Update PR (e.g., to retarget base)
    pub fn update_pr(
        &self,
        repo: &RepoIdentifier,
        pr_number: u64,
        request: UpdatePrRequest,
    ) -> Result<PullRequest, GitHubError> {
        let url = format!(
            "{}/repos/{}/{}/pulls/{}",
            self.config.api_base, repo.owner, repo.repo, pr_number
        );

        let mut response = ureq::patch(&url)
            .header("Authorization", &format!("Bearer {}", self.config.token))
            .header("Accept", "application/vnd.github.v3+json")
            .header("User-Agent", "git-stack")
            .send_json(&request)
            .map_err(|e| self.handle_ureq_error(e))?;

        response
            .body_mut()
            .read_json()
            .map_err(|e| GitHubError::Network(e.to_string()))
    }

    fn handle_ureq_error(&self, error: ureq::Error) -> GitHubError {
        // In ureq 3.x, errors are structured differently
        let msg = error.to_string();
        if msg.contains("401") {
            GitHubError::Unauthorized
        } else if msg.contains("403") {
            GitHubError::Api {
                status: 403,
                message: msg,
            }
        } else if msg.contains("422") {
            GitHubError::Api {
                status: 422,
                message: msg,
            }
        } else if msg.contains("404") {
            GitHubError::Api {
                status: 404,
                message: msg,
            }
        } else {
            GitHubError::Network(msg)
        }
    }
}

// ============== Helper Functions ==============

/// Parse GitHub remote URL to extract owner/repo
pub fn parse_remote_url(url: &str) -> Result<RepoIdentifier> {
    // Handle various URL formats:
    // - git@github.com:owner/repo.git
    // - https://github.com/owner/repo.git
    // - https://github.com/owner/repo
    // - ssh://git@github.com/owner/repo.git
    // - git://github.com/owner/repo.git

    let url = url.trim();

    // SSH format: git@github.com:owner/repo.git
    if let Some(rest) = url.strip_prefix("git@") {
        let parts: Vec<&str> = rest.splitn(2, ':').collect();
        if parts.len() == 2 {
            let host = parts[0].to_string();
            let path = parts[1].trim_end_matches(".git");
            let path_parts: Vec<&str> = path.splitn(2, '/').collect();
            if path_parts.len() == 2 {
                return Ok(RepoIdentifier {
                    host,
                    owner: path_parts[0].to_string(),
                    repo: path_parts[1].to_string(),
                });
            }
        }
    }

    // HTTPS/SSH URL format
    if url.starts_with("https://") || url.starts_with("ssh://") || url.starts_with("git://") {
        // Parse as URL
        let without_protocol = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("ssh://git@"))
            .or_else(|| url.strip_prefix("ssh://"))
            .or_else(|| url.strip_prefix("git://"))
            .unwrap_or(url);

        let parts: Vec<&str> = without_protocol.splitn(2, '/').collect();
        if parts.len() == 2 {
            let host = parts[0].to_string();
            let path = parts[1].trim_end_matches(".git");
            let path_parts: Vec<&str> = path.splitn(2, '/').collect();
            if path_parts.len() == 2 {
                return Ok(RepoIdentifier {
                    host,
                    owner: path_parts[0].to_string(),
                    repo: path_parts[1].to_string(),
                });
            }
        }
    }

    bail!(
        "Could not parse GitHub remote URL: {}. Expected format like 'git@github.com:owner/repo.git' or 'https://github.com/owner/repo'",
        url
    )
}

/// Get RepoIdentifier from the current git repository's origin remote
pub fn get_repo_identifier(git_repo: &GitRepo) -> Result<RepoIdentifier> {
    let remote_url = git_repo
        .get_remote_url("origin")
        .context("Failed to get origin remote URL")?;
    parse_remote_url(&remote_url)
}

/// Load GitHub configuration from XDG config file
fn load_github_config_file() -> Option<GitHubConfigFile> {
    let config_path = get_github_config_path().ok()?;
    let contents = fs::read_to_string(&config_path).ok()?;
    serde_yaml::from_str(&contents).ok()
}

/// Find GitHub token and config from various sources
fn find_github_config(host: &str) -> Result<(String, Vec<String>), GitHubError> {
    let config_file = load_github_config_file();
    let sync_authors = config_file
        .as_ref()
        .map(|c| c.sync_authors.clone())
        .unwrap_or_default();

    // 1. Check GITHUB_TOKEN env var
    if let Ok(token) = std::env::var("GITHUB_TOKEN")
        && !token.is_empty()
    {
        tracing::debug!("Using GitHub token from GITHUB_TOKEN env var");
        return Ok((token, sync_authors));
    }

    // 2. Check GH_TOKEN env var (used by gh CLI)
    if let Ok(token) = std::env::var("GH_TOKEN")
        && !token.is_empty()
    {
        tracing::debug!("Using GitHub token from GH_TOKEN env var");
        return Ok((token, sync_authors));
    }

    // 3. Check git config github.token
    if let Ok(output) = Command::new("git")
        .args(["config", "--get", "github.token"])
        .output()
        && output.status.success()
    {
        let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !token.is_empty() {
            tracing::debug!("Using GitHub token from git config");
            return Ok((token, sync_authors));
        }
    }

    // 4. Check XDG config file for token
    if let Some(config) = config_file {
        // Check for host-specific token first
        if let Some(hosts) = &config.hosts
            && let Some(token) = hosts.get(host)
        {
            tracing::debug!("Using GitHub token from config file (host-specific)");
            return Ok((token.clone(), sync_authors));
        }
        // Fall back to default token
        if let Some(token) = config.default_token {
            tracing::debug!("Using GitHub token from config file (default)");
            return Ok((token, sync_authors));
        }
    }

    Err(GitHubError::NoToken)
}

/// GitHub config file structure
#[derive(Debug, Default, Deserialize, Serialize)]
struct GitHubConfigFile {
    default_token: Option<String>,
    hosts: Option<std::collections::HashMap<String, String>>,
    /// GitHub usernames whose PRs should be synced.
    /// When set, only PRs from these authors will be synced.
    /// When empty/unset, PRs from forks will be excluded.
    #[serde(default)]
    sync_authors: Vec<String>,
}

/// Get path to GitHub config file
fn get_github_config_path() -> Result<PathBuf> {
    let base_dirs = xdg::BaseDirectories::with_prefix("git-stack");
    base_dirs
        .get_config_file("github.yaml")
        .ok_or_else(|| anyhow!("Failed to determine config file path"))
}

/// Save GitHub token to config file
pub fn save_github_token(token: &str) -> Result<()> {
    let base_dirs = xdg::BaseDirectories::with_prefix("git-stack");
    let config_path = base_dirs
        .place_config_file("github.yaml")
        .context("Failed to create config directory")?;

    // Load existing config to preserve other settings (like sync_authors)
    let mut config = load_github_config_file().unwrap_or_default();
    config.default_token = Some(token.to_string());

    let contents = serde_yaml::to_string(&config)?;
    fs::write(&config_path, contents)?;

    // Set restrictive permissions on the config file (Unix only)
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&config_path)?.permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&config_path, perms)?;
    }

    println!("Token saved to {}", config_path.display());
    Ok(())
}

/// Check if GitHub token is configured
pub fn has_github_token(host: &str) -> bool {
    find_github_config(host).is_ok()
}

/// Interactive token setup
pub fn setup_github_token_interactive() -> Result<String> {
    println!(
        "No GitHub token found. To manage PRs, git-stack needs a GitHub Personal Access Token."
    );
    println!();
    println!("Steps to create a token:");
    println!("1. Go to: https://github.com/settings/tokens/new");
    println!("2. Name: \"git-stack CLI\"");
    println!("3. Scopes needed: repo (full control of private repos)");
    println!("4. Click \"Generate token\" and copy the value");
    println!();
    print!("Enter your token: ");
    io::stdout().flush()?;

    let mut token = String::new();
    io::stdin().read_line(&mut token)?;
    let token = token.trim().to_string();

    if token.is_empty() {
        bail!("No token provided");
    }

    save_github_token(&token)?;
    Ok(token)
}

/// Open a URL in the default browser
pub fn open_in_browser(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        Command::new("open").arg(url).spawn()?;
    }
    #[cfg(target_os = "linux")]
    {
        Command::new("xdg-open").arg(url).spawn()?;
    }
    #[cfg(target_os = "windows")]
    {
        Command::new("cmd").args(["/c", "start", url]).spawn()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_ssh_url() {
        let repo = parse_remote_url("git@github.com:owner/repo.git").unwrap();
        assert_eq!(repo.host, "github.com");
        assert_eq!(repo.owner, "owner");
        assert_eq!(repo.repo, "repo");
    }

    #[test]
    fn test_parse_https_url() {
        let repo = parse_remote_url("https://github.com/owner/repo.git").unwrap();
        assert_eq!(repo.host, "github.com");
        assert_eq!(repo.owner, "owner");
        assert_eq!(repo.repo, "repo");
    }

    #[test]
    fn test_parse_https_url_no_git_suffix() {
        let repo = parse_remote_url("https://github.com/owner/repo").unwrap();
        assert_eq!(repo.host, "github.com");
        assert_eq!(repo.owner, "owner");
        assert_eq!(repo.repo, "repo");
    }

    #[test]
    fn test_parse_enterprise_url() {
        let repo = parse_remote_url("git@github.mycompany.com:team/project.git").unwrap();
        assert_eq!(repo.host, "github.mycompany.com");
        assert_eq!(repo.owner, "team");
        assert_eq!(repo.repo, "project");
    }
}
