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

use crate::{git2_ops::GitRepo, state::write_file_secure, stats::GitBenchmark};

// ============== Configuration Types ==============

/// GitHub authentication configuration
#[derive(Debug, Clone)]
pub struct GitHubConfig {
    pub token: String,
    pub api_base: String,
    /// GitHub usernames whose PRs should be displayed prominently in status.
    /// When non-empty, branches whose PR author isn't listed are hidden from
    /// `status`/`interactive`, except the current branch, its ancestor chain to trunk, and
    /// branches with no PR yet. `--show-all` bypasses this for one invocation.
    /// No longer used for filtering during sync.
    pub display_authors: Vec<String>,
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
    /// Timestamp when PR was last updated (ISO 8601 format)
    pub updated_at: String,
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

// ============== PR Cache Types ==============
//
// The cache storage itself (schema, per-repo scoped access) lives in `crate::pr_cache`. The
// types below are the cached PR shapes it stores; they stay here since they mirror the API
// response types (`PullRequest` et al.) above.

/// Full PR metadata for caching (mirrors PullRequest with Serialize)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CachedPullRequest {
    pub number: u64,
    pub state: PrState,
    pub title: String,
    pub html_url: String,
    pub base: CachedPrBranchRef,
    pub head: CachedPrBranchRef,
    pub user: CachedPrUser,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub merged: bool,
    #[serde(default)]
    pub merged_at: Option<String>,
    pub updated_at: String,
}

/// Cached branch reference (mirrors PrBranchRef with Serialize)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CachedPrBranchRef {
    pub ref_name: String,
    pub sha: String,
    pub repo: Option<CachedPrRepoRef>,
}

/// Cached repo reference
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CachedPrRepoRef {
    pub full_name: String,
}

/// Cached user reference
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CachedPrUser {
    pub login: String,
}

/// Result from list_prs operations, containing both filtered PRs and all author mappings
#[derive(Debug)]
pub struct PrListResult {
    /// PRs filtered to exclude forks
    pub prs: std::collections::HashMap<String, PullRequest>,
    /// All branch -> author mappings (before filtering)
    pub all_authors: std::collections::HashMap<String, String>,
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
    agent: ureq::Agent,
}

impl GitHubClient {
    pub fn new(config: GitHubConfig) -> Self {
        Self {
            config,
            agent: ureq::agent(), // default config: connection pooling + keep-alive
        }
    }

    /// Load config from environment/git config/config file
    pub fn from_env(repo_id: &RepoIdentifier) -> Result<Self, GitHubError> {
        let (token, display_authors) = find_github_config(&repo_id.host)?;
        let api_base = if repo_id.host == "github.com" {
            "https://api.github.com".to_string()
        } else {
            format!("https://{}/api/v3", repo_id.host)
        };
        Ok(Self::new(GitHubConfig {
            token,
            api_base,
            display_authors,
        }))
    }

