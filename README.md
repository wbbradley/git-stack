# git-stack

A Rust CLI tool to manage, update, and rollback a stack of Git branches.

## Overview

This tool manages a "stack" of related branches. It automates stacking, updating, and restoring
them.

Main capabilities:

- Update (rebase and force-push) all branches in the defined stack atop `origin/main`, creating
  backups before rebasing.
- Display summary info about the latest commit on each stack branch (`--verbose`).
- Roll back both stack branches to a backup version (`rollback` subcommand).

## Usage

```sh
# Stack/rebase and update all branches
cargo run --release

# Simply show the commit message for each stack branch
cargo run --release -- --verbose

# Roll back the stack to a previous backup (by backup version or 'origin')
cargo run --release -- rollback --version <backup-id>
```

Typical Workflow:
1. All local changes must be committed or stashed.
2. When running, each stack branch is rebased on the previous one (starting from `main`). Backups
   are made before rebasing.
3. If a conflict occurs during rebase, resolve conflicts and rerun.

## Arguments

- `--verbose, -v`
  Print current stack info and exit.

- `rollback --version <backup-id>`
  Restore both branches in the stack to a prior state, using the given backup id (a timestamp or
  `origin` for remote state).

## Notes

- Requires [Git](https://git-scm.com/) installed and accessible in `PATH`.

## Error Handling

- Exits with an error if the git status is not clean.
- Exits if the stack branches do not exist or have no commits.
- On rebase failure, suggests manually resolving conflicts and rerunning.

## Customization

- To use with your repo/branches, change `GIT_ROOT` & `STACK` constants at the top of `main.rs`.

---

Example:
```sh
cargo run -- --verbose
# branch-01: ...
# branch-02: ...

cargo run
# Rebases branch-01 & branch-02 in order, force-pushes both to origin.

cargo run -- rollback --version 1717682735
# Restores both to the backup made at that timestamp.
```
