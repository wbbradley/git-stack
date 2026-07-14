//! GitHub API client for git-stack PR integration.
//!
//! This module provides direct GitHub REST API access without
//! depending on the `gh` CLI tool.

use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
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

/// Per-branch outcome of a stack-scoped open-PR fetch, so the caller can distinguish "this
/// branch has no open PR" (drop it from the cache) from "the query for this branch failed"
/// (keep its cached entry as last-known-good).
#[derive(Debug, Default)]
pub struct ScopedOpenPrs {
    /// branch -> open PR (`Ok(Some)`)
    pub found: std::collections::HashMap<String, PullRequest>,
    /// branches that definitively have no open PR (`Ok(None)`)
    pub confirmed_absent: Vec<String>,
    // Errored branches are omitted from both — the caller falls back to cache for them.
}

// ============== GraphQL Types ==============
//
// GraphQL responds with HTTP 200 even for query-level failures, carrying them in a top-level
// `errors` array alongside an optional `data`. These wrappers let the transport surface those as
// `GitHubError`s. The `Search*` types below model exactly the `search` connection shape that
// `list_open_prs_by_authors` requests (rename-cased; `Option` fields tolerate empty non-PR nodes).

#[derive(Debug, Deserialize)]
struct GraphQlResponse<T> {
    data: Option<T>,
    #[serde(default)]
    errors: Vec<GraphQlError>,
}

#[derive(Debug, Deserialize)]
struct GraphQlError {
    message: String,
}

#[derive(Debug, Deserialize)]
struct SearchData {
    search: SearchConnection,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchConnection {
    page_info: PageInfo,
    #[serde(default)]
    nodes: Vec<SearchNode>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageInfo {
    has_next_page: bool,
    end_cursor: Option<String>,
}

/// One `search` node. All fields are `Option` so an empty `{}` node (a non-`PullRequest` result
/// the `... on PullRequest` fragment doesn't populate) deserializes cleanly and is skipped.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchNode {
    number: Option<u64>,
    title: Option<String>,
    url: Option<String>,
    is_draft: Option<bool>,
    is_cross_repository: Option<bool>,
    updated_at: Option<String>,
    base_ref_name: Option<String>,
    head_ref_name: Option<String>,
    head_ref_oid: Option<String>,
    head_repository: Option<SearchRepo>,
    base_repository: Option<SearchRepo>,
    author: Option<SearchAuthor>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchRepo {
    name_with_owner: String,
}

#[derive(Debug, Deserialize)]
struct SearchAuthor {
    login: String,
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
    /// Org forbids classic PATs; carries the org name when parseable from the 403 body.
    ClassicPatForbidden { org: Option<String> },
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
            Self::ClassicPatForbidden { org } => {
                let org_label = org.as_deref().unwrap_or("this organization");
                write!(
                    f,
                    "The GitHub organization '{org_label}' does not allow classic personal access tokens.\n\
                     git-stack authenticated fine, but the org blocks classic-PAT access to its repos.\n\
                     \n\
                     Use one of these org-accepted options:\n\
                     \n\
                     1. Create a fine-grained personal access token:\n\
                     \x20  - Visit https://github.com/settings/personal-access-tokens/new\n\
                     \x20  - Set the resource owner to '{org_label}'\n\
                     \x20  - Grant repository permissions: Pull requests (Read and write),\n\
                     \x20    Contents (Read and write), Metadata (Read-only)\n\
                     \x20  - An organization admin may need to approve the token before it works\n\
                     \x20  - Save it as `default_token` in ~/.config/git-stack/github.yaml,\n\
                     \x20    or export it as GITHUB_TOKEN / GH_TOKEN\n\
                     \n\
                     2. Or authenticate with browser OAuth: run `git stack auth login`\n\
                     \x20  (an org admin may still need to approve git-stack's OAuth app)."
                )
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
        let agent = ureq::Agent::config_builder()
            // Return non-2xx as Ok(response) so we can read GitHub's explanatory body
            // (e.g. the classic-PAT-forbidden 403 message) instead of a body-less StatusCode error.
            .http_status_as_error(false)
            .build()
            .new_agent();
        Self { config, agent }
    }

    /// Load config from environment/git config/config file
    pub fn from_env(repo_id: &RepoIdentifier) -> Result<Self, GitHubError> {
        let token = find_github_config(&repo_id.host)?;
        let api_base = if repo_id.host == "github.com" {
            "https://api.github.com".to_string()
        } else {
            format!("https://{}/api/v3", repo_id.host)
        };
        Ok(Self::new(GitHubConfig { token, api_base }))
    }

    /// Get a reference to the client's config
    pub fn config(&self) -> &GitHubConfig {
        &self.config
    }

    /// Apply the auth/Accept/User-Agent headers common to every GitHub REST call.
    fn auth_headers<B>(&self, rb: ureq::RequestBuilder<B>) -> ureq::RequestBuilder<B> {
        rb.header("Authorization", &format!("Bearer {}", self.config.token))
            .header("Accept", "application/vnd.github.v3+json")
            .header("User-Agent", "git-stack")
    }

    /// Issue a GET and deserialize the JSON response (classifying non-2xx into a `GitHubError`).
    fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        bench: &'static str,
    ) -> Result<T, GitHubError> {
        let _bench = GitBenchmark::start(bench);
        let response = self
            .auth_headers(self.agent.get(url))
            .call()
            .map_err(transport_error)?;
        read_checked(response)
    }

    /// Issue a POST with a JSON body and deserialize the JSON response.
    fn post_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        body: &impl Serialize,
        bench: &'static str,
    ) -> Result<T, GitHubError> {
        let _bench = GitBenchmark::start(bench);
        let response = self
            .auth_headers(self.agent.post(url))
            .send_json(body)
            .map_err(transport_error)?;
        read_checked(response)
    }

