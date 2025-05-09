use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};

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
            Some(self.stdout)
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

pub(crate) fn run_git(args: &[&str]) -> Result<GitOutput> {
    tracing::debug!("Running `git {}`", args.join(" "));
    let out = Command::new("git")
        .args(args)
        .output()
        .with_context(|| format!("running git {args:?}"))?;

    if !out.status.success() {
        bail!("git {:?} failed with exit status: {}", args, out.status);
    }
    Ok(GitOutput {
        stdout: String::from_utf8_lossy(&out.stdout).trim().to_string(),
    })
}

pub(crate) fn run_git_ok(args: &[&str]) -> Result<bool> {
    tracing::debug!("Running `git {}`", args.join(" "));
    Ok(Command::new("git").args(args).status()?.success())
}

pub(crate) fn git_fetch() -> Result<()> {
    let _ = run_git(&["fetch", "--prune"])?;
    Ok(())
}

pub(crate) fn run_git_status_clean() -> Result<bool> {
    Ok(run_git(&["status", "--porcelain"])?.is_empty())
}

pub(crate) fn after_text(s: &str, needle: impl AsRef<str>) -> Option<&str> {
    let needle = needle.as_ref();
    s.find(needle)
        .map(|pos| &s[pos + needle.chars().fold(0, |x, y| x + y.len_utf8())..])
}

pub(crate) fn git_checkout_main() -> Result<()> {
    if !run_git_status_clean()? {
        bail!("git status is not clean, please commit or stash your changes.")
    }
    git_fetch()?;
    let remote = "origin";
    let remote_main =
        run_git(&["symbolic-ref", &format!("refs/remotes/{}/HEAD", remote)])?.output();
    let main_branch = after_text(
        &remote_main.ok_or(anyhow!("No remote main branch?"))?,
        format!("{remote}/"),
    )
    .ok_or(anyhow!("no branch?"))?
    .to_string();
    if !run_git_ok(&[
        "checkout",
        "-B",
        &main_branch,
        &format!("{}/{}", remote, main_branch),
    ])
    .with_context(|| format!("git checkout {} failed", main_branch))?
    {
        bail!("git checkout {} failed", main_branch)
    }
    Ok(())
}