    /// Get a reference to the client's config
    pub fn config(&self) -> &GitHubConfig {
        &self.config
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

        let _bench = GitBenchmark::start("github:get-pr");
        let mut response = self
            .agent
            .get(&url)
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

    /// Resolve the GitHub login GitHub associates with a commit (via a verified email on the
    /// committer's account), independent of any PR. Returns `Ok(None)` if GitHub has no author
    /// association for the commit (e.g. an unverified/unregistered email) — that's not an error,
    /// just an unresolvable author.
    pub fn get_commit_author(
        &self,
        repo: &RepoIdentifier,
        sha: &str,
    ) -> Result<Option<String>, GitHubError> {
        #[derive(Deserialize)]
        struct CommitResponse {
            author: Option<PrUser>,
        }

        let url = format!(
            "{}/repos/{}/{}/commits/{}",
            self.config.api_base, repo.owner, repo.repo, sha
        );

        let _bench = GitBenchmark::start("github:get-commit-author");
        let mut response = self
            .agent
            .get(&url)
            .header("Authorization", &format!("Bearer {}", self.config.token))
            .header("Accept", "application/vnd.github.v3+json")
            .header("User-Agent", "git-stack")
            .call()
            .map_err(|e| self.handle_ureq_error(e))?;

        let parsed: CommitResponse = response
            .body_mut()
            .read_json()
            .map_err(|e| GitHubError::Network(e.to_string()))?;

        Ok(parsed.author.map(|u| u.login))
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

        let _bench = GitBenchmark::start("github:find-pr");
        let mut response = self
            .agent
            .get(&url)
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

        let _bench = GitBenchmark::start("github:create-pr");
        let mut response = self
            .agent
            .post(&url)
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
    /// Returns a PrListResult containing filtered PRs and all author mappings
    ///
    /// The optional `on_progress` callback is called after each page fetch with
    /// (page_number, cumulative_count) to enable progress reporting.
    pub fn list_prs(
        &self,
        repo: &RepoIdentifier,
        state: &str, // "open", "closed", or "all"
        on_progress: Option<&dyn Fn(usize, usize)>,
    ) -> Result<PrListResult, GitHubError> {
        let mut all_prs = Vec::new();
        let mut page = 1;
        let per_page = 100;

        loop {
            let url = format!(
                "{}/repos/{}/{}/pulls?state={}&per_page={}&page={}",
                self.config.api_base, repo.owner, repo.repo, state, per_page, page
            );

            let _bench = GitBenchmark::start("github:list-prs");
            let mut response = self
                .agent
                .get(&url)
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

            // Report progress if callback provided
            if let Some(callback) = on_progress {
                callback(page, all_prs.len());
            }

            // If we got fewer than per_page results, we've reached the end
            if count < per_page {
                break;
            }
            page += 1;
        }

        // Collect all authors before filtering (for pruning decisions)
        let all_authors: std::collections::HashMap<String, String> = all_prs
            .iter()
            .map(|pr| (pr.head.ref_name.clone(), pr.user.login.clone()))
            .collect();

        // Build map of head branch name -> PR, filtering out PRs from forks
        let prs: std::collections::HashMap<String, PullRequest> = all_prs
            .into_iter()
            .filter(|pr| {
                // Filter out PRs from forks (we can't track remote branches for forks)
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

        Ok(PrListResult { prs, all_authors })
    }

    /// List all open PRs for a repository (convenience wrapper)
    pub fn list_open_prs(
        &self,
        repo: &RepoIdentifier,
        on_progress: Option<&dyn Fn(usize, usize)>,
    ) -> Result<PrListResult, GitHubError> {
        self.list_prs(repo, "open", on_progress)
    }

    /// List closed PRs with caching support.
    ///
    /// Uses a watermark timestamp strategy:
    /// 1. Loads cached closed PRs for this repo from the cache handle
    /// 2. Fetches PRs from API sorted by `updated_at` descending
    /// 3. Stops fetching when encountering a PR older than the watermark
    /// 4. Merges fresh data with cache (fresh data wins for any branch name)
    /// 5. Persists the merged data and an updated watermark (best-effort; a persistence
    ///    failure only costs the *next* call's warm cache, not this call's result)
    pub fn list_closed_prs_with_cache(
        &self,
        repo: &RepoIdentifier,
        cache: &crate::pr_cache::PrCacheHandle,
        on_progress: Option<&dyn Fn(usize, usize)>,
    ) -> Result<PrListResult, GitHubError> {
        let repo_key = repo.full_name();

        let mut closed_prs = cache.closed_prs_for_repo(&repo_key).unwrap_or_else(|e| {
            tracing::warn!("Failed to read PR cache for {}: {}", repo_key, e);
            std::collections::HashMap::new()
        });
        let watermark = cache.watermark(&repo_key).unwrap_or_else(|e| {
            tracing::warn!("Failed to read PR cache watermark for {}: {}", repo_key, e);
            None
        });
        tracing::debug!(
            "PR cache for {}: {} cached closed PRs, watermark={:?}",
            repo_key,
            closed_prs.len(),
            watermark
        );

        // Fetch PRs with early termination based on watermark
        let fresh_prs =
            self.list_prs_until_watermark(repo, "closed", watermark.as_deref(), on_progress)?;
        tracing::debug!(
            "Fetched {} fresh closed PRs for {} (a small number means the watermark cache hit; \
             a number near the repo's total closed-PR count means a full backfill happened)",
            fresh_prs.len(),
            repo_key
        );

        // Track the newest updated_at for new watermark
        let mut newest_updated_at: Option<String> = None;
        let mut fresh_cached: std::collections::HashMap<String, CachedPullRequest> =
            std::collections::HashMap::new();

        for (branch_name, pr) in &fresh_prs {
            if newest_updated_at
                .as_ref()
                .is_none_or(|ts| pr.updated_at > *ts)
            {
                newest_updated_at = Some(pr.updated_at.clone());
            }

            let cached_pr = CachedPullRequest::from(pr);
            closed_prs.insert(branch_name.clone(), cached_pr.clone());
            fresh_cached.insert(branch_name.clone(), cached_pr);
        }

        let new_watermark = match (&watermark, &newest_updated_at) {
            (None, Some(ts)) => Some(ts.clone()),
            (Some(current), Some(ts)) if ts > current => Some(ts.clone()),
            _ => None,
        };

        tracing::debug!(
            "Updated PR cache watermark for {}: {:?} -> {:?}",
            repo_key,
            watermark,
            new_watermark
        );
        if let Err(e) = cache.commit_fresh_prs(
            &repo_key,
            fresh_cached.iter().map(|(k, v)| (k.as_str(), v)),
            new_watermark.as_deref(),
        ) {
            tracing::warn!("Failed to persist PR cache for {}: {}", repo_key, e);
        }

        // Collect all authors from cache before filtering (for pruning decisions)
        let all_authors: std::collections::HashMap<String, String> = closed_prs
            .iter()
            .map(|(branch, cached_pr)| (branch.clone(), cached_pr.user.login.clone()))
            .collect();

        // Convert cache to return type, applying filters
        let prs: std::collections::HashMap<String, PullRequest> = closed_prs
            .iter()
            .map(|(k, v)| (k.clone(), PullRequest::from(v)))
            .filter(|(_, pr)| self.should_include_pr(pr))
            .collect();

        Ok(PrListResult { prs, all_authors })
    }

    /// Check if a PR should be included based on fork filtering
    fn should_include_pr(&self, pr: &PullRequest) -> bool {
        // Filter out PRs from forks (we can't track remote branches for forks)
        !pr.is_from_fork()
    }

    /// Fetch PRs with early termination when hitting the watermark
    fn list_prs_until_watermark(
        &self,
        repo: &RepoIdentifier,
        state: &str,
        watermark: Option<&str>,
        on_progress: Option<&dyn Fn(usize, usize)>,
    ) -> Result<std::collections::HashMap<String, PullRequest>, GitHubError> {
        let mut all_prs = Vec::new();
        let mut page = 1;
        let per_page = 100;
        let mut hit_watermark = false;

        loop {
            // Use sort=updated and direction=desc for watermark strategy
            let url = format!(
                "{}/repos/{}/{}/pulls?state={}&sort=updated&direction=desc&per_page={}&page={}",
                self.config.api_base, repo.owner, repo.repo, state, per_page, page
            );

            let _bench = GitBenchmark::start("github:list-closed-prs");
            let mut response = self
                .agent
                .get(&url)
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

            // Check each PR against watermark
            for pr in prs {
                // If we have a watermark and this PR's updated_at is older or equal, we can stop
                // after this page (still include PRs on this page to handle edge cases)
                if let Some(wm) = watermark
                    && pr.updated_at.as_str() <= wm
                {
                    hit_watermark = true;
                }
                all_prs.push(pr);
            }

            // Report progress if callback provided
            if let Some(callback) = on_progress {
                callback(page, all_prs.len());
            }

            // Stop if we hit the watermark or reached the end
            if hit_watermark || count < per_page {
                break;
            }
            page += 1;
        }

        // Build map of head branch name -> PR, filtering out irrelevant PRs
        let pr_map: std::collections::HashMap<String, PullRequest> = all_prs
            .into_iter()
            .filter(|pr| self.should_include_pr(pr))
            .map(|pr| (pr.head.ref_name.clone(), pr))
            .collect();

        Ok(pr_map)
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

        let _bench = GitBenchmark::start("github:update-pr");
        let mut response = self
            .agent
            .patch(&url)
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

/// Load display_authors from the GitHub config file.
/// Returns an empty vec if the config file doesn't exist or has no display_authors.
pub fn load_display_authors() -> Vec<String> {
    load_github_config_file()
        .map(|c| c.display_authors)
        .unwrap_or_default()
}

/// Where the active GitHub token was resolved from. PATs win over OAuth.
#[derive(Debug, PartialEq, Eq)]
pub enum AuthSource {
    EnvGithubToken,
    EnvGhToken,
    GitConfig,
    ConfigHostToken,
    ConfigDefaultToken, // classic/fine-grained PAT
    ConfigOauth { scope: Option<String> },
    GhCli,
}

/// Ask the `gh` CLI for a token for `host`. Returns None if `gh` is absent,
/// not logged in for the host, or prints nothing. A single `gh auth token`
/// invocation both detects availability and yields the token.
fn gh_auth_token(host: &str) -> Option<String> {
    let output = Command::new("gh")
        .args(["auth", "token", "--hostname", host])
        .output()
        .ok()?; // command-not-found -> None
    if !output.status.success() {
        return None; // not logged in for host -> None
    }
    let token = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if token.is_empty() { None } else { Some(token) }
}

/// Resolve the active token and where it came from (PAT-wins order).
fn resolve_github_auth(host: &str) -> Option<(String, AuthSource)> {
    let env_github_token = std::env::var("GITHUB_TOKEN").ok();
    let env_gh_token = std::env::var("GH_TOKEN").ok();

    // git config github.token
    let git_config_token = Command::new("git")
        .args(["config", "--get", "github.token"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string());

    let config_file = load_github_config_file();

    resolve_auth_core(
        host,
        env_github_token,
        env_gh_token,
        git_config_token,
        config_file,
        gh_auth_token,
    )
}

/// Pure resolution core — all sources injected so it is deterministic and testable.
/// Empty-string tokens are treated as absent, matching the live source guards.
fn resolve_auth_core(
    host: &str,
    env_github_token: Option<String>,
    env_gh_token: Option<String>,
    git_config_token: Option<String>,
    config_file: Option<GitHubConfigFile>,
    gh_token: impl FnOnce(&str) -> Option<String>,
) -> Option<(String, AuthSource)> {
    // 1. Check GITHUB_TOKEN env var
    if let Some(token) = env_github_token
        && !token.is_empty()
    {
        tracing::debug!("Using GitHub token from GITHUB_TOKEN env var");
        return Some((token, AuthSource::EnvGithubToken));
    }

    // 2. Check GH_TOKEN env var (used by gh CLI)
    if let Some(token) = env_gh_token
        && !token.is_empty()
    {
        tracing::debug!("Using GitHub token from GH_TOKEN env var");
        return Some((token, AuthSource::EnvGhToken));
    }

    // 3. Check git config github.token
    if let Some(token) = git_config_token
        && !token.is_empty()
    {
        tracing::debug!("Using GitHub token from git config");
        return Some((token, AuthSource::GitConfig));
    }

    // 4-6. Check XDG config file for tokens
    if let Some(config) = config_file {
        // 4. Host-specific token first
        if let Some(hosts) = &config.hosts
            && let Some(token) = hosts.get(host)
        {
            tracing::debug!("Using GitHub token from config file (host-specific)");
            return Some((token.clone(), AuthSource::ConfigHostToken));
        }
        // 5. Default token (PAT) wins over OAuth
        if let Some(token) = config.default_token {
            tracing::debug!("Using GitHub token from config file (default)");
            return Some((token, AuthSource::ConfigDefaultToken));
        }
        // 6. OAuth device-flow token
        if let Some(token) = config.oauth_token {
            tracing::debug!("Using GitHub token from config file (oauth)");
            return Some((
                token,
                AuthSource::ConfigOauth {
                    scope: config.oauth_scope,
                },
            ));
        }
    }

    // 7. Last resort: borrow the gh CLI's token.
    if let Some(token) = gh_token(host) {
        let token = token.trim().to_string();
        if !token.is_empty() {
            tracing::debug!("Using GitHub token from gh CLI");
            return Some((token, AuthSource::GhCli));
        }
    }

    None
}

/// Report the active auth method for `auth status` (no token leaked).
pub fn find_auth_source(host: &str) -> Option<AuthSource> {
    resolve_github_auth(host).map(|(_, src)| src)
}

/// Find GitHub token and config from various sources
fn find_github_config(host: &str) -> Result<(String, Vec<String>), GitHubError> {
    let display_authors = load_display_authors();
    match resolve_github_auth(host) {
        Some((token, _)) => Ok((token, display_authors)),
        None => Err(GitHubError::NoToken),
    }
}

/// GitHub config file structure
#[derive(Debug, Default, Deserialize, Serialize)]
struct GitHubConfigFile {
    default_token: Option<String>,
    hosts: Option<std::collections::HashMap<String, String>>,
    /// GitHub usernames whose PRs should be displayed prominently in status.
    /// When non-empty, branches whose PR author isn't listed are hidden from
    /// `status`/`interactive`, except the current branch, its ancestor chain to trunk, and
    /// branches with no PR yet. `--show-all` bypasses this for one invocation.
    #[serde(default)]
    display_authors: Vec<String>,
    /// OAuth device-flow token (distinct from `default_token`, which holds a PAT).
    #[serde(skip_serializing_if = "Option::is_none")]
    oauth_token: Option<String>,
    /// Scope granted to the OAuth token.
    #[serde(skip_serializing_if = "Option::is_none")]
    oauth_scope: Option<String>,
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

    // Load existing config to preserve other settings (like display_authors)
    let mut config = load_github_config_file().unwrap_or_default();
    config.default_token = Some(token.to_string());

    let contents = serde_yaml::to_string(&config)?;
    write_file_secure(&config_path, &contents)?;

    println!("Token saved to {}", config_path.display());
    Ok(())
}

/// Save an OAuth device-flow token to the config file.
///
/// Writes only the OAuth fields; `default_token` (a PAT) is never touched.
pub fn save_github_oauth_token(token: &str, scope: &str) -> Result<()> {
    let base_dirs = xdg::BaseDirectories::with_prefix("git-stack");
    let config_path = base_dirs
        .place_config_file("github.yaml")
        .context("Failed to create config directory")?;

    // Load existing config to preserve other settings (PAT, display_authors).
    let mut config = load_github_config_file().unwrap_or_default();
    config.oauth_token = Some(token.to_string());
    config.oauth_scope = Some(scope.to_string());

    let contents = serde_yaml::to_string(&config)?;
    write_file_secure(&config_path, &contents)?;

    println!("OAuth token saved to {}", config_path.display());
    Ok(())
}

/// Clear stored tokens from the config file.
///
/// Clears OAuth and/or PAT (`default_token`) per the flags; when neither flag
/// is set, both are cleared. `display_authors` and other settings are preserved.
pub fn clear_github_tokens(oauth: bool, pat: bool) -> Result<()> {
    let config_path = get_github_config_path()?;
    let Some(mut config) = load_github_config_file() else {
        println!("No stored GitHub token found.");
        return Ok(());
    };

    // When no selector is given, clear both.
    let (clear_oauth, clear_pat) = if !oauth && !pat {
        (true, true)
    } else {
        (oauth, pat)
    };

    if clear_oauth {
        config.oauth_token = None;
        config.oauth_scope = None;
    }
    if clear_pat {
        config.default_token = None;
    }

    let contents = serde_yaml::to_string(&config)?;
    write_file_secure(&config_path, &contents)?;
    println!("Cleared stored token(s) in {}", config_path.display());
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
    println!("Tip: `git stack auth login` offers browser-based OAuth login (recommended),");
    println!("which works even where classic personal access tokens are disallowed.");
    println!();
    println!("Steps to create a token:");
    println!("1. Go to: https://github.com/settings/tokens/new");
    println!("2. Name: \"git-stack CLI\"");
    println!("3. Scopes needed: repo (full control of private repos)");
    println!("4. Click \"Generate token\" and copy the value");
    println!();
    println!("Alternatively, set GITHUB_TOKEN or GH_TOKEN in your environment,");
    println!("or manually create the config file with:");
    if let Ok(config_path) = get_github_config_path() {
        println!("  {}", config_path.display());
    }
    println!("Contents:");
    println!("  default_token: ghp_yourtoken");
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

// ============== OAuth Device Flow ==============

const GITHUB_OAUTH_CLIENT_ID: &str = "Ov23liPTCxzZTCwOphVj";
const DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const DEVICE_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const OAUTH_SCOPE: &str = "repo";

#[derive(Debug, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    #[serde(default)]
    #[allow(dead_code)]
    expires_in: u64, // default 900
    interval: u64, // poll seconds
}

/// Raw access-token poll response: carries either a token or an `error`.
#[derive(Debug, Deserialize)]
struct TokenPollResponse {
    access_token: Option<String>,
    #[allow(dead_code)]
    token_type: Option<String>,
    scope: Option<String>,
    error: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum PollOutcome {
    Pending,  // authorization_pending -> keep polling
    SlowDown, // slow_down -> +5s interval, keep polling
    Success { token: String, scope: String },
    Expired,        // expired_token -> abort, re-run login
    Denied,         // access_denied -> abort
    Failed(String), // any other error string
}

fn classify_poll_response(resp: TokenPollResponse) -> PollOutcome {
    if let Some(token) = resp.access_token {
        return PollOutcome::Success {
            token,
            scope: resp.scope.unwrap_or_default(),
        };
    }
    match resp.error.as_deref() {
        Some("authorization_pending") => PollOutcome::Pending,
        Some("slow_down") => PollOutcome::SlowDown,
        Some("expired_token") => PollOutcome::Expired,
        Some("access_denied") => PollOutcome::Denied,
        Some(other) => PollOutcome::Failed(other.to_string()),
        None => PollOutcome::Failed("empty response from GitHub".to_string()),
    }
}

fn request_device_code() -> Result<DeviceCodeResponse> {
    let mut resp = ureq::post(DEVICE_CODE_URL)
        .header("Accept", "application/json")
        .header("User-Agent", "git-stack")
        .send_form([
            ("client_id", GITHUB_OAUTH_CLIENT_ID),
            ("scope", OAUTH_SCOPE),
        ])
        .context("requesting device code from GitHub")?;
    resp.body_mut()
        .read_json()
        .context("parsing device-code response")
}

fn poll_for_token(device_code: &str, mut interval: u64) -> Result<(String, String)> {
    loop {
        std::thread::sleep(std::time::Duration::from_secs(interval));
        let mut resp = ureq::post(DEVICE_TOKEN_URL)
            .header("Accept", "application/json")
            .header("User-Agent", "git-stack")
            .send_form([
                ("client_id", GITHUB_OAUTH_CLIENT_ID),
                ("device_code", device_code),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .context("polling GitHub for access token")?;
        let parsed: TokenPollResponse = resp
            .body_mut()
            .read_json()
            .context("parsing token poll response")?;
        match classify_poll_response(parsed) {
            PollOutcome::Pending => continue,
            PollOutcome::SlowDown => {
                interval += 5;
                continue;
            }
            PollOutcome::Success { token, scope } => return Ok((token, scope)),
            PollOutcome::Expired => {
                bail!("Device code expired. Run `git stack auth login` again.")
            }
            PollOutcome::Denied => bail!("Authorization was denied."),
            PollOutcome::Failed(e) => bail!("GitHub device-flow error: {e}"),
        }
    }
}

/// Run the full device flow; returns (token, scope) on success.
pub fn login_via_device_flow() -> Result<(String, String)> {
    let dc = request_device_code()?;
    println!(
        "\nTo authorize git-stack, visit:\n  {}",
        dc.verification_uri
    );
    println!("and enter the code: {}\n", dc.user_code); // user_code shown prominently
    let _ = open_in_browser(&dc.verification_uri); // best-effort convenience
    println!("Waiting for authorization in your browser...");
    poll_for_token(&dc.device_code, dc.interval)
}

/// Interactive login menu: choose browser OAuth or paste a token.
///
/// Returns the active token on success.
pub fn login_interactive() -> Result<String> {
    println!("How would you like to authenticate with GitHub?");
    println!("  [1] Browser login (recommended)");
    println!("  [2] Paste a token");
    print!("Enter choice [1]: ");
    io::stdout().flush()?;

    let mut choice = String::new();
    io::stdin().read_line(&mut choice)?;
    let choice = choice.trim();

    match choice {
        "" | "1" => {
            let (token, scope) = login_via_device_flow()?;
            save_github_oauth_token(&token, &scope)?;
            Ok(token)
        }
        "2" => setup_github_token_interactive(),
        other => bail!("Invalid choice: {other}"),
    }
}

// ============== Cache Conversion Traits ==============

impl From<&PullRequest> for CachedPullRequest {
    fn from(pr: &PullRequest) -> Self {
        Self {
            number: pr.number,
            state: pr.state,
            title: pr.title.clone(),
            html_url: pr.html_url.clone(),
            base: CachedPrBranchRef {
                ref_name: pr.base.ref_name.clone(),
                sha: pr.base.sha.clone(),
                repo: pr.base.repo.as_ref().map(|r| CachedPrRepoRef {
                    full_name: r.full_name.clone(),
                }),
            },
            head: CachedPrBranchRef {
                ref_name: pr.head.ref_name.clone(),
                sha: pr.head.sha.clone(),
                repo: pr.head.repo.as_ref().map(|r| CachedPrRepoRef {
                    full_name: r.full_name.clone(),
                }),
            },
            user: CachedPrUser {
                login: pr.user.login.clone(),
            },
            draft: pr.draft,
            merged: pr.merged,
            merged_at: pr.merged_at.clone(),
            updated_at: pr.updated_at.clone(),
        }
    }
}

impl From<&CachedPullRequest> for PullRequest {
    fn from(cached: &CachedPullRequest) -> Self {
        Self {
            number: cached.number,
            state: cached.state,
            title: cached.title.clone(),
            html_url: cached.html_url.clone(),
            base: PrBranchRef {
                ref_name: cached.base.ref_name.clone(),
                sha: cached.base.sha.clone(),
                repo: cached.base.repo.as_ref().map(|r| PrRepoRef {
                    full_name: r.full_name.clone(),
                }),
            },
            head: PrBranchRef {
                ref_name: cached.head.ref_name.clone(),
                sha: cached.head.sha.clone(),
                repo: cached.head.repo.as_ref().map(|r| PrRepoRef {
                    full_name: r.full_name.clone(),
                }),
            },
            user: PrUser {
                login: cached.user.login.clone(),
            },
            draft: cached.draft,
            merged: cached.merged,
            merged_at: cached.merged_at.clone(),
            updated_at: cached.updated_at.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::cell::Cell;

    #[test]
    fn gh_fallback_reached_when_all_empty() {
        let result = resolve_auth_core("github.com", None, None, None, None, |_| {
            Some("ghtok".to_string())
        });
        assert_eq!(result, Some(("ghtok".to_string(), AuthSource::GhCli)));
    }

    #[test]
    fn github_token_env_wins_over_gh() {
        let gh_called = Cell::new(false);
        let result = resolve_auth_core(
            "github.com",
            Some("envtok".to_string()),
            None,
            None,
            None,
            |_| {
                gh_called.set(true);
                Some("ghtok".to_string())
            },
        );
        assert_eq!(
            result,
            Some(("envtok".to_string(), AuthSource::EnvGithubToken))
        );
        assert!(
            !gh_called.get(),
            "gh CLI must not be consulted when an earlier source resolves"
        );
    }

    #[test]
    fn no_token_when_gh_also_empty() {
        let result = resolve_auth_core("github.com", None, None, None, None, |_| None);
        assert_eq!(result, None);
    }

    #[test]
    fn gh_empty_string_treated_as_absent() {
        let result = resolve_auth_core("github.com", None, None, None, None, |_| {
            Some("  ".to_string())
        });
        assert_eq!(result, None);
    }

    #[test]
    fn config_default_wins_over_gh() {
        let gh_called = Cell::new(false);
        let config = GitHubConfigFile {
            default_token: Some("cfgtok".to_string()),
            ..Default::default()
        };
        let result = resolve_auth_core("github.com", None, None, None, Some(config), |_| {
            gh_called.set(true);
            Some("ghtok".to_string())
        });
        assert_eq!(
            result,
            Some(("cfgtok".to_string(), AuthSource::ConfigDefaultToken))
        );
        assert!(
            !gh_called.get(),
            "gh CLI must not be consulted when config resolves"
        );
    }

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

    #[test]
    fn test_device_code_response_parses() {
        let json = r#"{
            "device_code": "3584d83530557fdd1f46af8289938c8ef79f9dc5",
            "user_code": "WDJB-MJHT",
            "verification_uri": "https://github.com/login/device",
            "expires_in": 900,
            "interval": 5
        }"#;
        let dc: DeviceCodeResponse = serde_json::from_str(json).unwrap();
        assert_eq!(dc.device_code, "3584d83530557fdd1f46af8289938c8ef79f9dc5");
        assert_eq!(dc.user_code, "WDJB-MJHT");
        assert_eq!(dc.verification_uri, "https://github.com/login/device");
        assert_eq!(dc.expires_in, 900);
        assert_eq!(dc.interval, 5);
    }

    fn parse_poll(json: &str) -> PollOutcome {
        let resp: TokenPollResponse = serde_json::from_str(json).unwrap();
        classify_poll_response(resp)
    }

    #[test]
    fn test_classify_authorization_pending() {
        assert_eq!(
            parse_poll(r#"{"error":"authorization_pending"}"#),
            PollOutcome::Pending
        );
    }

    #[test]
    fn test_classify_slow_down() {
        assert_eq!(
            parse_poll(r#"{"error":"slow_down"}"#),
            PollOutcome::SlowDown
        );
    }

    #[test]
    fn test_classify_expired_token() {
        assert_eq!(
            parse_poll(r#"{"error":"expired_token"}"#),
            PollOutcome::Expired
        );
    }

    #[test]
    fn test_classify_access_denied() {
        assert_eq!(
            parse_poll(r#"{"error":"access_denied"}"#),
            PollOutcome::Denied
        );
    }

    #[test]
    fn test_classify_unknown_error() {
        assert_eq!(
            parse_poll(r#"{"error":"unmapped_thing"}"#),
            PollOutcome::Failed("unmapped_thing".to_string())
        );
    }

    #[test]
    fn test_classify_success() {
        assert_eq!(
            parse_poll(r#"{"access_token":"gho_abc123","token_type":"bearer","scope":"repo"}"#),
            PollOutcome::Success {
                token: "gho_abc123".to_string(),
                scope: "repo".to_string(),
            }
        );
    }
}