    /// Issue a PATCH with a JSON body and deserialize the JSON response.
    fn patch_json<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        body: &impl Serialize,
        bench: &'static str,
    ) -> Result<T, GitHubError> {
        let _bench = GitBenchmark::start(bench);
        let response = self
            .auth_headers(self.agent.patch(url))
            .send_json(body)
            .map_err(transport_error)?;
        read_checked(response)
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

        self.get_json(&url, "github:get-pr")
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

        let parsed: CommitResponse = self.get_json(&url, "github:get-commit-author")?;
        Ok(parsed.author.map(|u| u.login))
    }

    /// Resolve the login of the authenticated user via `GET {api_base}/user`. Used to derive the
    /// default author filter (`[<your login>]`) when it's left unconfigured.
    pub fn whoami(&self) -> Result<String, GitHubError> {
        let url = format!("{}/user", self.config.api_base);

        Ok(self.get_json::<PrUser>(&url, "github:whoami")?.login)
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

        let prs: Vec<PullRequest> = self.get_json(&url, "github:find-pr")?;
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

        self.post_json(&url, &request, "github:create-pr")
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

            let prs: Vec<PullRequest> = self.get_json(&url, "github:list-prs")?;

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

    /// Fetch open PRs for exactly `branches` (the stack's branches) with bounded parallelism,
    /// scaling with stack size rather than total repo PR activity. Each branch is looked up with
    /// the single-branch `find_pr_for_branch` query (head=`owner:branch`, non-fork by
    /// construction). Best-effort: never returns `Result` — a per-branch error omits that branch
    /// from both outcome lists so the caller keeps its cached (last-known-good) entry.
    pub fn list_open_prs_for_branches(
        &self,
        repo: &RepoIdentifier,
        branches: &[String],
    ) -> ScopedOpenPrs {
        if branches.is_empty() {
            return ScopedOpenPrs::default();
        }

        // branches is non-empty (early return above), so this is always >= 1.
        let worker_count = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(branches.len())
            .min(8);

        // Partition branches round-robin across workers.
        let mut buckets: Vec<Vec<&String>> = (0..worker_count).map(|_| Vec::new()).collect();
        for (i, branch) in branches.iter().enumerate() {
            buckets[i % worker_count].push(branch);
        }

        let mut result = ScopedOpenPrs::default();
        std::thread::scope(|scope| {
            let handles: Vec<_> = buckets
                .into_iter()
                .map(|bucket| {
                    scope.spawn(move || {
                        let mut found: Vec<(String, PullRequest)> = Vec::new();
                        let mut absent: Vec<String> = Vec::new();
                        for branch in bucket {
                            match self.find_pr_for_branch(repo, branch) {
                                Ok(Some(pr)) => found.push((branch.clone(), pr)),
                                Ok(None) => absent.push(branch.clone()),
                                Err(e) => {
                                    tracing::debug!(
                                        "Scoped open-PR fetch failed for branch {}: {}",
                                        branch,
                                        e
                                    );
                                }
                            }
                        }
                        // `GitBenchmark` records into thread-local stats, so hand this worker's
                        // `github:find-pr` spans back for merging into the caller's thread.
                        (found, absent, crate::stats::get_stats())
                    })
                })
                .collect();

            for handle in handles {
                if let Ok((found, absent, stats)) = handle.join() {
                    for (branch, pr) in found {
                        result.found.insert(branch, pr);
                    }
                    result.confirmed_absent.extend(absent);
                    crate::stats::merge_into_current(&stats);
                }
            }
        });

        result
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

            let prs: Vec<PullRequest> = self.get_json(&url, "github:list-closed-prs")?;

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

        self.patch_json(&url, &request, "github:update-pr")
    }

    /// The GraphQL endpoint for this host. github.com's REST base is `https://api.github.com`
    /// (GraphQL at `…/graphql`); GHE's REST base is `https://{host}/api/v3` (GraphQL at
    /// `https://{host}/api/graphql`).
    fn graphql_url(&self) -> String {
        match self.config.api_base.strip_suffix("/api/v3") {
            Some(base) => format!("{base}/api/graphql"),
            None => format!("{}/graphql", self.config.api_base),
        }
    }

    /// POST a GraphQL `{query, variables}` and deserialize `data` into `T`. GraphQL returns HTTP
    /// 200 even for query-level failures, carrying them in a top-level `errors` array, so this
    /// maps a non-empty `errors` (or a missing `data`) to a `GitHubError::Api { status: 200 }`.
    fn graphql<T: serde::de::DeserializeOwned>(
        &self,
        query: &str,
        variables: serde_json::Value,
    ) -> Result<T, GitHubError> {
        let _bench = GitBenchmark::start("github:graphql");
        let url = self.graphql_url();
        let body = serde_json::json!({ "query": query, "variables": variables });
        let response = self
            .auth_headers(self.agent.post(&url))
            .send_json(&body)
            .map_err(transport_error)?;
        let parsed: GraphQlResponse<T> = read_checked(response)?;
        if !parsed.errors.is_empty() {
            let message = parsed
                .errors
                .iter()
                .map(|e| e.message.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(GitHubError::Api {
                status: 200,
                message,
            });
        }
        parsed.data.ok_or_else(|| GitHubError::Api {
            status: 200,
            message: "GraphQL response contained no data".to_string(),
        })
    }

    /// Enumerate the open PRs authored by any of `authors` in `repo`, via a single paginated
    /// GraphQL search. Returns PRs *with* their base/head refs and author, so the caller needs no
    /// per-branch REST hydration. Fork PRs (`isCrossRepository`) are dropped — their head branch
    /// isn't on `origin` and can't be mounted. Empty `authors` short-circuits to `Ok(vec![])`
    /// (no HTTP), since there's no cheap way to enumerate "everyone".
    pub fn list_open_prs_by_authors(
        &self,
        repo: &RepoIdentifier,
        authors: &[String],
    ) -> Result<Vec<PullRequest>, GitHubError> {
        if authors.is_empty() {
            return Ok(Vec::new());
        }

        const QUERY: &str = r"
            query($q: String!, $cursor: String) {
              search(query: $q, type: ISSUE, first: 100, after: $cursor) {
                pageInfo { hasNextPage endCursor }
                nodes {
                  ... on PullRequest {
                    number
                    title
                    url
                    isDraft
                    isCrossRepository
                    updatedAt
                    baseRefName
                    headRefName
                    headRefOid
                    headRepository { nameWithOwner }
                    baseRepository { nameWithOwner }
                    author { login }
                  }
                }
              }
            }
        ";

        let search_query = build_author_search_query(repo, authors);
        let mut all_prs: Vec<PullRequest> = Vec::new();
        let mut cursor: Option<String> = None;

        loop {
            let variables = serde_json::json!({ "q": search_query, "cursor": cursor });
            let data: SearchData = self.graphql(QUERY, variables)?;
            all_prs.extend(pull_requests_from_search_nodes(&data.search.nodes));

            if !data.search.page_info.has_next_page {
                break;
            }
            match data.search.page_info.end_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }

        Ok(all_prs)
    }
}

/// Build the GitHub search string for author-scoped open-PR discovery: the repo, `is:pr is:open`,
/// and one `author:` qualifier per login (multiple `author:` qualifiers OR together in search).
fn build_author_search_query(repo: &RepoIdentifier, authors: &[String]) -> String {
    let mut query = format!("repo:{}/{} is:pr is:open", repo.owner, repo.repo);
    for login in authors {
        query.push_str(&format!(" author:{login}"));
    }
    query
}

/// Map GraphQL `search` nodes into `PullRequest`s, dropping fork PRs (`isCrossRepository`) and any
/// node missing the core PR fields (e.g. an empty non-PR result). All results come from an
/// `is:open` search, so `state` is hardcoded to `PrState::Open`; the base SHA is unused downstream
/// (`RemotePr` carries only `base.ref_name`), so it's left empty.
fn pull_requests_from_search_nodes(nodes: &[SearchNode]) -> Vec<PullRequest> {
    nodes
        .iter()
        .filter_map(|node| {
            if node.is_cross_repository == Some(true) {
                return None;
            }
            let number = node.number?;
            let head_ref_name = node.head_ref_name.clone()?;
            let base_ref_name = node.base_ref_name.clone()?;
            Some(PullRequest {
                number,
                state: PrState::Open,
                title: node.title.clone().unwrap_or_default(),
                html_url: node.url.clone().unwrap_or_default(),
                base: PrBranchRef {
                    ref_name: base_ref_name,
                    sha: String::new(),
                    repo: node.base_repository.as_ref().map(|r| PrRepoRef {
                        full_name: r.name_with_owner.clone(),
                    }),
                },
                head: PrBranchRef {
                    ref_name: head_ref_name,
                    sha: node.head_ref_oid.clone().unwrap_or_default(),
                    repo: node.head_repository.as_ref().map(|r| PrRepoRef {
                        full_name: r.name_with_owner.clone(),
                    }),
                },
                user: PrUser {
                    login: node
                        .author
                        .as_ref()
                        .map(|a| a.login.clone())
                        .unwrap_or_default(),
                },
                draft: node.is_draft.unwrap_or(false),
                merged: false,
                merged_at: None,
                updated_at: node.updated_at.clone().unwrap_or_default(),
            })
        })
        .collect()
}

/// A genuine transport failure (DNS/connect/TLS/timeout); status codes never land here now
/// because the agent has `http_status_as_error(false)`.
fn transport_error(error: ureq::Error) -> GitHubError {
    GitHubError::Network(error.to_string())
}

/// Status-check + JSON-deserialize. Non-2xx reads the body and classifies the error.
fn read_checked<T: serde::de::DeserializeOwned>(
    mut response: ureq::http::Response<ureq::Body>,
) -> Result<T, GitHubError> {
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        let body = response.body_mut().read_to_string().unwrap_or_default();
        return Err(classify_status_error(status, &body));
    }
    response
        .body_mut()
        .read_json()
        .map_err(|e| GitHubError::Network(e.to_string()))
}

