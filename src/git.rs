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

/// Raw captured output from a git command, returned regardless of exit status
/// (no bail). Lets callers inspect stderr and the status directly.
struct RawGitOutput {
    status: ExitStatus,
    stdout: String,
    stderr: String,
}

/// Run a git command and capture stdout/stderr/status without bailing on
/// failure. Used by callers (e.g. the fetch path) that need to react to the
/// raw stderr instead of a pre-formatted error message.
fn run_git_capture(args: &[&str]) -> Result<RawGitOutput> {
    let start = Instant::now();
    tracing::debug!("Running `git {}`", args.join(" "));
    let out = Command::new("git")
        .args(args)
        .stderr(std::process::Stdio::piped())
        .output()
        .with_context(|| format!("running git {args:?}"))?;
    record_git_command(args, start.elapsed());
    if !out.status.success() {
        tracing::debug!(?args, "git error: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(RawGitOutput {
        status: out.status,
        stdout: String::from_utf8_lossy(&out.stdout).to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
    })
}

/// Convert a failed git invocation into an error, routing stale-lock contention
/// to an actionable hint and everything else to a generic exit-status message.
fn git_failure_error(args: &[&str], stderr: &str, status: ExitStatus) -> anyhow::Error {
    if is_ref_lock_contention(stderr) {
        anyhow!(
            "`git {}` failed: could not lock a git ref.\n{}\n\
             Another git process may be running, or a previous one was interrupted \
             and left a stale lock.\n\
             If nothing else is using this repo, clear stale locks with:\n    \
             find .git -name '*.lock' -delete\n\
             then try again.",
            args.join(" "),
            stderr.trim(),
        )
    } else {
        anyhow!(
            "`git {}` failed with exit status: {}",
            args.join(" "),
            status
        )
    }
}

/// Run a git command and return the output. If the git command fails, this will return an error.
pub(crate) fn run_git(args: &[&str]) -> Result<GitOutput> {
    let out = run_git_capture(args)?;
    if !out.status.success() {
        return Err(git_failure_error(args, &out.stderr, out.status));
    }
    Ok(GitOutput {
        stdout: out.stdout.trim().to_string(),
    })
}

/// Detect git's "cannot lock ref" / stale-lock failure in stderr so we can give
/// the user an actionable hint instead of a raw non-zero exit status.
fn is_ref_lock_contention(stderr: &str) -> bool {
    stderr.contains("cannot lock ref")
        || (stderr.contains("Unable to create") && stderr.contains(".lock"))
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
    fetch_with_recovery(&["fetch", "--prune"])
}

/// Run a fetch, and if it fails specifically because of a case-insensitive
/// remote-ref collision, attempt to self-heal (delete stale twins) and retry
/// once. The happy path runs the fetch and returns immediately — no extra ref
/// scan or network call.
pub(crate) fn fetch_with_recovery(args: &[&str]) -> Result<()> {
    let out = run_git_capture(args)?;
    if out.status.success() {
        // HAPPY PATH — zero extra work.
        return Ok(());
    }
    if is_ref_lock_contention(&out.stderr) {
        // Failure path only: O(n) in-memory scan of remote-tracking refs.
        let refs = list_remote_tracking_refs()?;
        let names: Vec<String> = refs.iter().map(|(_, n)| n.clone()).collect();
        if is_case_collision_failure(&out.stderr, &names) {
            return recover_case_collision(args, &refs, &out);
        }
    }
    Err(git_failure_error(args, &out.stderr, out.status))
}

/// List all remote-tracking refs for the default remote as `(oid, refname)`
/// pairs via a single `git for-each-ref`.
fn list_remote_tracking_refs() -> Result<Vec<(String, String)>> {
    let refs_glob = format!("refs/remotes/{DEFAULT_REMOTE}");
    let out = run_git(&[
        "for-each-ref",
        "--format=%(objectname) %(refname)",
        &refs_glob,
    ])?;
    let mut result = Vec::new();
    for line in out.stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((oid, refname)) = line.split_once(' ') {
            result.push((oid.to_string(), refname.to_string()));
        }
    }
    Ok(result)
}

/// Parse the ref names git reported it could not lock from `cannot lock ref
/// '...'`. Handles the name being wrapped onto the next line. Keeps only
/// `refs/...` names.
fn parse_locked_refs(stderr: &str) -> Vec<String> {
    const NEEDLE: &str = "cannot lock ref";
    let mut refs = Vec::new();
    let mut rest = stderr;
    while let Some(pos) = rest.find(NEEDLE) {
        // Everything after the needle, potentially spanning newlines, up to the
        // closing quote of the quoted ref name.
        let after = &rest[pos + NEEDLE.len()..];
        if let Some(open) = after.find('\'') {
            let quoted = &after[open + 1..];
            if let Some(close) = quoted.find('\'') {
                let name = quoted[..close].trim();
                if name.starts_with("refs/") {
                    refs.push(name.to_string());
                }
                rest = &quoted[close + 1..];
                continue;
            }
        }
        rest = after;
    }
    refs
}

