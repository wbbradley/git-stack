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
- Resolve rebase conflicts with standard git commands (`git mergetool`, then `git rebase --continue`)
- Rerun `git stack restack` after resolving conflicts

## License

[GPL2](LICENSE)