const CLASSIC_PAT_MARKER: &str = "forbids access via a personal access token (classic)";

/// Map an HTTP error status + body to a `GitHubError`. Preserves prior behavior (401 →
/// Unauthorized; others → Api) and adds classic-PAT detection on 403.
fn classify_status_error(status: u16, body: &str) -> GitHubError {
    match status {
        401 => GitHubError::Unauthorized,
        403 if body.to_ascii_lowercase().contains(CLASSIC_PAT_MARKER) => {
            GitHubError::ClassicPatForbidden {
                org: parse_forbidden_org(body),
            }
        }
        _ => GitHubError::Api {
            status,
            message: api_error_message(body, status),
        },
    }
}

/// Extract GitHub's `message` field from a JSON error body; fall back to the raw body, then to a
/// bare "HTTP <status>".
fn api_error_message(body: &str, status: u16) -> String {
    #[derive(Deserialize)]
    struct ApiErrorBody {
        message: Option<String>,
    }
    serde_json::from_str::<ApiErrorBody>(body)
        .ok()
        .and_then(|b| b.message)
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| {
            let t = body.trim();
            if t.is_empty() {
                format!("HTTP {status}")
            } else {
                t.to_string()
            }
        })
}

/// Pull the org login out of the classic-PAT-forbidden message. GitHub phrases it
/// "<org> forbids access via a personal access token (classic)"; the org is the whitespace/quote-
/// delimited token immediately before the marker. Returns None if it doesn't look like a login.
fn parse_forbidden_org(body: &str) -> Option<String> {
    let lower = body.to_ascii_lowercase(); // ASCII marker → byte indices align with `body`
    let pos = lower.find(CLASSIC_PAT_MARKER)?;
    let org = body[..pos]
        .trim_end()
        .rsplit(|c: char| c.is_whitespace() || c == '"' || c == '\'')
        .find(|s| !s.is_empty())?;
    (!org.is_empty() && org.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        .then(|| org.to_string())
}

// ============== Helper Functions ==============

/// Build a render-ready `PrListResult` from cached open PRs, without needing a `GitHubClient`
/// (so the no-token / offline fallback path can use it). `prs` is the fork-filtered
/// `PullRequest` map; `all_authors` spans every cached branch's author (forks included, matching
/// `list_prs`).
pub fn pr_list_result_from_cached(
    cached: &std::collections::HashMap<String, CachedPullRequest>,
) -> PrListResult {
    let all_authors: std::collections::HashMap<String, String> = cached
        .iter()
        .map(|(branch, pr)| (branch.clone(), pr.user.login.clone()))
        .collect();

    let prs: std::collections::HashMap<String, PullRequest> = cached
        .iter()
        .map(|(branch, cached_pr)| (branch.clone(), PullRequest::from(cached_pr)))
        .filter(|(_, pr)| !pr.is_from_fork())
        .collect();

    PrListResult { prs, all_authors }
}

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

/// The on-disk state of `authors_filter`, distinguishing "unset" from "explicitly empty".
///
/// - `Default` — no `authors_filter` key at all → resolve to `[<your login>]`.
/// - `Explicit(vec![])` — `authors_filter: []` → show everyone (filtering off).
/// - `Explicit([a, b])` — filter to exactly those authors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfiguredAuthorsFilter {
    Default,
    Explicit(Vec<String>),
}

