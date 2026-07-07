---
name: git-stack
description: Drive and manage stacked git branches with the git-stack tool — create stacked branches, restack them onto their parents, open and retarget GitHub PRs for the stack, and sync local state with GitHub. Use whenever you are working in a repo that uses git-stack (or the user mentions stacked branches, `git stack`, restacking, or stacked PRs) and need to operate the tool correctly.
---

## 1. Load context first

Before doing anything else, load the canonical reference:

```sh
git stack llms
```

Read the full output. **This is the single source of truth** for the mental
model, every subcommand and its flags, the state and config file formats, auth
resolution, and the restack / PR / sync semantics. This skill body intentionally
does not restate any of it — if something is unclear, re-read the
`git stack llms` output, not this file.

If `git stack` is not on PATH (`command -v git-stack` fails), tell the user how
to install it by pointing at the **Installation** section of the
https://github.com/wbbradley/git-stack repo's `README.md`
(`cargo install --git https://github.com/wbbradley/git-stack --locked`) and
**stop**. Do not try to install git-stack for the user.

## 2. The common loop

Most work follows this loop (see `git stack llms` for the full flag set and
semantics of each step):

1. `git stack checkout <name>` — create a new branch stacked on the current one.
2. Make commits normally with `git`.
3. `git stack restack` (often `-afp` to fetch, restack the whole stack from
   trunk, and push) after a parent branch changes, to stay rebased.
4. `git stack pr create` — open a GitHub PR based on the git-stack parent; it
   ensures ancestor PRs exist first.
5. `git stack sync` — reconcile local stack state with GitHub PRs.

## 3. Operating guidance

- Check `git stack status` (or just `git stack`) first to understand the current
  tree before mutating it.
- Restack requires a clean working tree, and a conflict pauses it: resolve with
  `git mergetool`, then re-run `git stack restack`.
- If any command reports that a squash is in progress, only
  `git stack restack --continue` (after resolving conflicts) or
  `git stack restack --abort` will run until it is resolved.
- To remove a branch from the tree, use `git stack delete <branch>` — there is
  no `unmount` command.
- Prefer `-n`/`--dry-run` (on `sync`, `pr sync`, `cleanup`) to preview
  destructive or remote-facing operations before applying them.
