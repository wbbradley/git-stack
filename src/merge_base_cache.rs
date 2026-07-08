//! Repo-scoped, persistent cache for `merge_base` / `is_ancestor` results, backed by `redb`.
//!
//! `GitRepo::merge_base` and `GitRepo::is_ancestor` each do a full libgit2 graph walk. But both
//! are pure functions of two immutable, content-addressed commit OIDs, so a computed result is
//! valid forever: a rebase/force-push simply changes the OIDs, orphaning old rows rather than
//! returning a wrong answer. On a large branch tree the same `(scope, oid_a, oid_b)` pairs are
//! recomputed on every invocation, so persisting them across runs collapses the repeated walks to
//! a single cost-per-pair.
//!
//! Modeled directly on [`crate::pr_cache`]. The `scope` here is the repo's canonicalized common
//! git dir (local-only, works for repos with no GitHub remote), *not* the GitHub `owner/repo`
//! string `pr_cache` keys on.

use std::path::Path;

use anyhow::{Context, Result};
use redb::{ReadableDatabase, ReadableTable, TableDefinition, TableError};

/// (scope, oid_lo, oid_hi) -> merge_base_oid. `merge_base` is symmetric, so the two OIDs are
/// normalized into `(lo, hi)` by lexicographic sort before keying.
const MERGE_BASE_TABLE: TableDefinition<(&str, &str, &str), &str> =
    TableDefinition::new("merge_base_v1");
/// (scope, ancestor_oid, descendant_oid) -> 0|1. `is_ancestor` is *not* symmetric, so key order is
/// preserved.
const IS_ANCESTOR_TABLE: TableDefinition<(&str, &str, &str), u8> =
    TableDefinition::new("is_ancestor_v1");

pub struct MergeBaseCacheHandle {
    db: redb::Database,
}

impl MergeBaseCacheHandle {
    /// Open the merge-base cache database at its default XDG state path.
    pub fn open() -> Result<Self> {
        let path = get_merge_base_cache_path()?;
        Self::open_at(&path)
    }

    /// Open (or create) the merge-base cache database at an explicit path. Exposed for tests.
    pub fn open_at(path: &Path) -> Result<Self> {
        let db = redb::Database::create(path)
            .with_context(|| format!("opening merge-base cache database at {}", path.display()))?;
        secure_permissions(path)?;
        tracing::debug!("Opened merge-base cache database at {}", path.display());
        Ok(Self { db })
    }