/// Read the three-state `authors_filter` from the GitHub config file. An absent config file or an
/// absent key both mean `Default`.
pub fn configured_authors_filter() -> ConfiguredAuthorsFilter {
    match load_github_config_file().and_then(|c| c.authors_filter) {
        Some(list) => ConfiguredAuthorsFilter::Explicit(list),
        None => ConfiguredAuthorsFilter::Default,
    }
}

/// Whether pushes issued by `git stack restack --push` should bypass Git's pre-push hook.
/// Missing config files and missing keys retain Git's default hook behavior.
pub fn restack_push_no_verify() -> bool {
    load_github_config_file()
        .map(|config| config.restack_push_no_verify)
        .unwrap_or(false)
}

/// Pure resolution core for the three-state author filter, with all identity inputs injected so
/// the "can't resolve → error" path is unit-testable with no live API.
///
/// - `Explicit(v)` → `v` (works offline; identity is irrelevant).
/// - `Default` → `[login]`, preferring a freshly-`fetched_login` over a `cached_login`, and
///   erroring (never guessing) when neither is available.
fn resolve_effective_authors_filter_core(
    configured: ConfiguredAuthorsFilter,
    cached_login: Option<String>,
    fetched_login: Option<String>,
) -> Result<Vec<String>> {
    match configured {
        ConfiguredAuthorsFilter::Explicit(list) => Ok(list),
        ConfiguredAuthorsFilter::Default => match fetched_login.or(cached_login) {
            Some(login) => Ok(vec![login]),
            None => bail!(
                "Could not determine your GitHub login to filter the stack to your own branches.\n\
                 Fix this in any of these ways:\n\
                 \x20 - run `git stack auth login` (or set GITHUB_TOKEN / GH_TOKEN) so git-stack \
                 can look up your login;\n\
                 \x20 - set `authors_filter: [<your-login>]` (or a specific list) in \
                 ~/.config/git-stack/github.yaml;\n\
                 \x20 - set `authors_filter: []` (or pass `--show-all`) to show everyone's branches."
            ),
        },
    }
}

