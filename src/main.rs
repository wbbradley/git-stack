use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use state::{Stack, State, load_state};
use std::env;
use std::process::Command;

mod state;

#[derive(Parser)]
#[command(author, version, about, arg_required_else_help = true)]
struct Args {
    #[arg(long, short, help = "Enable verbose output")]
    verbose: bool,

    /// Subcommand to run.
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// List all stacks in the given directory.
    List,
    Status,
    Restack,
    New {
        name: String,
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

fn git_fetch() -> Result<()> {
    let _ = run_git(&["fetch", "--prune"])?;
    Ok(())
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
    let subscriber = tracing_subscriber::FmtSubscriber::new();
    tracing::subscriber::set_global_default(subscriber)?;

    let dir_key = std::fs::canonicalize(
        run_git(&["rev-parse", "--show-toplevel"])?.output_or("No git directory found")?,
    )?
    .into_os_string()
    .into_string()
    .map_err(|error| anyhow!("Invalid git directory: '{}'", error.to_string_lossy()))?;

    let state = load_state().context("loading state")?;

    tracing::debug!("Current directory: {}", dir_key);

    let run_version = format!("{}", chrono::Utc::now().timestamp());
    let orig_branch = run_git(&["rev-parse", "--abbrev-ref", "HEAD"])?
        .output()
        .ok_or(anyhow!("No current branch?"))?;

    // --verbose: Print current stack and exit.
    /*if args.verbose {
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
    }*/

    tracing::debug!("This is git-stack run version {}.", run_version);

    match args.command {
        Commands::List => list_stacks(&state, &dir_key),
        Commands::New { name } => new_stack(state, dir_key, name),
        Commands::Restack => restack(state, &dir_key, run_version, orig_branch),
        Commands::Status => todo!(),
    }
}

fn new_stack(mut _state: State, _dir_key: String, _name: String) -> Result<()> {
    todo!()
}

fn list_stacks(state: &State, dir_key: &str) -> std::result::Result<(), anyhow::Error> {
    let stacks: Vec<Stack> = state.get_stacks(dir_key);

    for stack in stacks {
        tracing::info!("Stack: {}", stack.branches.join(", "));
    }
    Ok(())
}

fn restack(
    state: State,
    dir_key: &str,
    run_version: String,
    starting_branch: String,
) -> Result<(), anyhow::Error> {
    // Find starting_branch in the stacks of branches to determine which stack to use.
    let stack = state
        .get_stacks(dir_key)
        .into_iter()
        .find(|stack| stack.branches.contains(&starting_branch))
        .ok_or(anyhow!("No stack found for branch {}", starting_branch))?;
    git_checkout_main()?;
    let mut stack_on = "main".to_string();
    for branch in &stack.branches {
        tracing::info!(
            "Starting branch: {} [pwd={}]",
            starting_branch,
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
    tracing::info!("Restoring starting branch '{}'...", starting_branch);
    if !run_git_ok(&["checkout", &starting_branch])? {
        bail!("git checkout {} failed", starting_branch);
    }
    tracing::info!("Done.");
    Ok(())
}