    /// The cached merge-base OID for `(a, b)`, if one has ever been written. Normalizes the two
    /// OIDs into `(lo, hi)` order, so both call orderings hit the same row.
    pub fn get_merge_base(&self, scope: &str, a: &str, b: &str) -> Result<Option<String>> {
        let (lo, hi) = normalize(a, b);
        let read_txn = self
            .db
            .begin_read()
            .context("opening merge-base cache read transaction")?;
        let table = match read_txn.open_table(MERGE_BASE_TABLE) {
            Ok(table) => table,
            Err(TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(anyhow::Error::from(e).context("opening merge_base table")),
        };
        Ok(table
            .get((scope, lo, hi))
            .context("reading cached merge-base")?
            .map(|guard| guard.value().to_string()))
    }

    /// Cache the merge-base OID for `(a, b)`. Normalizes the two OIDs into `(lo, hi)` order.
    pub fn put_merge_base(&self, scope: &str, a: &str, b: &str, base_oid: &str) -> Result<()> {
        let (lo, hi) = normalize(a, b);
        let write_txn = self
            .db
            .begin_write()
            .context("opening merge-base cache write transaction")?;
        {
            let mut table = write_txn
                .open_table(MERGE_BASE_TABLE)
                .context("opening merge_base table")?;
            table
                .insert((scope, lo, hi), base_oid)
                .context("inserting cached merge-base")?;
        }
        write_txn
            .commit()
            .context("committing merge-base cache write")?;
        Ok(())
    }

    /// The cached `is_ancestor(ancestor, descendant)` result, if one has ever been written.
    pub fn get_is_ancestor(
        &self,
        scope: &str,
        ancestor: &str,
        descendant: &str,
    ) -> Result<Option<bool>> {
        let read_txn = self
            .db
            .begin_read()
            .context("opening merge-base cache read transaction")?;
        let table = match read_txn.open_table(IS_ANCESTOR_TABLE) {
            Ok(table) => table,
            Err(TableError::TableDoesNotExist(_)) => return Ok(None),
            Err(e) => return Err(anyhow::Error::from(e).context("opening is_ancestor table")),
        };
        Ok(table
            .get((scope, ancestor, descendant))
            .context("reading cached is-ancestor")?
            .map(|guard| guard.value() != 0))
    }

    /// Cache the `is_ancestor(ancestor, descendant)` result. Preserves key order (not symmetric).
    pub fn put_is_ancestor(
        &self,
        scope: &str,
        ancestor: &str,
        descendant: &str,
        val: bool,
    ) -> Result<()> {
        let write_txn = self
            .db
            .begin_write()
            .context("opening merge-base cache write transaction")?;
        {
            let mut table = write_txn
                .open_table(IS_ANCESTOR_TABLE)
                .context("opening is_ancestor table")?;
            table
                .insert((scope, ancestor, descendant), u8::from(val))
                .context("inserting cached is-ancestor")?;
        }
        write_txn
            .commit()
            .context("committing merge-base cache write")?;
        Ok(())
    }

    /// Remove all cached rows for `scope` in both tables.
    pub fn clear_scope(&self, scope: &str) -> Result<()> {
        let write_txn = self
            .db
            .begin_write()
            .context("opening merge-base cache write transaction")?;
        {
            let mut table = match write_txn.open_table(MERGE_BASE_TABLE) {
                Ok(table) => Some(table),
                Err(TableError::TableDoesNotExist(_)) => None,
                Err(e) => return Err(anyhow::Error::from(e).context("opening merge_base table")),
            };
            if let Some(table) = table.as_mut() {
                let keys: Vec<(String, String)> = {
                    let mut keys = Vec::new();
                    for entry in table
                        .range((scope, "", "")..)
                        .context("scanning merge_base table")?
                    {
                        let (key, _) = entry.context("reading merge-base cache entry")?;
                        let (key_scope, lo, hi) = key.value();
                        if key_scope != scope {
                            break;
                        }
                        keys.push((lo.to_string(), hi.to_string()));
                    }
                    keys
                };
                for (lo, hi) in keys {
                    table
                        .remove((scope, lo.as_str(), hi.as_str()))
                        .context("removing cached merge-base")?;
                }
            }
        }
        {
            let mut table = match write_txn.open_table(IS_ANCESTOR_TABLE) {
                Ok(table) => Some(table),
                Err(TableError::TableDoesNotExist(_)) => None,
                Err(e) => return Err(anyhow::Error::from(e).context("opening is_ancestor table")),
            };
            if let Some(table) = table.as_mut() {
                let keys: Vec<(String, String)> = {
                    let mut keys = Vec::new();
                    for entry in table
                        .range((scope, "", "")..)
                        .context("scanning is_ancestor table")?
                    {
                        let (key, _) = entry.context("reading is-ancestor cache entry")?;
                        let (key_scope, anc, desc) = key.value();
                        if key_scope != scope {
                            break;
                        }
                        keys.push((anc.to_string(), desc.to_string()));
                    }
                    keys
                };
                for (anc, desc) in keys {
                    table
                        .remove((scope, anc.as_str(), desc.as_str()))
                        .context("removing cached is-ancestor")?;
                }
            }
        }
        write_txn
            .commit()
            .context("committing merge-base cache clear")?;
        Ok(())
    }
}

/// Lexicographically sort two OID strings into `(lo, hi)`, so `merge_base`'s symmetry maps both
/// call orderings onto a single cache row.
fn normalize<'a>(a: &'a str, b: &'a str) -> (&'a str, &'a str) {
    if a <= b { (a, b) } else { (b, a) }
}

fn get_merge_base_cache_path() -> Result<std::path::PathBuf> {
    let base_dirs = xdg::BaseDirectories::with_prefix(env!("CARGO_PKG_NAME"));
    base_dirs
        .place_state_file("merge_base_cache.redb")
        .context("Failed to determine merge-base cache database path")
}

/// Restrict the cache file to owner-only access, mirroring `pr_cache`'s convention. `redb` owns
/// its own binary file I/O, so it can't go through the `&str`-typed `write_file_secure` helper.
#[cfg(unix)]
fn secure_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = std::fs::metadata(path)
        .context("reading merge-base cache file metadata")?
        .permissions();
    perms.set_mode(0o600);
    std::fs::set_permissions(path, perms).context("setting merge-base cache file permissions")?;
    Ok(())
}

