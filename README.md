# git-stack

`git-stack` is a command-line tool for managing stacked git branches — a workflow where you develop
features atop one another, keeping their history clean and rebasing as needed. It tracks
relationships between branches, helps you re-stack and synchronize them, and visualizes your stack
tree.

`git-stack` also allows for synchronization between Github and local state.

## Key Features

- Stacked Workflow: Track parent-child branch relationships ("stacks") in your repo.
- Restack: Automatically rebase your branch stack on top of trunk or another branch.
- Status Tree: Visualize your current stacked branches, their hierarchy, and sync status.
- Branch Mounting: Change the parent branch of an existing branch in your stack.
- Diffs: Show changes between a branch and its parent.
- Safe Operations: Prevent accidental data loss by checking upstream sync status.

## Installation

Build from source (Rust required):

```bash
cargo install --path .
```

This will install a CLI you can use as follows:

```bash
git stack <command> [options]
```

## Usage

You can invoke `git-stack` directly or as a git subcommand:

```bash
git stack <subcommand> [options]
```

### Common Subcommands

- `status`: Show the current stack tree in the repo.
  - _Default_, so `git stack` is equivalent to `git stack status`.

- `checkout <branch>`: Create a new branch stacked on the current branch, or checkout an existing
  one in the stack.

- `restack [--branch <branch>]`: Rebase the named (or current) branch and its descendents onto their
  updated stack base (like trunk or a parent feature branch). This command recursively applies to
  all ancestors of the given branch.

- `mount [<parent-branch>]`: Mounts (attaches) the current branch on a different parent branch.

- `diff [<branch>]`: Show diff between a branch and its stack parent.

- `delete <branch>`: Remove a branch from the stack tree.

### Examples

#### See your stack status

```
git stack status
```

#### Create and stack a new branch

```
git stack checkout my-feature
```

#### Restack your current branch

```
git stack restack
```

#### Change the parent stack of a feature branch

```
git stack mount new-parent-branch
```

#### Show diff with parent

```
git stack diff my-feature
```

#### Remove a branch from stack management

```
git stack delete my-feature
```

## Stack Storage

- Stack state is stored per-repo in a YAML file at:
  `~/.local/state/git-stack/state.yaml` (using XDG state dir conventions).

## Requirements

- A POSIX shell
- git (on your `$PATH`)
- Rust (to install/build)

## Troubleshooting

If `git stack` reports missing branches or refuses to restack:

- Ensure your working tree is clean (`git status`).
- Use standard git commands to resolve any rebase conflicts (`git mergetool` is your friend,
  followed by `git rebase --continue`), then rerun `git stack restack`.
- Note that the last-known-good location of the parent branch is stored in the sub-branch in order
  to allow for cleaner movement of branches

## License

[GPL2](LICENSE)
