//! Per-repo scoped cache for closed PR data, backed by `redb`.
//!
//! Replaces the old single `pr_cache.yaml` file, which held every repo's closed-PR history in one
//! blob and had to be fully parsed/re-serialized for any single-repo read or write. `redb` gives
//! indexed, per-repo access directly: point reads/writes touch only the rows for the repo at
//! hand, never the whole cache.

use std::{collections::HashMap, path::Path};

use anyhow::{Context, Result};
use redb::{ReadableDatabase, ReadableTable, TableDefinition, TableError};

use crate::github::CachedPullRequest;

const WATERMARKS_TABLE: TableDefinition<&str, &str> = TableDefinition::new("watermarks_v1");
const CLOSED_PRS_TABLE: TableDefinition<(&str, &str), &[u8]> =
    TableDefinition::new("closed_prs_v1");

pub struct PrCacheHandle {
    db: redb::Database,
}

impl PrCacheHandle {
    /// Open the PR cache database at its default XDG state path.
    pub fn open() -> Result<Self> {
        let path = get_pr_cache_path()?;
        Self::open_at(&path)
    }

    /// Open (or create) the PR cache database at an explicit path. Exposed for tests.
    pub fn open_at(path: &Path) -> Result<Self> {
        let db = redb::Database::create(path)
            .with_context(|| format!("opening PR cache database at {}", path.display()))?;
        secure_permissions(path)?;
        tracing::debug!("Opened PR cache database at {}", path.display());
        Ok(Self { db })
    }

    /// The cached watermark for `repo`, if one has ever been written.
    pub fn watermark(&self, repo: &str) -> Result<Option<String>> {
        let read_txn = self
            .db
            .begin_read()
            .context("opening PR cache read transaction")?;
        let table = match read_txn.open_table(WATERMARKS_TABLE) {
            Ok(table) => table,
            Err(TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(anyhow::Error::from(e).context("opening watermarks table")),
        };
        Ok(table
            .get(repo)
            .context("reading watermark")?
            .map(|guard| guard.value().to_string()))
    }

    /// All cached closed PRs for `repo`, keyed by head branch name.
    pub fn closed_prs_for_repo(&self, repo: &str) -> Result<HashMap<String, CachedPullRequest>> {
        let read_txn = self
            .db
            .begin_read()
            .context("opening PR cache read transaction")?;
        let table = match read_txn.open_table(CLOSED_PRS_TABLE) {
            Ok(table) => table,
            Err(TableError::TableDoesNotExist(_)) => return Ok(HashMap::new()),
            Err(e) => return Err(anyhow::Error::from(e).context("opening closed PRs table")),
        };

        let mut result = HashMap::new();
        for entry in table
            .range((repo, "")..)
            .context("scanning closed PRs table")?
        {
            let (key, value) = entry.context("reading closed PR cache entry")?;
            let (key_repo, key_branch) = key.value();
            if key_repo != repo {
                break;
            }
            let cached: CachedPullRequest =
                serde_json::from_slice(value.value()).context("deserializing cached PR")?;
            result.insert(key_branch.to_string(), cached);
        }
        Ok(result)
    }

    /// Single-commit upsert of freshly-fetched PRs plus an optional watermark bump. No-ops if
    /// `fresh` is empty and `new_watermark` is `None`.
    pub fn commit_fresh_prs<'a>(
        &self,
        repo: &str,
        fresh: impl Iterator<Item = (&'a str, &'a CachedPullRequest)>,
        new_watermark: Option<&str>,
    ) -> Result<()> {
        let fresh: Vec<(&str, &CachedPullRequest)> = fresh.collect();
        if fresh.is_empty() && new_watermark.is_none() {
            return Ok(());
        }

        let write_txn = self
            .db
            .begin_write()
            .context("opening PR cache write transaction")?;
        {
            let mut table = write_txn
                .open_table(CLOSED_PRS_TABLE)
                .context("opening closed PRs table")?;
            for (branch, pr) in &fresh {
                let value = serde_json::to_vec(pr).context("serializing cached PR")?;
                table
                    .insert((repo, *branch), value.as_slice())
                    .context("inserting cached PR")?;
            }
        }
        if let Some(watermark) = new_watermark {
            let mut table = write_txn
                .open_table(WATERMARKS_TABLE)
                .context("opening watermarks table")?;
            table
                .insert(repo, watermark)
                .context("updating watermark")?;
        }
        write_txn.commit().context("committing PR cache write")?;
        Ok(())
    }

