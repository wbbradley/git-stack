# git-stack

A command-line tool for managing stacked git branches. Develop features on top of one another, keep history clean, and sync with GitHub.

## Installation

```bash
cargo install --git https://github.com/wbbradley/git-stack --locked
```

## Quick Start

```bash
git stack                    # show your stack
git stack sync               # sync local state with GitHub (push + pull)
git stack checkout feature   # create branch "feature" as child of current branch
# ...make changes, commit...
git stack restack            # restack the current branch onto its parent
git stack diff               # diff against parent branch
git stack pr create          # create GitHub PR with correct base branch
```

## Commands

### View Your Stack

```bash
git stack                    # show the stack tree (alias: git stack status)
```

### Create Branches

```bash
git stack checkout feature   # create "feature" stacked on current branch
```

### Restack Branches

```bash
git stack restack            # restack current branch onto its parent
git stack restack -afp       # fetch, recursively restack from trunk, push on success
```

The `-afp` flags:
- `-a` / `--ancestors`: recursively restack all ancestors from trunk up to current branch
- `-f` / `--fetch`: fetch updates from remote first
- `-p` / `--push`: push branch updates to remote on success

### Diff Against Parent

```bash
git stack diff               # diff against parent branch
```

### Create Pull Requests

```bash
git stack pr create          # create GitHub PR with correct base branch
```

### Change Parent Branch

```bash
git stack mount <parent>     # stack current branch on a different parent
```

This only updates git-stack metadata, not git history. Use `restack` afterward to keep this branch
in sync with its parent.

### Delete Branches

```bash
git stack delete <branch>    # remove a branch from the stack
```

Note that `git stack sync` will automatically prune local branches that are duplicates of the remote
branch, or have already been merged.

## Authentication

Commands that talk to GitHub (`sync`, `pr create`) need a token. Set one up with:

```bash
git stack auth login         # interactive OAuth device flow (recommended)
git stack auth login --pat   # paste a personal access token instead
git stack auth status        # show the active auth method
git stack auth logout        # clear git-stack's stored tokens
```

git-stack resolves a token from the first source that provides one, in order:

1. `GITHUB_TOKEN` environment variable
2. `GH_TOKEN` environment variable
3. `git config --get github.token`
4. Config file (`~/.config/git-stack/github.yaml`): host-specific token, then PAT, then OAuth token
5. The [`gh` CLI](https://cli.github.com/): if none of the above resolve and you've run `gh auth
   login`, git-stack borrows `gh`'s token automatically (via `gh auth token`). Use `gh auth logout`
   to sign out of `gh`.

## Filtering by author

By default, `git stack status` and the interactive TUI filter the tree to **your own GitHub
login** — branches whose PR was authored by someone else are hidden, so a shared repo shows just
your stacks. The current branch, its ancestor chain up to trunk, the trunk itself, and any branch
whose author can't be determined (e.g. local work with no PR yet) always stay visible.

The same effective filter also governs two non-display behaviors, so they stay consistent with
what `status` shows:

- `git stack cleanup` **prunes** out-of-scope branches from the stack tree (it prompts for
  confirmation before saving, and refuses to prune on a non-interactive terminal).
- `git stack sync` skips **injecting** remote-only PR branches authored by others.
- `git stack sync` also **discovers** the filtered authors' open PRs and auto-mounts them, so
  running it from a trunk-only tree reconstructs and mounts your stacks (skipped under `--push`
  and when `authors_filter: []` is set).

Override the default with an `authors_filter` key in `~/.config/git-stack/github.yaml`:

```yaml
# Filter to specific authors (yourself and a collaborator):
authors_filter: [octocat, hubber]

# Show everyone's branches (filtering off):
authors_filter: []
```

- **Key absent** (the default) → filter to your own login.
- **`authors_filter: []`** → show everyone.
- **`authors_filter: [a, b]`** → show exactly those authors (plus the always-visible protected
  branches above).

To show everything for a single invocation without editing config, pass `--show-all`.

Deriving the default requires knowing your GitHub login. git-stack looks it up once via `GET /user`
and caches it (keyed by host) in its local state, refreshing it on `git stack auth login` and
`git stack sync`. If the filter is unset **and** there's no cached login **and** git-stack can't
fetch one right now (offline and token-less), `status` prints an actionable error rather than
guessing — set a token (`git stack auth login`, or `GITHUB_TOKEN`/`GH_TOKEN`), set `authors_filter`
explicitly, or use `authors_filter: []` / `--show-all` to show everyone.

## Workflow Example

```bash
# Start on main
git stack checkout auth      # create auth branch
# ...implement auth, commit...

git stack checkout login     # create login branch (child of auth)
# ...implement login, commit...

git stack                    # see your stack tree
git stack restack -afp       # sync everything and push
git stack pr create          # create PR for current branch
```

## Stack Storage

Stack state is stored per-repo in `~/.local/state/git-stack/state.yaml`.

## Troubleshooting

If `git stack` reports issues:

- Ensure your working tree is clean (`git status`)
- On a conflict, restack pauses and records a recovery point. Resolve the conflict
  (`git mergetool`), `git add` the resolved files, then run `git stack restack --continue`
  to finish the branch and resume the rest of the stack. If the conflicting patch resolved to
  nothing — its changes are already present, e.g. a superseded or duplicated commit — run
  `git stack restack --skip` to drop that patch and continue. Or run `git stack restack --abort`
  to restore the conflicting branch to its original state. This works for every stack method;
  `--continue`/`--skip` also tolerate an `am`/`rebase` you finished by hand, and `--abort`
  recovers even if you already ran a bare `git am --abort` / `git rebase --abort`.

## License

[GPL2](LICENSE)
