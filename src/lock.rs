//! Repo-scoped advisory locking so two git-stack invocations can't mutate the
//! same repository's refs concurrently.
//!
//! This uses an OS advisory lock (`flock`) on the repository's `config` file
//! rather than a sidecar lockfile. It creates no new file and writes no bytes:
//! the lock lives in the kernel, keyed to the open descriptor. Crucially it is
//! released automatically when the descriptor closes — including on crash,
//! panic, SIGKILL, or a closed terminal — so it can never go stale.
//!
//! The lock is advisory, so it only coordinates cooperating git-stack
//! processes; real `git` (which serializes with sidecar `*.lock` files) neither
//! honors nor is blocked by it, so there's no deadlock risk with the git
//! subcommands we spawn while holding it.

use std::{
    fs::{File, TryLockError},
    path::Path,
};

use anyhow::{Context, Result};

/// An acquired repo-scoped advisory lock. Held for the duration of a mutating
/// operation; the lock is released when this value is dropped (the file
/// descriptor closes, and the kernel drops the `flock`).
#[must_use = "the lock is released as soon as the guard is dropped"]
pub(crate) struct RepoLock {
    _file: File,
}

impl RepoLock {
    /// Acquire the repo-scoped lock, blocking (after printing a notice) if
    /// another git-stack process currently holds it.
    ///
    /// `common_dir` should be the repository's common git directory (i.e.
    /// `git2::Repository::commondir()`), so linked worktrees of the same
    /// repository serialize against each other — the refs mutated by
    /// `fetch --prune` live in the common dir.
    pub(crate) fn acquire(common_dir: &Path) -> Result<Self> {
        // `config` always exists in a valid git dir and is never flock'd by git
        // itself, making it a safe, well-known anchor for the advisory lock.
        let path = common_dir.join("config");
        let file = File::open(&path)
            .with_context(|| format!("opening {} to acquire repo lock", path.display()))?;

        match file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                eprintln!(
                    "Another git-stack process is running in this repo; \
                     waiting for it to finish..."
                );
                file.lock()
                    .with_context(|| format!("waiting for repo lock on {}", path.display()))?;
            }
            Err(TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("locking {}", path.display()));
            }
        }

        Ok(Self { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, TryLockError};

    /// While a `RepoLock` is held, a second attempt on the same repo contends
    /// (the guard we return from `acquire` would otherwise block), and once the
    /// first guard is dropped the lock is immediately available again.
    #[test]
    fn lock_is_exclusive_then_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        // Minimal stand-in for a git common dir: just needs a `config` file.
        fs::write(dir.path().join("config"), b"").unwrap();

        let guard = RepoLock::acquire(dir.path()).unwrap();

        // A second acquirer must find the lock contended.
        let probe = File::open(dir.path().join("config")).unwrap();
        assert!(matches!(probe.try_lock(), Err(TryLockError::WouldBlock)));
        drop(probe);

        // After dropping the guard the lock is free again.
        drop(guard);
        let _guard2 = RepoLock::acquire(dir.path()).unwrap();
    }
}
