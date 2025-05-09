use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use state::Stack;
use std::env;
use std::process::Command;

mod state;

#[derive(Parser)]
#[command(author, version, about)]
struct Args {
    #[arg(long, short, help = "Enable verbose output")]
    verbose: bool,

    /// Subcommand to run.
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// List all stacks in the given directory.
    List,
    Rollback {
        #[arg(short, long)]
        version: String,
    },
}

pub struct GitOutput {
    stdout: String,
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

fn run_git(args: &[&str]) -> Result<GitOutput> {
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

fn run_git_ok(args: &[&str]) -> Result<bool> {
    tracing::debug!("Running `git {}`", args.join(" "));
    Ok(Command::new("git").args(args).status()?.success())
}

fn run_git_status_clean() -> Result<bool> {
    Ok(run_git(&["status", "--porcelain"])?.is_empty())
}

fn after_text(s: &str, needle: impl AsRef<str>) -> Option<&str> {
    let needle = needle.as_ref();
    s.find(needle)
        .map(|pos| &s[pos + needle.chars().fold(0, |x, y| x + y.len_utf8())..])
}

fn git_checkout_main() -> Result<()> {
    if !run_git_status_clean()? {
        bail!("git status is not clean, please commit or stash your changes.")
    }
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

fn main() {
    if let Err(e) = inner_main() {
        tracing::error!(error = ?e);
        std::process::exit(1);
    }
    std::process::exit(0);
}

fn inner_main() -> Result<()> {
    // Run from the git root directory.
    let args = Args::parse();

    // construct a subscriber that prints formatted traces to stdout
    let subscriber = tracing_subscriber::FmtSubscriber::new();
    // use that subscriber to process traces emitted after this point
    tracing::subscriber::set_global_default(subscriber)?;

    let dir_key = std::fs::canonicalize(
        run_git(&["rev-parse", "--show-toplevel"])?.output_or("No git directory found")?,
    )?
    .into_os_string()
    .into_string()
    .map_err(|error| anyhow!("Invalid git directory: '{}'", error.to_string_lossy()))?;

    let state = state::load_state().context("loading state")?;

    tracing::debug!("Current directory: {}", dir_key);

    let stacks: Vec<Stack> = state
        .directories
        .get(&dir_key)
        .map(|dir| dir.stacks.to_vec())
        .unwrap_or(vec![]);

    let run_version = format!("{}", chrono::Utc::now().timestamp());
    let orig_branch = run_git(&["rev-parse", "--abbrev-ref", "HEAD"])?
        .output()
        .ok_or(anyhow!("No current branch?"))?;

    // --verbose: Print current stack and exit.
    if args.verbose {
        let stack = stacks.first().ok_or(anyhow!("No stacks found"))?;

        for branch in &stack.branches {
            let source = run_git(&["rev-parse", branch])?
                .output_or(format!("branch '{}' does not exist", branch))?;
            let log_msg = run_git(&["log", "-1", "--pretty=format:%s", source.as_ref()])?;
            if log_msg.is_empty() {
                bail!("branch '{}' has no commit message!?", branch);
            }
            tracing::info!("{}: {}", branch, log_msg.as_ref());
        }
        std::process::exit(0);
    }

    tracing::debug!("This is git-stack run version {}.", run_version);
    let mut stack_on = "main".to_string();

    // --rollback mode
    match args.command {
        Some(Commands::List) => {
            for stack in &stacks {
                tracing::info!("Stack: {}", stack.branches.join(", "));
            }
            return Ok(());
        }
        Some(Commands::Rollback {
            version: run_version,
        }) => {
            let stack = stacks.first().ok_or(anyhow!("No stacks found"))?;
            tracing::info!("Rolling back to version {}...", run_version);
            for branch in &stack.branches {
                let backup_branch = if run_version == "origin" {
                    format!("origin/{}", branch)
                } else {
                    format!("{}-at-{}", branch, run_version)
                };
                let source = run_git(&["rev-parse", &backup_branch])?
                    .output_or(format!("missing tree from {backup_branch}?"))?;
                tracing::info!("Rolling back branch '{}' to '{}'...", branch, backup_branch);
                if !run_git_ok(&["branch", "-f", branch, &source])? {
                    bail!("git branch -f {} {} failed", branch, source);
                }
                if run_version != "origin" && !run_git_ok(&["branch", "-D", &backup_branch])? {
                    bail!("git branch -D {} failed", backup_branch);
                }
            }
            return Ok(());
        }
        None => {
            // Fallthrough to the default behavior.
        }
    }

    let stack = stacks.first().ok_or(anyhow!("No stacks found"))?;
    git_checkout_main()?;

    // Attempt to get everything stacked up.
    for branch in &stack.branches {
        tracing::info!(
            "Starting branch: {} [pwd={}]",
            orig_branch,
            env::current_dir()?.display()
        );
        let source = run_git(&["rev-parse", branch])?
            .output_or(format!("branch {branch} does not exist?"))?;
        let log_msg = run_git(&["log", "-1", "--pretty=format:%s", &source])?
            .output_or(format!("branch '{}' has no commit message!?", branch))?;

        if run_git_ok(&["merge-base", "--is-ancestor", &stack_on, branch])? {
            tracing::info!(
                "Branch '{}' is already up to date with '{}'.",
                branch,
                stack_on
            );
            stack_on = branch.to_string();
            tracing::info!("Force-pushing '{}' to origin...", branch);
            if !run_git_ok(&["push", "-fu", "origin", &format!("{}:{}", branch, branch)])? {
                bail!("git push -fu origin {}:{} failed", branch, branch);
            }
            continue;
        } else {
            tracing::info!(
                "Branch '{}' is not descended from '{}'...",
                branch,
                stack_on
            );
            let backup_branch = format!("{}-at-{}", branch, run_version);
            tracing::info!(
                "Creating backup branch '{}' from '{}'...",
                backup_branch,
                branch
            );
            if !run_git_ok(&["branch", &backup_branch, &source])? {
                bail!("git branch '{}' failed", backup_branch);
            }
            tracing::info!("Initiating a rebase of '{}' onto '{}'...", branch, stack_on);
            tracing::info!(
                "Note: use `git commit -m '{}'` to commit the changes.",
                log_msg
            );
            if !run_git_ok(&["checkout", branch])? {
                bail!("git checkout {} failed", branch);
            }
            let rebased = run_git_ok(&["rebase", &stack_on]).context("rebase")?;
            if !rebased {
                tracing::warn!("Rebase failed, aborting...");
                tracing::warn!("Run `git mergetool` to resolve conflicts.");
                tracing::info!("Once you have finished the rebase, re-run this script.");
                std::process::exit(1);
            }
            tracing::info!("Rebase completed successfully. Continuing...");
            stack_on = branch.to_string();
        }
    }
    tracing::info!("All branches are up to date with '{}'.", stack_on);
    tracing::info!("Restoring starting branch '{}'...", orig_branch);
    if !run_git_ok(&["checkout", &orig_branch])? {
        bail!("git checkout {} failed", orig_branch);
    }
    tracing::info!("Done.");
    Ok(())
}