/// Resolve the three-state author filter to a concrete list (the central entry point).
///
/// `Explicit` config short-circuits with no identity work (offline-safe). For `Default`:
/// - with `live_client = Some` (always-online callers like `sync`): live `whoami` refresh, writing
///   through to the host-keyed cache, falling back to the cache on failure;
/// - with `live_client = None` (the `status`/`interactive` hot path): cache-first, building a
///   client on demand only on a cache miss (cold-cache path).
///
/// Errors — never guesses — when a `Default` filter can't be resolved to a login by any means.
pub fn resolve_effective_authors_filter(
    repo_id: &RepoIdentifier,
    live_client: Option<&GitHubClient>,
) -> Result<Vec<String>> {
    let configured = configured_authors_filter();
    // Explicit config never needs identity resolution.
    if let ConfiguredAuthorsFilter::Explicit(list) = configured {
        return Ok(list);
    }

    let cache = crate::pr_cache::PrCacheHandle::open().ok();
    let cached_login = cache
        .as_ref()
        .and_then(|c| c.identity(&repo_id.host).ok().flatten());

    // Fetch a live login only when it's worth it: refresh on the always-online callers, and on the
    // hot path only when the cache missed (cold cache). A warm cache with no live client fetches
    // nothing.
    let fetch_and_cache = |client: &GitHubClient| -> Option<String> {
        match client.whoami() {
            Ok(login) => {
                if let Some(cache) = &cache {
                    let _ = cache.put_identity(&repo_id.host, &login);
                }
                Some(login)
            }
            Err(e) => {
                tracing::debug!("whoami lookup failed for {}: {e}", repo_id.host);
                None
            }
        }
    };

    let fetched_login = if let Some(client) = live_client {
        fetch_and_cache(client)
    } else if cached_login.is_none() {
        match GitHubClient::from_env(repo_id) {
            Ok(client) => fetch_and_cache(&client),
            Err(e) => {
                tracing::debug!("could not build client for whoami on {}: {e}", repo_id.host);
                None
            }
        }
    } else {
        None
    };

    resolve_effective_authors_filter_core(
        ConfiguredAuthorsFilter::Default,
        cached_login,
        fetched_login,
    )
}

/// Cache-only resolution of the effective author filter: never touches the network and never
/// errors. Returns `None` for a `Default` filter with no cached login, letting the caller fall
/// back to its own network-free default behavior. Used by the `eager_refresh_lkgs` hot path.
pub fn resolve_effective_authors_filter_cached(repo_id: &RepoIdentifier) -> Option<Vec<String>> {
    match configured_authors_filter() {
        ConfiguredAuthorsFilter::Explicit(list) => Some(list),
        ConfiguredAuthorsFilter::Default => {
            let cache = crate::pr_cache::PrCacheHandle::open().ok()?;
            let login = cache.identity(&repo_id.host).ok().flatten()?;
            Some(vec![login])
        }
    }
}

/// Best-effort force-live `whoami` + host-keyed cache write, ignoring all errors. Used by
/// `auth login` to warm the identity cache after a successful login. Returns the login on success.
pub fn refresh_self_login(repo_id: &RepoIdentifier) -> Option<String> {
    let client = GitHubClient::from_env(repo_id).ok()?;
    let login = client.whoami().ok()?;
    if let Ok(cache) = crate::pr_cache::PrCacheHandle::open() {
        let _ = cache.put_identity(&repo_id.host, &login);
    }
    Some(login)
}

/// Case-insensitive membership test for the author filter. GitHub logins are
/// case-insensitive, so a config entry `WBBradley` matches a `wbbradley` login.
pub(crate) fn author_in_filter(authors_filter: &[String], author: &str) -> bool {
    authors_filter
        .iter()
        .any(|a| a.eq_ignore_ascii_case(author))
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

/// Find a GitHub token from various sources.
fn find_github_config(host: &str) -> Result<String, GitHubError> {
    match resolve_github_auth(host) {
        Some((token, _)) => Ok(token),
        None => Err(GitHubError::NoToken),
    }
}

/// GitHub config file structure
#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct GitHubConfigFile {
    default_token: Option<String>,
    hosts: Option<std::collections::HashMap<String, String>>,
    /// GitHub usernames whose PRs should be displayed prominently in status. Three states:
    /// **absent** (`None`) → default to filtering to your own login; **`[]`** → show everyone
    /// (filtering off); **`[a, b]`** → filter to exactly those authors. When filtering is active,
    /// branches whose PR author isn't listed are hidden from `status`/`interactive`, except the
    /// current branch, its ancestor chain to trunk, and branches with no PR yet. `--show-all`
    /// bypasses this for one invocation.
    ///
    /// `skip_serializing_if` is critical: the write-back paths round-trip this config, and `None`
    /// must NOT be re-materialized as `[]` (that would flip "unset → [me]" into "explicit [] →
    /// everyone").
    #[serde(
        default,
        alias = "display_authors",
        skip_serializing_if = "Option::is_none"
    )]
    authors_filter: Option<Vec<String>>,
    /// Add `--no-verify` to pushes performed by `git stack restack --push`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    restack_push_no_verify: bool,
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

/// Path to the GitHub config file, creating its parent directory if needed so an editor can
/// save a not-yet-existing file.
pub fn ensure_github_config_path() -> Result<PathBuf> {
    let base_dirs = xdg::BaseDirectories::with_prefix("git-stack");
    base_dirs
        .place_config_file("github.yaml")
        .context("Failed to determine config file path")
}

