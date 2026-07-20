use std::{fs, path::Path, process::Command};

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed with {status}");
}

fn git_output(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?} failed: {output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

/// Build a repo with a `main` branch (plus an `origin/main` remote-tracking ref
/// and `origin/HEAD`, so git-stack can resolve trunk) and a second branch
/// `feature`, leaving HEAD on `main`.
fn init_repo(repo: &Path) {
    git(repo, &["init", "-q", "-b", "main"]);
    git(repo, &["config", "user.email", "test@example.com"]);
    git(repo, &["config", "user.name", "Test"]);
    git(repo, &["config", "maintenance.auto", "false"]);
    git(repo, &["config", "gc.auto", "0"]);

    fs::write(repo.join("file.txt"), "base\n").unwrap();
    git(repo, &["add", "file.txt"]);
    git(repo, &["commit", "-q", "-m", "root"]);
    let root_sha = git_output(repo, &["rev-parse", "HEAD"]);
    git(repo, &["update-ref", "refs/remotes/origin/main", &root_sha]);
    git(
        repo,
        &[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ],
    );

    git(repo, &["branch", "feature", "main"]);
}

fn run_git_stack(repo: &Path, state_home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_git-stack"))
        .args(args)
        .current_dir(repo)
        .env("XDG_STATE_HOME", state_home)
        .env("XDG_CONFIG_HOME", state_home.join("config"))
        .output()
        .unwrap()
}

#[test]
fn checkout_branch_used_by_another_worktree_explains_conflict() {
    let repo_dir = tempfile::tempdir().unwrap();
    let repo = repo_dir.path();
    init_repo(repo);

    // Check out `feature` in a linked worktree so the main repo cannot.
    let wt_parent = tempfile::tempdir().unwrap();
    let wt_path = wt_parent.path().join("feature-wt");
    git(
        repo,
        &["worktree", "add", wt_path.to_str().unwrap(), "feature"],
    );

    let state_home = tempfile::tempdir().unwrap();
    let output = run_git_stack(repo, state_home.path(), &["checkout", "feature"]);

    assert!(
        !output.status.success(),
        "checkout should fail while feature is held by another worktree:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    // git-stack surfaces errors via `tracing`, whose fmt layer writes to stdout;
    // check both streams so the assertions don't depend on that detail.
    let out = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(
        out.contains("another worktree"),
        "message should explain the worktree conflict, got:\n{out}"
    );
    assert!(
        out.contains("feature"),
        "message should name the branch, got:\n{out}"
    );
    assert!(
        out.contains("feature-wt"),
        "message should name the worktree path, got:\n{out}"
    );

    // The main worktree must remain on `main` (the checkout never happened).
    assert_eq!(git_output(repo, &["branch", "--show-current"]), "main");
}

#[test]
fn checkout_branch_in_current_worktree_succeeds() {
    let repo_dir = tempfile::tempdir().unwrap();
    let repo = repo_dir.path();
    init_repo(repo);

    // Already on `feature` in this (the only) worktree: checking it out again is
    // a valid no-op and must not trigger the worktree-conflict guard.
    git(repo, &["checkout", "-q", "feature"]);

    let state_home = tempfile::tempdir().unwrap();
    let output = run_git_stack(repo, state_home.path(), &["checkout", "feature"]);

    assert!(
        output.status.success(),
        "checking out the current branch should succeed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert_eq!(git_output(repo, &["branch", "--show-current"]), "feature");
}
