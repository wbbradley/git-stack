use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::env;
use std::process::Command;

const GIT_ROOT: &str = "/home/youruser/src/walrus"; // change as needed
const STACK: [&str; 2] = ["my-branch-01", "my-branch-02"];

#[derive(Parser)]
#[command(version, about)]
struct Args {
    #[arg(long, short)]
    verbose: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    Rollback {
        #[arg(short, long)]
        version: String,
    },
}

fn run_git(args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .output()
        .with_context(|| format!("running git {args:?}"))?;

    if !out.status.success() {
        bail!("git {:?} failed with exit status: {}", args, out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn run_git_ok(args: &[&str]) -> Result<bool> {
    Ok(Command::new("git")
        .args(args)
        // .stdout(Stdio::null())
        // .stderr(Stdio::null())
        .status()?
        .success())
}

fn run_git_status_clean() -> Result<bool> {
    Ok(run_git(&["status", "--porcelain"])
        .context("run_git_status_clean")?
        .is_empty())
}

fn git_fetch() -> Result<()> {
    if !run_git_ok(&["fetch", "--prune"]).context("git_fetch")? {
        bail!("git fetch failed")
    }
    Ok(())
}

fn git_checkout_main() -> Result<()> {
    if !run_git_status_clean()? {
        bail!("git status is not clean, please commit or stash your changes.")
    }
    let remote = "origin";
    let main = run_git(&["symbolic-ref", &format!("refs/remotes/{}/HEAD", remote)])?;
    let main_branch = main.trim().rsplit('/').next().expect("main branch");
    if !run_git_ok(&[
        "checkout",
        "-B",
        main_branch,
        &format!("{}/{}", remote, main_branch),
    ])
    .with_context(|| format!("git checkout {} failed", main_branch))?
    {
        bail!("git checkout {} failed", main_branch)
    }
    Ok(())
}

fn main() -> Result<()> {
    let args = Args::parse();
    if let Err(e) = env::set_current_dir(GIT_ROOT) {
        bail!("cd {} failed: {}", GIT_ROOT, e);
    }
    git_fetch()?;

    let run_version = format!("{}", chrono::Utc::now().timestamp());
    let orig_branch = run_git(&["rev-parse", "--abbrev-ref", "HEAD"]).context("get orig_branch")?;
    tracing::info!("Starting branch: {}", orig_branch);

    // --verbose: Print current stack and exit.
    if args.verbose {
        for &branch in &STACK {
            let source = run_git(&["rev-parse", branch]).context("get source")?;
            if source.is_empty() {
                bail!("branch '{}' does not exist", branch);
            }
            let log_msg = run_git(&["log", "-1", "--pretty=format:%s", &source])
                .context("get log message")?;
            if log_msg.is_empty() {
                bail!("branch '{}' has no commit message!?", branch);
            }
            tracing::info!("{}: {}", branch, log_msg);
        }
        std::process::exit(0);
    }

    tracing::info!("This is git-stack run version {}.", run_version);
    git_checkout_main()?;
    let mut stack_on = "main".to_string();

    // --rollback mode
    match args.command {
        Some(Commands::Rollback {
            version: run_version,
        }) => {
            tracing::info!("Rolling back to version {}...", run_version);
            for &branch in &STACK {
                let backup_branch = if run_version == "origin" {
                    format!("origin/{}", branch)
                } else {
                    format!("{}-at-{}", branch, run_version)
                };
                let source = run_git(&["rev-parse", &backup_branch]).context("get source")?;
                if source.is_empty() {
                    tracing::warn!("branch '{}' does not exist, skipping...", backup_branch);
                    continue;
                }
                tracing::info!("Rolling back branch '{}' to '{}'...", branch, backup_branch);
                if !run_git_ok(&["branch", "-f", branch, &source])? {
                    bail!("git branch -f {} {} failed", branch, source);
                }
                if run_version != "origin" && !run_git_ok(&["branch", "-D", &backup_branch])? {
                    bail!("git branch -D {} failed", backup_branch);
                }
            }
        }
        None => todo!(),
    }

    for &branch in &STACK {
        let source = run_git(&["rev-parse", branch])?;
        if source.is_empty() {
            bail!("branch '{}' does not exist", branch);
        }
        let log_msg = run_git(&["log", "-1", "--pretty=format:%s", &source])?;
        if log_msg.is_empty() {
            bail!("branch '{}' has no commit message!?", branch);
        }

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