    /// Remove all cached data (closed PRs and watermark) for `repo`.
    pub fn clear_repo(&self, repo: &str) -> Result<()> {
        let write_txn = self
            .db
            .begin_write()
            .context("opening PR cache write transaction")?;
        {
            let mut table = write_txn
                .open_table(CLOSED_PRS_TABLE)
                .context("opening closed PRs table")?;
            let branches: Vec<String> = {
                let mut branches = Vec::new();
                for entry in table
                    .range((repo, "")..)
                    .context("scanning closed PRs table")?
                {
                    let (key, _) = entry.context("reading closed PR cache entry")?;
                    let (key_repo, key_branch) = key.value();
                    if key_repo != repo {
                        break;
                    }
                    branches.push(key_branch.to_string());
                }
                branches
            };
            for branch in branches {
                table
                    .remove((repo, branch.as_str()))
                    .context("removing cached PR")?;
            }
        }
        {
            let mut table = write_txn
                .open_table(WATERMARKS_TABLE)
                .context("opening watermarks table")?;
            table.remove(repo).context("removing watermark")?;
        }
        write_txn.commit().context("committing PR cache clear")?;
        Ok(())
    }
}

/// Clear PR cache for a specific repo (used by `git stack cache clear`).
pub fn clear_pr_cache(repo_full_name: &str) -> Result<()> {
    PrCacheHandle::open()?.clear_repo(repo_full_name)
}

fn get_pr_cache_path() -> Result<std::path::PathBuf> {
    let base_dirs = xdg::BaseDirectories::with_prefix(env!("CARGO_PKG_NAME"));
    base_dirs
        .place_state_file("pr_cache.redb")
        .context("Failed to determine PR cache database path")
}

/// Restrict the PR cache file to owner-only access (it can contain private-repo PR titles/URLs/
/// logins), mirroring `write_file_secure`'s convention for other git-stack state files. `redb`
/// owns its own binary file I/O, so it can't go through that `&str`-typed helper directly.
#[cfg(unix)]
fn secure_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)
        .context("reading PR cache file metadata")?
        .permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(path, perms).context("setting PR cache file permissions")?;
    Ok(())
}

