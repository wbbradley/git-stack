use std::process::{Command, ExitStatus};

use anyhow::{Context, Result, anyhow, bail};
pub const DEFAULT_REMOTE: &str = "origin";

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

/// Return whether two git references point to the same commit.
pub(crate) fn shas_match(ref1: &str, ref2: &str) -> bool {
    match (run_git(&["rev-parse", ref1]), run_git(&["rev-parse", ref2])) {
        (Ok(output1), Ok(output2)) => !output1.is_empty() && output1.output() == output2.output(),
        _ => false,
    }
}

/// Run a git command and return the output. If the git command fails, this will return an error.
pub(crate) fn run_git_passthrough(args: &[&str]) -> Result<ExitStatus> {
    tracing::debug!("Running `git {}`", args.join(" "));
    let mut child = Command::new("git").args(args).spawn()?;
    Ok(child.wait()?)
}

/// Run a git command and return the output. If the git command fails, this will return an error.
pub(crate) fn run_git(args: &[&str]) -> Result<GitOutput> {
    tracing::debug!("Running `git {}`", args.join(" "));
    let out = Command::new("git")
        .args(args)
        .stderr(std::process::Stdio::piped())
        .output()
        .with_context(|| format!("running git {args:?}"))?;
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
    tracing::debug!("Running `git {}`", args.join(" "));
    if let Some(stdin_text) = stdin {
        let mut child = Command::new("git")
            .args(args)
            .stdin(std::process::Stdio::piped())
            .spawn()?;
        let stdin = child.stdin.as_mut().context("Failed to open stdin")?;
        std::io::Write::write_all(stdin, stdin_text.as_bytes())?;
        Ok(child.wait()?)
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
        Ok(output.status)
    }
}

pub(crate) fn git_fetch() -> Result<()> {
    let _ = run_git(&["fetch", "--prune"])?;
    Ok(())
}

pub(crate) fn git_branch_exists(branch: &str) -> bool {
    run_git(&["rev-parse", "--verify", branch]).is_ok_and(|out| !out.is_empty())
}

#[derive(Debug)]
pub(crate) struct UpstreamStatus {
    pub(crate) symbolic_name: String,
    pub(crate) synced: bool,
}

#[derive(Debug)]
pub(crate) struct GitBranchStatus {
    pub(crate) sha: String,
    pub(crate) exists: bool,
    pub(crate) is_descendent: bool,
    pub(crate) parent_branch: String,
    pub(crate) upstream_status: Option<UpstreamStatus>,
}

pub(crate) fn git_sha(branch: &str) -> Result<String> {
    run_git(&["rev-parse", branch])?.output_or("No sha found")
}

pub(crate) fn git_branch_status(
    parent_branch: Option<&str>,
    branch: &str,
) -> Result<GitBranchStatus> {
    let exists = git_branch_exists(branch);
    let parent_branch = match parent_branch {
        Some(parent_branch) => parent_branch.to_string(),
        None => git_remote_main(DEFAULT_REMOTE)?,
    };
    let is_descendent = exists && is_ancestor(&parent_branch, branch)?;
    let upstream_symbolic_name = git_get_upstream(branch);
    let upstream_synced = upstream_symbolic_name
        .as_ref()
        .is_some_and(|upstream| shas_match(upstream, branch));
    Ok(GitBranchStatus {
        sha: git_sha(branch)?,
        parent_branch,
        exists,
        is_descendent,
        upstream_status: upstream_symbolic_name.map(|symbolic_name| UpstreamStatus {
            symbolic_name,
            synced: upstream_synced,
        }),
    })
}
pub(crate) fn is_ancestor(parent: &str, branch: &str) -> Result<bool> {
    Ok(run_git_status(&["merge-base", "--is-ancestor", parent, branch], None)?.success())
}
pub(crate) fn run_git_status_clean() -> Result<bool> {
    Ok(run_git(&["status", "--porcelain"])?.is_empty())
}

pub(crate) fn after_text(s: &str, needle: impl AsRef<str>) -> Option<&str> {
    let needle = needle.as_ref();
    s.find(needle)
        .map(|pos| &s[pos + needle.chars().fold(0, |x, y| x + y.len_utf8())..])
}

pub(crate) fn git_checkout_main(new_branch: Option<&str>) -> Result<()> {
    if !run_git_status_clean()? {
        bail!("git status is not clean, please commit or stash your changes.")
    }
    git_fetch()?;
    // Assuming the dominant remote is "origin".
    // TODO: add support for different remotes.
    let remote = DEFAULT_REMOTE;
    let trunk = git_trunk()?;

    // Check that we don't orphan unpushed changes in the local `main` branch.
    if !is_ancestor(&trunk.main_branch, &trunk.remote_main)? {
        bail!("It looks like this would orphan unpushed changes in your main branch! Aborting...");
    }

    // Repoint "main" to the remote main branch.
    run_git(&[
        "branch",
        "-f",
        &trunk.main_branch,
        &format!("{}/{}", remote, trunk.main_branch),
    ])?;
    if let Some(new_branch) = new_branch {
        run_git(&["checkout", "-B", new_branch, &trunk.remote_main])?;
    }
    Ok(())
}

pub(crate) struct GitTrunk {
    pub(crate) remote_main: String,
    pub(crate) main_branch: String,
}

pub(crate) fn git_trunk() -> Result<GitTrunk> {
    let remote_main = git_remote_main(DEFAULT_REMOTE)?;
    let main_branch = after_text(&remote_main, format!("{DEFAULT_REMOTE}/"))
        .ok_or(anyhow!("no branch?"))?
        .to_string();
    Ok(GitTrunk {
        remote_main,
        main_branch,
    })
}
/// Returns a string of the form "origin/main".
pub(crate) fn git_remote_main(remote: &str) -> Result<String> {
    run_git(&["symbolic-ref", &format!("refs/remotes/{}/HEAD", remote)])?
        .output()
        .map(|s| s.trim().to_string())
        .ok_or(anyhow!("No remote main branch?"))
        .and_then(|s| {
            Ok(after_text(s.trim(), "refs/remotes/")
                .ok_or(anyhow!("no refs/remotes/ prefix?"))?
                .to_string())
        })
}

pub(crate) fn git_get_upstream(branch: &str) -> Option<String> {
    run_git(&[
        "rev-parse",
        "--abbrev-ref",
        "--symbolic-full-name",
        &format!("{branch}@{{upstream}}"),
    ])
    .ok()
    .and_then(|s| s.output())
}