#[cfg(not(unix))]
fn secure_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // 40-char hex OID-shaped strings for keys.
    const OID_A: &str = "1111111111111111111111111111111111111111";
    const OID_B: &str = "2222222222222222222222222222222222222222";
    const OID_C: &str = "3333333333333333333333333333333333333333";

    fn open_test_handle(dir: &tempfile::TempDir) -> MergeBaseCacheHandle {
        MergeBaseCacheHandle::open_at(&dir.path().join("merge_base_cache.redb")).unwrap()
    }

    #[test]
    fn merge_base_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let handle = open_test_handle(&dir);

        assert_eq!(handle.get_merge_base("/repo", OID_A, OID_B).unwrap(), None);
        handle.put_merge_base("/repo", OID_A, OID_B, OID_C).unwrap();
        assert_eq!(
            handle.get_merge_base("/repo", OID_A, OID_B).unwrap(),
            Some(OID_C.to_string())
        );
    }

    #[test]
    fn is_ancestor_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let handle = open_test_handle(&dir);

        assert_eq!(handle.get_is_ancestor("/repo", OID_A, OID_B).unwrap(), None);
        handle.put_is_ancestor("/repo", OID_A, OID_B, true).unwrap();
        assert_eq!(
            handle.get_is_ancestor("/repo", OID_A, OID_B).unwrap(),
            Some(true)
        );
        handle
            .put_is_ancestor("/repo", OID_B, OID_A, false)
            .unwrap();
        assert_eq!(
            handle.get_is_ancestor("/repo", OID_B, OID_A).unwrap(),
            Some(false)
        );
    }

    #[test]
    fn merge_base_is_symmetric() {
        let dir = tempfile::tempdir().unwrap();
        let handle = open_test_handle(&dir);

        // Write with one ordering, read back with the other.
        handle.put_merge_base("/repo", OID_A, OID_B, OID_C).unwrap();
        assert_eq!(
            handle.get_merge_base("/repo", OID_B, OID_A).unwrap(),
            Some(OID_C.to_string())
        );
    }

    #[test]
    fn is_ancestor_is_not_symmetric() {
        let dir = tempfile::tempdir().unwrap();
        let handle = open_test_handle(&dir);

        handle.put_is_ancestor("/repo", OID_A, OID_B, true).unwrap();
        // The reverse ordering must be a distinct, still-empty row.
        assert_eq!(handle.get_is_ancestor("/repo", OID_B, OID_A).unwrap(), None);
    }

    #[test]
    fn scopes_do_not_leak_across_prefix_collisions() {
        let dir = tempfile::tempdir().unwrap();
        let handle = open_test_handle(&dir);

        // Adversarial prefix-colliding scopes: "/repo/a" is a prefix of "/repo/a2".
        handle
            .put_merge_base("/repo/a", OID_A, OID_B, OID_C)
            .unwrap();
        handle
            .put_is_ancestor("/repo/a", OID_A, OID_B, true)
            .unwrap();

        handle
            .put_merge_base("/repo/a2", OID_A, OID_B, OID_A)
            .unwrap();
        handle
            .put_is_ancestor("/repo/a2", OID_A, OID_B, false)
            .unwrap();

        assert_eq!(
            handle.get_merge_base("/repo/a", OID_A, OID_B).unwrap(),
            Some(OID_C.to_string())
        );
        assert_eq!(
            handle.get_merge_base("/repo/a2", OID_A, OID_B).unwrap(),
            Some(OID_A.to_string())
        );
        assert_eq!(
            handle.get_is_ancestor("/repo/a", OID_A, OID_B).unwrap(),
            Some(true)
        );
        assert_eq!(
            handle.get_is_ancestor("/repo/a2", OID_A, OID_B).unwrap(),
            Some(false)
        );
    }

    #[test]
    fn data_survives_close_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("merge_base_cache.redb");

        {
            let handle = MergeBaseCacheHandle::open_at(&path).unwrap();
            handle.put_merge_base("/repo", OID_A, OID_B, OID_C).unwrap();
            handle.put_is_ancestor("/repo", OID_A, OID_B, true).unwrap();
        }

        {
            let handle = MergeBaseCacheHandle::open_at(&path).unwrap();
            assert_eq!(
                handle.get_merge_base("/repo", OID_A, OID_B).unwrap(),
                Some(OID_C.to_string())
            );
            assert_eq!(
                handle.get_is_ancestor("/repo", OID_A, OID_B).unwrap(),
                Some(true)
            );
        }
    }

    #[test]
    fn clear_scope_removes_only_target_scope() {
        let dir = tempfile::tempdir().unwrap();
        let handle = open_test_handle(&dir);

        handle
            .put_merge_base("/repo/a", OID_A, OID_B, OID_C)
            .unwrap();
        handle
            .put_is_ancestor("/repo/a", OID_A, OID_B, true)
            .unwrap();
        handle
            .put_merge_base("/repo/b", OID_A, OID_B, OID_C)
            .unwrap();
        handle
            .put_is_ancestor("/repo/b", OID_A, OID_B, true)
            .unwrap();

        handle.clear_scope("/repo/a").unwrap();

        assert_eq!(
            handle.get_merge_base("/repo/a", OID_A, OID_B).unwrap(),
            None
        );
        assert_eq!(
            handle.get_is_ancestor("/repo/a", OID_A, OID_B).unwrap(),
            None
        );

        assert_eq!(
            handle.get_merge_base("/repo/b", OID_A, OID_B).unwrap(),
            Some(OID_C.to_string())
        );
        assert_eq!(
            handle.get_is_ancestor("/repo/b", OID_A, OID_B).unwrap(),
            Some(true)
        );
    }

    #[test]
    fn clear_scope_on_missing_tables_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let handle = open_test_handle(&dir);
        // Never wrote anything; tables don't exist yet. Clearing must not error.
        handle.clear_scope("/repo").unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn opened_database_file_has_0600_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("merge_base_cache.redb");
        let _handle = MergeBaseCacheHandle::open_at(&path).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