/// Group ref names that are equal ignoring ASCII case; return only groups with
/// more than one member (collisions), preserving original names. Keyed by
/// `to_ascii_lowercase`.
fn collision_groups(refs: &[String]) -> Vec<Vec<String>> {
    use std::collections::BTreeMap;
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for name in refs {
        groups
            .entry(name.to_ascii_lowercase())
            .or_default()
            .push(name.clone());
    }
    groups
        .into_values()
        .filter(|group| group.len() > 1)
        .collect()
}

/// True iff a ref git failed to lock has a case-twin in `refs` (case-collision),
/// as opposed to a genuine stale lock (no twin). Pure: stderr + ref list only.
fn is_case_collision_failure(stderr: &str, refs: &[String]) -> bool {
    let locked = parse_locked_refs(stderr);
    if locked.is_empty() {
        return false;
    }
    for locked_ref in &locked {
        let lower = locked_ref.to_ascii_lowercase();
        let twins = refs
            .iter()
            .filter(|r| r.to_ascii_lowercase() == lower)
            .count();
        if twins > 1 {
            return true;
        }
    }
    false
}

/// Recover from a confirmed case-collision fetch failure: delete stale twins
/// (branches gone from the remote) via single-ref `update-ref -d` calls, then
/// retry the fetch once. Warns (without deleting) when all twins are still live.
fn recover_case_collision(
    args: &[&str],
    refs: &[(String, String)],
    original: &RawGitOutput,
) -> Result<()> {
    let names: Vec<String> = refs.iter().map(|(_, n)| n.clone()).collect();
    let groups = collision_groups(&names);

    let prefix = format!("refs/remotes/{DEFAULT_REMOTE}/");
    let strip =
        |refname: &str| -> Option<String> { refname.strip_prefix(&prefix).map(|s| s.to_string()) };

    // Collect the branch names of all colliding twins for a single ls-remote.
    let mut branch_query: Vec<String> = Vec::new();
    for group in &groups {
        for refname in group {
            if let Some(branch) = strip(refname) {
                branch_query.push(branch);
            }
        }
    }

    let live = live_remote_branches(&branch_query)?;

    let oid_of = |refname: &str| -> Option<String> {
        refs.iter()
            .find(|(_, n)| n == refname)
            .map(|(oid, _)| oid.clone())
    };

    let mut deleted_any = false;
    for group in &groups {
        // Partition twins into stale (branch absent from remote) and live.
        let (stale, still_live): (Vec<&String>, Vec<&String>) =
            group.iter().partition(|refname| match strip(refname) {
                Some(branch) => !live.contains(&branch),
                // A ref we can't map to a branch is treated as live (don't delete).
                None => false,
            });

        if stale.is_empty() {
            // Both/all twins are live on the remote — unresolvable locally.
            eprintln!(
                "warning: remote refs differ only in case and both exist on the remote:\n    {}\n\
                 On a case-insensitive filesystem these map to the same path, so `git fetch` \
                 cannot lock them.\n\
                 git-stack cannot resolve this automatically. The only fix is to rename one of \
                 these branches upstream (e.g. via org policy or a server-side pre-receive hook); \
                 there is no GitHub-side toggle to forbid case-only branch names.",
                still_live
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join("\n    "),
            );
            continue;
        }

        for refname in stale {
            let Some(oid) = oid_of(refname) else {
                continue;
            };
            // Single-ref transaction dodges the case collision.
            match run_git(&["update-ref", "-d", refname, &oid]) {
                Ok(_) => {
                    deleted_any = true;
                    println!("Pruned stale remote-tracking ref '{refname}' (case-collision).");
                }
                Err(e) => {
                    eprintln!("warning: failed to prune stale ref '{refname}': {e}");
                }
            }
        }
    }

    if deleted_any {
        // Retry the fetch exactly once — no recursion, cannot loop.
        let retry = run_git_capture(args)?;
        if retry.status.success() {
            return Ok(());
        }
        return Err(git_failure_error(args, &retry.stderr, retry.status));
    }

    // Nothing deleted (every group was both-live); the warning is already
    // printed, so surface the original failure rather than swallow it.
    Err(git_failure_error(args, &original.stderr, original.status))
}