/// Validate an edited GitHub config while preserving serde_yaml's location and schema details in
/// the returned error.
pub(crate) fn validate_github_config(path: &Path) -> Result<()> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("reading GitHub config file {}", path.display()))?;
    // The normal loader treats an empty file like an absent config, with every setting defaulted.
    if contents.trim().is_empty() {
        return Ok(());
    }
    serde_yaml::from_str::<GitHubConfigFile>(&contents)
        .with_context(|| format!("parsing GitHub config file {}", path.display()))?;
    Ok(())
}

/// Save GitHub token to config file
pub fn save_github_token(token: &str) -> Result<()> {
    let base_dirs = xdg::BaseDirectories::with_prefix("git-stack");
    let config_path = base_dirs
        .place_config_file("github.yaml")
        .context("Failed to create config directory")?;

    // Load existing config to preserve other settings (like authors_filter)
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

    // Load existing config to preserve other settings (PAT, authors_filter).
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
/// is set, both are cleared. `authors_filter` and other settings are preserved.
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
    fn authors_filter_alias_deserializes() {
        let config: GitHubConfigFile = serde_yaml::from_str("display_authors:\n- x\n").unwrap();
        assert_eq!(config.authors_filter, Some(vec!["x".to_string()]));
    }

    #[test]
    fn unknown_config_key_is_rejected_with_expected_keys() {
        let error = serde_yaml::from_str::<GitHubConfigFile>("authors: [x]\n").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("unknown field `authors`"), "{message}");
        assert!(message.contains("authors_filter"), "{message}");
    }

    #[test]
    fn authors_filter_serializes_with_new_key() {
        let config = GitHubConfigFile {
            authors_filter: Some(vec!["x".to_string()]),
            ..Default::default()
        };
        let yaml = serde_yaml::to_string(&config).unwrap();
        assert!(yaml.contains("authors_filter"));
        assert!(!yaml.contains("display_authors"));
    }

    #[test]
    fn authors_filter_absent_deserializes_to_none() {
        // No `authors_filter` key at all → `None` (the "unset → [me]" default).
        let config: GitHubConfigFile = serde_yaml::from_str("default_token: tok\n").unwrap();
        assert_eq!(config.authors_filter, None);
    }

    #[test]
    fn authors_filter_explicit_empty_deserializes_to_some_empty() {
        // `authors_filter: []` → `Some(vec![])` (the explicit "show everyone" state), distinct
        // from an absent key.
        let config: GitHubConfigFile = serde_yaml::from_str("authors_filter: []\n").unwrap();
        assert_eq!(config.authors_filter, Some(vec![]));
    }

    #[test]
    fn authors_filter_explicit_list_deserializes_to_some_list() {
        let config: GitHubConfigFile = serde_yaml::from_str("authors_filter:\n- a\n- b\n").unwrap();
        assert_eq!(
            config.authors_filter,
            Some(vec!["a".to_string(), "b".to_string()])
        );
    }

    #[test]
    fn none_authors_filter_is_not_serialized() {
        // Guards the write-back path: `None` must NOT round-trip to `authors_filter: []`, which
        // would flip "unset → [me]" into "explicit [] → everyone".
        let config = GitHubConfigFile {
            default_token: Some("tok".to_string()),
            authors_filter: None,
            ..Default::default()
        };
        let yaml = serde_yaml::to_string(&config).unwrap();
        assert!(
            !yaml.contains("authors_filter"),
            "None authors_filter must not emit a key, got:\n{yaml}"
        );
    }

    #[test]
    fn restack_push_no_verify_defaults_to_false_when_absent() {
        let config: GitHubConfigFile = serde_yaml::from_str("default_token: tok\n").unwrap();
        assert!(!config.restack_push_no_verify);
    }

    #[test]
    fn restack_push_no_verify_deserializes_and_survives_serialization() {
        let config: GitHubConfigFile =
            serde_yaml::from_str("restack_push_no_verify: true\n").unwrap();
        assert!(config.restack_push_no_verify);

        let yaml = serde_yaml::to_string(&config).unwrap();
        assert!(yaml.contains("restack_push_no_verify: true"));
    }

    #[test]
    fn false_restack_push_no_verify_is_not_materialized_on_auth_writeback() {
        let config = GitHubConfigFile {
            default_token: Some("tok".to_string()),
            ..Default::default()
        };
        let yaml = serde_yaml::to_string(&config).unwrap();
        assert!(!yaml.contains("restack_push_no_verify"));
    }

    #[test]
    fn resolve_core_explicit_empty_shows_everyone() {
        let out = resolve_effective_authors_filter_core(
            ConfiguredAuthorsFilter::Explicit(vec![]),
            None,
            None,
        )
        .unwrap();
        assert_eq!(out, Vec::<String>::new());
    }

    #[test]
    fn resolve_core_explicit_list_passes_through() {
        let out = resolve_effective_authors_filter_core(
            ConfiguredAuthorsFilter::Explicit(vec!["a".to_string(), "b".to_string()]),
            Some("me".to_string()),
            Some("me".to_string()),
        )
        .unwrap();
        assert_eq!(out, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn resolve_core_default_uses_fetched_login() {
        let out = resolve_effective_authors_filter_core(
            ConfiguredAuthorsFilter::Default,
            None,
            Some("me".to_string()),
        )
        .unwrap();
        assert_eq!(out, vec!["me".to_string()]);
    }

    #[test]
    fn resolve_core_default_uses_cached_login() {
        let out = resolve_effective_authors_filter_core(
            ConfiguredAuthorsFilter::Default,
            Some("me".to_string()),
            None,
        )
        .unwrap();
        assert_eq!(out, vec!["me".to_string()]);
    }

    #[test]
    fn resolve_core_default_fetched_wins_over_cached() {
        let out = resolve_effective_authors_filter_core(
            ConfiguredAuthorsFilter::Default,
            Some("stale".to_string()),
            Some("fresh".to_string()),
        )
        .unwrap();
        assert_eq!(out, vec!["fresh".to_string()]);
    }

    #[test]
    fn resolve_core_default_neither_errors() {
        // The acceptance-criteria error path: unset filter + no cached login + can't fetch.
        let err =
            resolve_effective_authors_filter_core(ConfiguredAuthorsFilter::Default, None, None)
                .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("auth login"), "message was: {msg}");
        assert!(msg.contains("authors_filter"), "message was: {msg}");
    }

    #[test]
    fn author_in_filter_is_case_insensitive() {
        assert!(author_in_filter(&["WBBradley".to_string()], "wbbradley"));
        assert!(!author_in_filter(&["alice".to_string()], "bob"));
    }

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

    fn cached_pr(branch: &str, login: &str, from_fork: bool) -> CachedPullRequest {
        // Non-fork: head repo full_name matches base repo full_name. Fork: head repo missing.
        let head_repo = if from_fork {
            None
        } else {
            Some(CachedPrRepoRef {
                full_name: "acme/app".to_string(),
            })
        };
        CachedPullRequest {
            number: 1,
            state: PrState::Open,
            title: "t".to_string(),
            html_url: "https://example.com".to_string(),
            base: CachedPrBranchRef {
                ref_name: "main".to_string(),
                sha: "base".to_string(),
                repo: Some(CachedPrRepoRef {
                    full_name: "acme/app".to_string(),
                }),
            },
            head: CachedPrBranchRef {
                ref_name: branch.to_string(),
                sha: "head".to_string(),
                repo: head_repo,
            },
            user: CachedPrUser {
                login: login.to_string(),
            },
            draft: false,
            merged: false,
            merged_at: None,
            updated_at: "2024-01-01T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn pr_list_result_from_cached_excludes_forks_from_prs_but_keeps_authors() {
        let mut cached = std::collections::HashMap::new();
        cached.insert("mine".to_string(), cached_pr("mine", "alice", false));
        cached.insert("theirs".to_string(), cached_pr("theirs", "bob", true));

        let result = pr_list_result_from_cached(&cached);

        // Non-fork PR is in `prs`; fork PR is excluded from `prs`...
        assert!(result.prs.contains_key("mine"));
        assert!(!result.prs.contains_key("theirs"));

        // ...but both authors are present in `all_authors`.
        assert_eq!(result.all_authors.get("mine").unwrap(), "alice");
        assert_eq!(result.all_authors.get("theirs").unwrap(), "bob");
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

    #[test]
    fn classify_403_classic_pat_detected() {
        let body = r#"{"message":"langchain-ai forbids access via a personal access token (classic). Please use a GitHub App, OAuth App, or a personal access token with fine-grained permissions.","documentation_url":"https://docs.github.com/"}"#;
        let err = classify_status_error(403, body);
        match &err {
            GitHubError::ClassicPatForbidden { org } => {
                assert_eq!(org.as_deref(), Some("langchain-ai"));
            }
            other => panic!("expected ClassicPatForbidden, got {other:?}"),
        }
        let display = err.to_string();
        assert!(display.contains("fine-grained"), "display: {display}");
        assert!(display.contains("langchain-ai"), "display: {display}");
    }

    #[test]
    fn classify_403_case_insensitive() {
        let body =
            r#"{"message":"Acme-Corp FORBIDS ACCESS VIA A PERSONAL ACCESS TOKEN (CLASSIC)."}"#;
        match classify_status_error(403, body) {
            GitHubError::ClassicPatForbidden { org } => {
                assert_eq!(org.as_deref(), Some("Acme-Corp"));
            }
            other => panic!("expected ClassicPatForbidden, got {other:?}"),
        }
    }

    #[test]
    fn classify_403_generic_permission_denied() {
        let body = r#"{"message":"Resource not accessible by integration"}"#;
        match classify_status_error(403, body) {
            GitHubError::Api { status, message } => {
                assert_eq!(status, 403);
                assert_eq!(message, "Resource not accessible by integration");
            }
            other => panic!("expected Api, got {other:?}"),
        }
    }

    #[test]
    fn classify_401_is_unauthorized() {
        assert!(matches!(
            classify_status_error(401, r#"{"message":"Bad credentials"}"#),
            GitHubError::Unauthorized
        ));
    }

    #[test]
    fn classify_404_and_422_are_api() {
        match classify_status_error(404, r#"{"message":"Not Found"}"#) {
            GitHubError::Api { status, message } => {
                assert_eq!(status, 404);
                assert_eq!(message, "Not Found");
            }
            other => panic!("expected Api, got {other:?}"),
        }
        match classify_status_error(422, r#"{"message":"Validation Failed"}"#) {
            GitHubError::Api { status, message } => {
                assert_eq!(status, 422);
                assert_eq!(message, "Validation Failed");
            }
            other => panic!("expected Api, got {other:?}"),
        }
    }

    #[test]
    fn parse_forbidden_org_missing_returns_none() {
        // Marker present but the preceding token isn't a plausible login.
        let body = "Something odd @@@ forbids access via a personal access token (classic).";
        match classify_status_error(403, body) {
            GitHubError::ClassicPatForbidden { org } => assert_eq!(org, None),
            other => panic!("expected ClassicPatForbidden, got {other:?}"),
        }
        // Display falls back to a generic label when the org is unknown.
        let display = GitHubError::ClassicPatForbidden { org: None }.to_string();
        assert!(display.contains("this organization"), "display: {display}");
    }

    #[test]
    fn api_error_message_falls_back() {
        // Empty body → bare "HTTP <status>".
        assert_eq!(api_error_message("", 500), "HTTP 500");
        // Non-JSON body → raw (trimmed) text.
        assert_eq!(
            api_error_message("  plain text error  ", 500),
            "plain text error"
        );
    }

    fn test_repo() -> RepoIdentifier {
        RepoIdentifier {
            owner: "acme".to_string(),
            repo: "app".to_string(),
            host: "github.com".to_string(),
        }
    }

    fn client_with_api_base(api_base: &str) -> GitHubClient {
        GitHubClient::new(GitHubConfig {
            token: "t".to_string(),
            api_base: api_base.to_string(),
        })
    }

    #[test]
    fn build_author_search_query_has_repo_and_per_author_qualifier() {
        let q = build_author_search_query(&test_repo(), &["alice".to_string(), "bob".to_string()]);
        assert!(q.contains("repo:acme/app is:pr is:open"), "q was: {q}");
        assert!(q.contains("author:alice"), "q was: {q}");
        assert!(q.contains("author:bob"), "q was: {q}");
    }

    #[test]
    fn graphql_url_github_com_appends_graphql() {
        let client = client_with_api_base("https://api.github.com");
        assert_eq!(client.graphql_url(), "https://api.github.com/graphql");
    }

    #[test]
    fn graphql_url_ghe_replaces_api_v3() {
        let client = client_with_api_base("https://ghe.host/api/v3");
        assert_eq!(client.graphql_url(), "https://ghe.host/api/graphql");
    }

    #[test]
    fn list_open_prs_by_authors_empty_short_circuits_without_http() {
        // Empty authors returns Ok(vec![]) before any network call.
        let client = client_with_api_base("https://api.github.com");
        let prs = client.list_open_prs_by_authors(&test_repo(), &[]).unwrap();
        assert!(prs.is_empty());
    }

    #[test]
    fn search_nodes_map_to_pull_requests() {
        let json = r#"{
            "search": {
                "pageInfo": { "hasNextPage": false, "endCursor": null },
                "nodes": [
                    {
                        "number": 4626,
                        "title": "ctrl-d handling",
                        "url": "https://github.com/acme/app/pull/4626",
                        "isDraft": true,
                        "isCrossRepository": false,
                        "updatedAt": "2026-07-01T00:00:00Z",
                        "baseRefName": "main",
                        "headRefName": "wbbradley/code/ctrl-d",
                        "headRefOid": "deadbeef",
                        "headRepository": { "nameWithOwner": "acme/app" },
                        "baseRepository": { "nameWithOwner": "acme/app" },
                        "author": { "login": "wbbradley" }
                    }
                ]
            }
        }"#;
        let data: SearchData = serde_json::from_str(json).unwrap();
        let prs = pull_requests_from_search_nodes(&data.search.nodes);
        assert_eq!(prs.len(), 1);
        let pr = &prs[0];
        assert_eq!(pr.number, 4626);
        assert_eq!(pr.head.ref_name, "wbbradley/code/ctrl-d");
        assert_eq!(pr.base.ref_name, "main");
        assert_eq!(pr.head.sha, "deadbeef");
        assert_eq!(pr.user.login, "wbbradley");
        assert!(pr.draft);
        assert_eq!(pr.state, PrState::Open);
        // Non-fork head/base repos map through so downstream fork detection stays consistent.
        assert!(!pr.is_from_fork());
    }

    #[test]
    fn search_nodes_drop_cross_repository_forks() {
        let json = r#"{
            "search": {
                "pageInfo": { "hasNextPage": false, "endCursor": null },
                "nodes": [
                    {
                        "number": 1,
                        "title": "fork pr",
                        "url": "https://github.com/acme/app/pull/1",
                        "isDraft": false,
                        "isCrossRepository": true,
                        "updatedAt": "2026-07-01T00:00:00Z",
                        "baseRefName": "main",
                        "headRefName": "feature",
                        "headRefOid": "abc",
                        "headRepository": { "nameWithOwner": "someone/app" },
                        "baseRepository": { "nameWithOwner": "acme/app" },
                        "author": { "login": "someone" }
                    }
                ]
            }
        }"#;
        let data: SearchData = serde_json::from_str(json).unwrap();
        let prs = pull_requests_from_search_nodes(&data.search.nodes);
        assert!(prs.is_empty());
    }

    #[test]
    fn search_nodes_skip_empty_non_pr_nodes() {
        // GitHub's ISSUE search can include an empty `{}` node the PullRequest fragment doesn't
        // populate; it must deserialize cleanly and be skipped rather than panic.
        let json = r#"{
            "search": {
                "pageInfo": { "hasNextPage": false, "endCursor": null },
                "nodes": [ {} ]
            }
        }"#;
        let data: SearchData = serde_json::from_str(json).unwrap();
        let prs = pull_requests_from_search_nodes(&data.search.nodes);
        assert!(prs.is_empty());
    }
}
