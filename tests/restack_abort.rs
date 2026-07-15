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

#[test]
fn aborting_conflicted_am_restores_head_and_branch_ref() {
    let repo_dir = tempfile::tempdir().unwrap();
    let repo = repo_dir.path();
    git(repo, &["init", "-q", "-b", "main"]);
    git(repo, &["config", "user.email", "test@example.com"]);
    git(repo, &["config", "user.name", "Test"]);
    git(repo, &["config", "maintenance.auto", "false"]);
    git(repo, &["config", "gc.auto", "0"]);

    fs::write(repo.join("shared.txt"), "base\n").unwrap();
    git(repo, &["add", "shared.txt"]);
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

    git(repo, &["checkout", "-q", "-b", "feature"]);
    fs::write(repo.join("shared.txt"), "feature\n").unwrap();
    git(repo, &["commit", "-q", "-am", "feature work"]);
    let original_feature_sha = git_output(repo, &["rev-parse", "feature"]);
    let patch = Command::new("git")
        .args(["format-patch", "-1", "--stdout", "feature"])
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(patch.status.success());
    let patch_path = repo.join("feature.patch");
    fs::write(&patch_path, patch.stdout).unwrap();

    git(repo, &["checkout", "-q", "main"]);
    fs::write(repo.join("shared.txt"), "main\n").unwrap();
    git(repo, &["commit", "-q", "-am", "main work"]);
    let main_sha = git_output(repo, &["rev-parse", "main"]);
    git(repo, &["checkout", "-q", "-B", "feature", "main"]);
    let am_output = Command::new("git")
        .args(["am", patch_path.to_str().unwrap()])
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(
        !am_output.status.success(),
        "the fixture must enter an am conflict"
    );

    let state_home = tempfile::tempdir().unwrap();
    let state_dir = state_home.path().join("git-stack");
    fs::create_dir_all(&state_dir).unwrap();
    let canonical_repo = repo.canonicalize().unwrap();
    let state = format!(
        "{}:\n  name: main\n  stack_method: apply_merge\n  lkg_parent: null\n  branches:\n  - name: feature\n    stack_method: apply_merge\n    lkg_parent: {}\n    branches: []\n  pending_restack:\n    method: am\n    branch_name: feature\n    parent: main\n    original_sha: {}\n    resume:\n      restack_branch: feature\n      orig_branch: feature\n      ancestors: false\n      push: false\n      squash: false\n",
        canonical_repo.display(),
        root_sha,
        original_feature_sha
    );
    fs::write(state_dir.join("state.yaml"), state).unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_git-stack"))
        .args(["restack", "--abort"])
        .current_dir(repo)
        .env("XDG_STATE_HOME", state_home.path())
        .env("XDG_CONFIG_HOME", state_home.path().join("config"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "abort failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(
        git_output(repo, &["rev-parse", "HEAD"]),
        original_feature_sha
    );
    assert_eq!(
        git_output(repo, &["rev-parse", "refs/heads/feature"]),
        original_feature_sha
    );
    assert_eq!(git_output(repo, &["branch", "--show-current"]), "feature");
    assert!(!repo.join(".git/rebase-apply").exists());

    let saved_state = fs::read_to_string(state_dir.join("state.yaml")).unwrap();
    assert!(!saved_state.contains("pending_restack:"));
    assert_ne!(git_output(repo, &["rev-parse", "HEAD"]), main_sha);
}