/// Query which of the given branch names still exist on the default remote via
/// a single `git ls-remote --heads` scoped to that small set (never a full
/// remote enumeration). Returns the set of live branch names.
fn live_remote_branches(branches: &[String]) -> Result<std::collections::HashSet<String>> {
    use std::collections::HashSet;
    if branches.is_empty() {
        return Ok(HashSet::new());
    }
    let mut args: Vec<&str> = vec!["ls-remote", "--heads", DEFAULT_REMOTE];
    for b in branches {
        args.push(b.as_str());
    }
    let out = run_git(&args)?;
    let mut live = HashSet::new();
    for line in out.stdout.lines() {
        // Each line: "<oid>\trefs/heads/<branch>"
        if let Some((_, refname)) = line.split_once('\t')
            && let Some(branch) = refname.trim().strip_prefix("refs/heads/")
        {
            live.insert(branch.to_string());
        }
    }
    Ok(live)
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
    let trunk = git_trunk(repo).ok_or_else(|| anyhow!("No remote configured"))?;

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

pub(crate) fn git_trunk(git_repo: &GitRepo) -> Option<GitTrunk> {
    let remote_main = git_repo.remote_main(DEFAULT_REMOTE).ok()?;
    let main_branch = after_text(&remote_main, format!("{DEFAULT_REMOTE}/"))?.to_string();
    Some(GitTrunk {
        remote_main,
        main_branch,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_cannot_lock_ref_error() {
        // The exact shape git emits when a ref lock is contended or stale.
        let stderr = "error: could not delete references: cannot lock ref \
            'refs/remotes/origin/foo': Unable to create \
            '/repo/.git/refs/remotes/origin/foo.lock': File exists.";
        assert!(is_ref_lock_contention(stderr));
    }

    #[test]
    fn ignores_unrelated_errors() {
        assert!(!is_ref_lock_contention(
            "fatal: couldn't find remote ref refs/heads/nope"
        ));
    }

    #[test]
    fn collision_groups_finds_case_twins() {
        let refs = vec![
            "refs/remotes/origin/Jacob/x".to_string(),
            "refs/remotes/origin/jacob/x".to_string(),
            "refs/remotes/origin/unique".to_string(),
        ];
        let groups = collision_groups(&refs);
        assert_eq!(groups.len(), 1);
        let mut group = groups[0].clone();
        group.sort();
        assert_eq!(
            group,
            vec![
                "refs/remotes/origin/Jacob/x".to_string(),
                "refs/remotes/origin/jacob/x".to_string(),
            ]
        );
        // 'unique' is excluded from any collision group.
        assert!(!groups.iter().flatten().any(|r| r.contains("unique")));
    }

    #[test]
    fn collision_groups_empty_when_all_distinct() {
        let refs = vec![
            "refs/remotes/origin/foo".to_string(),
            "refs/remotes/origin/bar".to_string(),
            "refs/remotes/origin/baz".to_string(),
        ];
        assert!(collision_groups(&refs).is_empty());
    }

    #[test]
    fn parse_locked_refs_handles_wrapped_name() {
        // git wraps the ref name onto the next line.
        let stderr = "error: could not delete references: cannot lock ref\n\
            'refs/remotes/origin/jacob/slack-sprint-v1-run-continuation':\n\
            Unable to create '.../slack-sprint-v1-run-continuation.lock': File exists.";
        assert_eq!(
            parse_locked_refs(stderr),
            vec!["refs/remotes/origin/jacob/slack-sprint-v1-run-continuation".to_string()]
        );
    }

    #[test]
    fn is_case_collision_failure_true_with_twin() {
        let stderr = "error: could not delete references: cannot lock ref\n\
            'refs/remotes/origin/jacob/slack-sprint-v1-run-continuation':\n\
            Unable to create '.../slack-sprint-v1-run-continuation.lock': File exists.";
        let refs = vec![
            "refs/remotes/origin/Jacob/slack-sprint-v1-run-continuation".to_string(),
            "refs/remotes/origin/jacob/slack-sprint-v1-run-continuation".to_string(),
            "refs/remotes/origin/other".to_string(),
        ];
        assert!(is_case_collision_failure(stderr, &refs));
    }

    #[test]
    fn is_case_collision_failure_false_for_genuine_stale_lock() {
        // No case-twin of 'foo' → genuine stale lock, not a collision.
        let stderr = "error: cannot lock ref 'refs/heads/foo': Unable to create \
            '/repo/.git/refs/heads/foo.lock': File exists.";
        let refs = vec!["refs/heads/foo".to_string(), "refs/heads/bar".to_string()];
        assert!(!is_case_collision_failure(stderr, &refs));
    }
}
