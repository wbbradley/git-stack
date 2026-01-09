use std::{
    process::{Command, ExitStatus},
    time::Instant,
};

use anyhow::{Context, Result, anyhow, bail};

use crate::{
    git2_ops::{DEFAULT_REMOTE, GitRepo},
    stats::record_git_command,
};

pub struct GitOutput {
    pub(crate) stdout: String,
}

impl GitOutput {
    pub fn is_empty(&self) -> bool {
        self.stdout.is_empty()
    }
    pub fn output(self) -> Option<String> {
        if self.stdout.is_empty() {
            None
        } else {
            Some(self.stdout.trim().to_string())
        }
    }
    pub fn output_or(self, message: impl AsRef<str>) -> Result<String> {
        if self.stdout.is_empty() {
            Err(anyhow!("{}", message.as_ref()))
        } else {
            Ok(self.stdout)
        }
    }
}

impl AsRef<str> for GitOutput {
    fn as_ref(&self) -> &str {
        &self.stdout
    }
}

/// Run a git command and return the output. If the git command fails, this will return an error.
pub(crate) fn run_git_passthrough(args: &[&str]) -> Result<ExitStatus> {
    let start = Instant::now();
    tracing::debug!("Running `git {}`", args.join(" "));
    let mut child = Command::new("git").args(args).spawn()?;
    let result = child.wait()?;
    record_git_command(args, start.elapsed());
    Ok(result)
}

/// Run a git command and return the output. If the git command fails, this will return an error.
pub(crate) fn run_git(args: &[&str]) -> Result<GitOutput> {
    let start = Instant::now();
    tracing::debug!("Running `git {}`", args.join(" "));
    let out = Command::new("git")
        .args(args)
        .stderr(std::process::Stdio::piped())
        .output()
        .with_context(|| format!("running git {args:?}"))?;
    record_git_command(args, start.elapsed());
    if !out.status.success() {
        tracing::debug!(
            ?args,
            "git error: {}",
            std::str::from_utf8(&out.stderr).unwrap_or("")
        );
        bail!(
            "`git {}` failed with exit status: {}",
            args.join(" "),
            out.status
        );
    }
    Ok(GitOutput {
        stdout: String::from_utf8_lossy(&out.stdout).trim().to_string(),
    })
}

pub(crate) fn run_git_status(args: &[&str], stdin: Option<&str>) -> Result<ExitStatus> {
    let start = Instant::now();
    tracing::debug!("Running `git {}`", args.join(" "));
    let status = if let Some(stdin_text) = stdin {
        let mut child = Command::new("git")
            .args(args)
            .stdin(std::process::Stdio::piped())
            .spawn()?;
        let stdin = child.stdin.as_mut().context("Failed to open stdin")?;
        std::io::Write::write_all(stdin, stdin_text.as_bytes())?;
        child.wait()?
    } else {
        let output = Command::new("git")
            .args(args)
            .stderr(std::process::Stdio::piped())
            .output()?;
        if !output.status.success() {
            tracing::debug!(
                ?args,
                "git error: {}",
                std::str::from_utf8(&output.stderr).unwrap_or("")
            );
        }
        output.status
    };
    record_git_command(args, start.elapsed());
    Ok(status)
}

pub(crate) fn git_fetch() -> Result<()> {
    let _ = run_git(&["fetch", "--prune"])?;
    Ok(())
}

pub(crate) fn git_branch_exists(repo: &GitRepo, branch: &str) -> bool {
    repo.branch_exists(branch)
}

pub(crate) fn run_git_status_clean() -> Result<bool> {
    Ok(run_git(&["status", "--porcelain"])?.is_empty())
}

/// Counts of local changes by category from `git status --porcelain`
#[derive(Debug, Default)]
pub struct LocalStatus {
    /// Files with staged changes (index differs from HEAD)
    pub staged: usize,
    /// Files with unstaged changes (working tree differs from index)
    pub unstaged: usize,
    /// Untracked files
    pub untracked: usize,
}

impl LocalStatus {
    pub fn is_clean(&self) -> bool {
        self.staged == 0 && self.unstaged == 0 && self.untracked == 0
    }
}

/// Get counts of local changes by category
pub(crate) fn get_local_status() -> Result<LocalStatus> {
    // Run git status directly to avoid run_git's trim() which strips leading spaces
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .context("running git status --porcelain")?;

    if !output.status.success() {
        bail!("git status --porcelain failed");
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut status = LocalStatus::default();

    for line in stdout.lines() {
        if line.len() < 2 {
            continue;
        }
        let bytes = line.as_bytes();
        let index_status = bytes[0];
        let worktree_status = bytes[1];

        if index_status == b'?' {
            status.untracked += 1;
        } else {
            // Staged if first column has meaningful status (not space)
            if index_status != b' ' {
                status.staged += 1;
            }
            // Unstaged if second column has meaningful status (not space)
            if worktree_status != b' ' {
                status.unstaged += 1;
            }
        }
    }

    Ok(status)
}

pub(crate) fn after_text(s: &str, needle: impl AsRef<str>) -> Option<&str> {
    let needle = needle.as_ref();
    s.find(needle)
        .map(|pos| &s[pos + needle.chars().fold(0, |x, y| x + y.len_utf8())..])
}

pub(crate) fn git_checkout_main(repo: &GitRepo, new_branch: Option<&str>) -> Result<()> {
    if !run_git_status_clean()? {
        bail!("git status is not clean, please commit or stash your changes.")
    }
    git_fetch()?;
    let remote = DEFAULT_REMOTE;
    let trunk = git_trunk(repo)?;

    // Check that we don't orphan unpushed changes in the local `main` branch.
    if !repo.is_ancestor(&trunk.main_branch, &trunk.remote_main)? {
        bail!("It looks like this would orphan unpushed changes in your main branch! Aborting...");
    }

    // Check if we're currently on the main branch
    let current_branch = repo.current_branch().unwrap_or_default();
    if current_branch == trunk.main_branch {
        // Can't use `git branch -f` on the current branch, so checkout directly
        run_git(&["checkout", &trunk.remote_main])?;
        // Now we can update the branch pointer
        run_git(&[
            "branch",
            "-f",
            &trunk.main_branch,
            &format!("{}/{}", remote, trunk.main_branch),
        ])?;
    } else {
        // Repoint "main" to the remote main branch.
        run_git(&[
            "branch",
            "-f",
            &trunk.main_branch,
            &format!("{}/{}", remote, trunk.main_branch),
        ])?;
    }

    if let Some(new_branch) = new_branch {
        run_git(&["checkout", "-B", new_branch, &trunk.remote_main])?;
    }
    Ok(())
}

pub(crate) struct GitTrunk {
    pub(crate) remote_main: String,
    pub(crate) main_branch: String,
}

pub(crate) fn git_trunk(git_repo: &GitRepo) -> Result<GitTrunk> {
    let remote_main = git_repo.remote_main(DEFAULT_REMOTE)?;
    let main_branch = after_text(&remote_main, format!("{DEFAULT_REMOTE}/"))
        .ok_or(anyhow!("no branch?"))?
        .to_string();
    Ok(GitTrunk {
        remote_main,
        main_branch,
    })
}