#[cfg(not(unix))]
fn secure_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::{CachedPrBranchRef, CachedPrUser, PrState};

    fn sample_pr(branch: &str, number: u64) -> CachedPullRequest {
        CachedPullRequest {
            number,
            state: PrState::Closed,
            title: format!("PR for {branch}"),
            html_url: format!("https://example.com/{branch}"),
            base: CachedPrBranchRef {
                ref_name: "main".to_string(),
                sha: "basesha".to_string(),
                repo: None,
            },
            head: CachedPrBranchRef {
                ref_name: branch.to_string(),
                sha: "headsha".to_string(),
                repo: None,
            },
            user: CachedPrUser {
                login: "octocat".to_string(),
            },
            draft: false,
            merged: true,
            merged_at: Some("2024-01-01T00:00:00Z".to_string()),
            updated_at: "2024-01-01T00:00:00Z".to_string(),
        }
    }

    fn open_test_handle(dir: &tempfile::TempDir) -> PrCacheHandle {
        PrCacheHandle::open_at(&dir.path().join("pr_cache.redb")).unwrap()
    }

    #[test]
    fn range_scan_is_scoped_to_one_repo() {
        let dir = tempfile::tempdir().unwrap();
        let handle = open_test_handle(&dir);

        // Adversarial prefix-colliding repo names: "acme/app" is a prefix of "acme/app2".
        let app_b1 = sample_pr("b1", 1);
        let app_b2 = sample_pr("b2", 2);
        handle
            .commit_fresh_prs(
                "acme/app",
                vec![("b1", &app_b1), ("b2", &app_b2)].into_iter(),
                None,
            )
            .unwrap();

        let app2_b1 = sample_pr("b1", 3);
        handle
            .commit_fresh_prs("acme/app2", vec![("b1", &app2_b1)].into_iter(), None)
            .unwrap();

        let app_prs = handle.closed_prs_for_repo("acme/app").unwrap();
        assert_eq!(app_prs.len(), 2);
        assert_eq!(app_prs.get("b1").unwrap().number, 1);
        assert_eq!(app_prs.get("b2").unwrap().number, 2);

        let app2_prs = handle.closed_prs_for_repo("acme/app2").unwrap();
        assert_eq!(app2_prs.len(), 1);
        assert_eq!(app2_prs.get("b1").unwrap().number, 3);
    }

    #[test]
    fn first_run_missing_file_yields_empty_cache() {
        let dir = tempfile::tempdir().unwrap();
        let handle = open_test_handle(&dir);

        assert_eq!(handle.watermark("acme/app").unwrap(), None);
        assert!(handle.closed_prs_for_repo("acme/app").unwrap().is_empty());
    }

    #[test]
    fn point_upsert_and_get_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let handle = open_test_handle(&dir);

        let pr = sample_pr("feature", 42);
        handle
            .commit_fresh_prs("acme/app", vec![("feature", &pr)].into_iter(), None)
            .unwrap();

        let prs = handle.closed_prs_for_repo("acme/app").unwrap();
        assert_eq!(prs.len(), 1);
        assert_eq!(prs.get("feature").unwrap(), &pr);
    }

    #[test]
    fn clear_repo_removes_only_target_repo() {
        let dir = tempfile::tempdir().unwrap();
        let handle = open_test_handle(&dir);

        let pr_a = sample_pr("b1", 1);
        let pr_b = sample_pr("b1", 2);
        handle
            .commit_fresh_prs(
                "acme/a",
                vec![("b1", &pr_a)].into_iter(),
                Some("2024-01-01"),
            )
            .unwrap();
        handle
            .commit_fresh_prs(
                "acme/b",
                vec![("b1", &pr_b)].into_iter(),
                Some("2024-02-01"),
            )
            .unwrap();

        handle.clear_repo("acme/a").unwrap();

        assert!(handle.closed_prs_for_repo("acme/a").unwrap().is_empty());
        assert_eq!(handle.watermark("acme/a").unwrap(), None);

        assert_eq!(handle.closed_prs_for_repo("acme/b").unwrap().len(), 1);
        assert_eq!(
            handle.watermark("acme/b").unwrap(),
            Some("2024-02-01".to_string())
        );
    }

    #[test]
    fn watermark_read_write_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let handle = open_test_handle(&dir);

        assert_eq!(handle.watermark("acme/app").unwrap(), None);

        handle
            .commit_fresh_prs("acme/app", std::iter::empty(), Some("2024-01-01T00:00:00Z"))
            .unwrap();
        assert_eq!(
            handle.watermark("acme/app").unwrap(),
            Some("2024-01-01T00:00:00Z".to_string())
        );

        handle
            .commit_fresh_prs("acme/app", std::iter::empty(), Some("2024-06-01T00:00:00Z"))
            .unwrap();
        assert_eq!(
            handle.watermark("acme/app").unwrap(),
            Some("2024-06-01T00:00:00Z".to_string())
        );
    }

    #[test]
    fn data_survives_close_and_reopen() {
        // Every real CLI invocation opens a fresh `PrCacheHandle` (see `PrCacheHandle::open`'s
        // doc comment) rather than sharing one across calls. All the other tests above reuse a
        // single `handle` for both the write and the read, which would not catch a bug where
        // committed data is invisible to a *new* `Database::create` against the same file.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pr_cache.redb");

        {
            let handle = PrCacheHandle::open_at(&path).unwrap();
            let pr = sample_pr("feature", 42);
            handle
                .commit_fresh_prs(
                    "acme/app",
                    vec![("feature", &pr)].into_iter(),
                    Some("2024-01-01T00:00:00Z"),
                )
                .unwrap();
        }

        {
            let handle = PrCacheHandle::open_at(&path).unwrap();
            assert_eq!(
                handle.watermark("acme/app").unwrap(),
                Some("2024-01-01T00:00:00Z".to_string())
            );
            let prs = handle.closed_prs_for_repo("acme/app").unwrap();
            assert_eq!(prs.len(), 1);
            assert_eq!(prs.get("feature").unwrap().number, 42);
        }
    }

    #[test]
    fn large_backfill_survives_close_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pr_cache.redb");

        let prs: Vec<CachedPullRequest> = (0..30_000)
            .map(|i| sample_pr(&format!("branch-{i}"), i as u64))
            .collect();

        {
            let handle = PrCacheHandle::open_at(&path).unwrap();
            let fresh = prs
                .iter()
                .enumerate()
                .map(|(i, pr)| (pr.head.ref_name.as_str(), pr))
                .collect::<Vec<_>>();
            handle
                .commit_fresh_prs("acme/big", fresh.into_iter(), Some("2024-01-01T00:00:00Z"))
                .unwrap();
            let _ = prs.len();
        }

        {
            let handle = PrCacheHandle::open_at(&path).unwrap();
            let cached = handle.closed_prs_for_repo("acme/big").unwrap();
            assert_eq!(cached.len(), 30_000);
            assert_eq!(
                handle.watermark("acme/big").unwrap(),
                Some("2024-01-01T00:00:00Z".to_string())
            );
        }
    }

    #[test]
    fn commit_with_no_changes_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let handle = open_test_handle(&dir);

        handle
            .commit_fresh_prs("acme/app", std::iter::empty(), None)
            .unwrap();

        assert_eq!(handle.watermark("acme/app").unwrap(), None);
        assert!(handle.closed_prs_for_repo("acme/app").unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn opened_database_file_has_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pr_cache.redb");
        let _handle = PrCacheHandle::open_at(&path).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
