# git-stack — agent-targeted reference

This document is printed by `git stack llms`. It is meant for LLM/agent
consumers landing in a repository managed with `git-stack`. Everything an agent
needs to drive the common
checkout → commit → `restack` → `pr create` → `sync` loop is here — the mental
model, every subcommand and its flags, the on-disk state and config files, auth
resolution, restack/PR/sync semantics, and the gotchas that block commands. No
[source code](https://github.com/wbbradley/git-stack) reading required.

`git-stack` installs as a `git-stack` binary that git invokes as the `git stack`
subcommand. Every command below is written `git stack <cmd>`; invoking the
binary directly as `git-stack <cmd>` is equivalent.

## 1. What `git-stack` is

`git-stack` manages **stacked branches**: chains of feature branches where each
branch builds on the one below it instead of all branching off trunk. You keep
each logical change small and reviewable, stack the next change on top, and let
`git-stack` keep the whole tower rebased and its PRs correctly targeted as lower
branches change.

The stack is a **tree rooted at trunk** (`main`/`master`). Every branch has
exactly one parent (the branch it is stacked on) and any number of children.
`git-stack` stores this tree per-repository in a state file — it is metadata
*about* your branches, layered on top of ordinary git branches and refs. Git
itself has no notion of the stack; `git-stack` is the bookkeeping that remembers
"branch `feature-b` is stacked on `feature-a`, which is stacked on `main`."

## 2. Mental model

- **Trunk is the root.** Each repo's tree is rooted at the trunk branch
  (detected from the remote's default branch, typically `main`). Trunk itself is
  never a stacked branch and cannot have a PR.
- **Parent / child.** A branch is *mounted* on a parent. `feature-b` mounted on
  `feature-a` means `feature-a` is `feature-b`'s parent and `feature-b` is one
  of `feature-a`'s children. Commits unique to a branch are those between the
  parent and the branch tip.
- **LKG parent (last-known-good parent).** Each branch records the SHA/ref of
  its parent as of the last successful restack. This lets `git-stack` take a
  fast `git format-patch`/`git am` path: it knows exactly which commits are
  "yours" (the range parent..branch as of the LKG point) and replays only those
  onto the parent's new tip, instead of re-deriving the range every time. The
  LKG parent is refreshed on load and after each successful restack.
- **StackMethod.** Each branch restacks by one of two methods (serialized in the
  state file):
  - `apply_merge` (**default**) — replay your commits onto the parent tip with
    `git format-patch` + `git am`. Rewrites the branch's history, so restacking
    force-pushes with lease. Best for solo branches; keeps history linear/clean.
  - `merge` — pull the parent's changes in with `git merge`. Does not rewrite
    history, so it is safe for branches shared with others (no force-push). Set
    this on a branch you share to avoid clobbering collaborators.
- **Auto-mount.** Most read/navigation commands auto-mount the current branch if
  it is not yet in the tree — they infer a sensible parent and add it — so you
  rarely need an explicit `git stack mount`. `status`, `up`, `down`, `log`,
  `diff`, `note`, `restack`, and `interactive` all auto-mount the branch they
  operate on.

## 3. Subcommands

`git stack` with no subcommand is `git stack status`.

| Command | What it does |
|---------|--------------|
| `git stack status` | Show the stack tree for the current repo (default command). |
| `git stack interactive` | Launch the interactive TUI for navigating/checking out branches. |
| `git stack up` | Check out the parent branch (move up the stack). |
| `git stack down` | Check out the child branch (only when there is exactly one child). |
| `git stack edit` | Open the state file in `$EDITOR` for manual editing. |
| `git stack restack` | Rebase the current (or `-b`) branch onto its parent. |
| `git stack log` | Show the commit log between a branch and its parent. |
| `git stack note` | Show or edit the free-form note attached to a branch. |
| `git stack diff` | Diff a branch against its parent. |
| `git stack checkout <branch_name>` | Create `<branch_name>` as a child of the current branch (or check it out if it exists). |
| `git stack mount [parent_branch]` | Mount the current branch on `parent_branch` (defaults to trunk); retargets an existing PR's base. |
| `git stack delete <branch_name>` | Remove a branch from the stack tree (metadata only). |
| `git stack cleanup` | Remove tree branches missing from git; prune out-of-scope (foreign-author) branches. |
| `git stack pr create\|view\|sync` | Manage GitHub PRs for the stack (see §7). |
| `git stack auth login\|status\|logout` | Manage GitHub authentication (see §6). |
| `git stack cache clear` | Clear the PR cache and seen-SHA set. |
| `git stack completions <shell>` | Print shell completions (works outside a git repo). |
| `git stack sync` | Bidirectionally sync local stack state with GitHub PRs (see §8). |
| `git stack llms` | Print this document (works outside a git repo). |

### Flags, command by command

- **`git stack status`** — `-f`/`--fetch` fetch from the remote before
  rendering. Renders even with no GitHub token (PR columns are simply omitted).
- **`git stack interactive`** — no flags. TUI: navigate branches, `o` opens the
  selected branch's PR in the browser, checkout on selection.
- **`git stack up`** — no flags. Checks out the parent of the current branch.
- **`git stack down`** — no flags. Checks out the sole child; errors if a branch
  has zero or multiple children (ambiguous).
- **`git stack edit`** — no flags. Opens `state.yaml` in `$EDITOR`.
- **`git stack restack`** — see §5 for semantics.
  - `-b`/`--branch <name>` — restack this branch instead of the current one.
  - `-f`/`--fetch` — fetch from the remote first.
  - `-p`/`--push` — push the branch(es) to the remote after a successful restack.
  - `-a`/`--ancestors` — recursively restack every ancestor from trunk up.
  - `-s`/`--squash` — squash all of the branch's commits into one.
  - `--continue` — resume a restack (any method) interrupted by a conflict (after resolving).
  - `--abort` — cancel an in-progress restack and restore the original branch state.
- **`git stack log [branch]`** — no flags. Log of the range parent..branch.
- **`git stack note [branch]`** — `-e`/`--edit` open the note in `$EDITOR`
  instead of printing it.
- **`git stack diff [branch]`** — no flags. `git diff` of parent..branch.
- **`git stack checkout <branch_name>`** — no flags. Positional branch name.
- **`git stack mount [parent_branch]`** — no flags. Optional positional parent
  (defaults to trunk). If the current branch has a PR, its base is retargeted to
  the new parent.
- **`git stack delete <branch_name>`** — no flags. Positional branch name.
  Removes it from the tree only; does not delete the git branch or any PR.
- **`git stack cleanup`** — remove branches from the tree that no longer exist
  locally or on the remote, re-mounting their children onto the grandparent. When
  `authors_filter` is set, it *also* prunes branches confidently attributed to an
  author outside that list (the same set `status` hides), prompting for
  confirmation before persisting (and refusing on a non-interactive terminal).
  `-n`/`--dry-run` previews without changing anything; `-a`/`--all` cleans every
  repo tree in the state file — missing-branch removal only, no author prune — and
  drops invalid repos too.
- **`git stack pr create|view|sync`** — see §7.
- **`git stack auth login|status|logout`** — see §6.
- **`git stack cache clear`** — no flags. Empties the PR cache and seen-SHA set.
- **`git stack completions <shell>`** — positional shell (`bash`, `zsh`,
  `fish`, `elvish`, `powershell`).
- **`git stack sync`** — `--push` push-only, `--pull` pull-only (mutually
  exclusive; default is both), `-n`/`--dry-run` preview without changing
  anything. See §8.
- **`git stack llms`** — no flags. Prints this document.

Global flags (valid on any subcommand): `-v`/`--verbose`, `--benchmark` (print
git-command timing stats), `--json` (emit those stats as JSON, implies
`--benchmark`), `--show-all` (bypasses `authors_filter`-based hiding for this
invocation).

## 4. State file

Location: `~/.local/state/git-stack/state.yaml` (via the XDG state dir with
prefix `git-stack`). It is YAML, written with `0600` permissions. `git stack
edit` opens it in `$EDITOR` for manual repair.

Structure: a top-level map keyed by **canonical repo path**. Each repo's value
inlines the root `Branch` of that repo's tree (the trunk branch) plus two
bookkeeping fields:

- Per `Branch`: `name`, `stack_method` (`apply_merge` | `merge`), optional
  `note`, `lkg_parent` (nullable), optional `pr_number`, and `branches` (the
  list of child `Branch`es — this is what makes it a tree).
- Per repo: `seen_remote_shas` (SHAs observed on the remote, used to prove a
  local branch can be safely pruned) and, when a restack is interrupted by a
  conflict, `pending_restack` (records the `method` — `am`/`rebase`/`merge`/
  `squash` — the conflicting branch, its pre-restack SHA, and enough of the
  original invocation to resume the remaining branches).

Sample `state.yaml`:

```yaml
/Users/you/src/myrepo:
  name: main
  stack_method: apply_merge
  lkg_parent: null
  branches:
    - name: feature-a
      stack_method: apply_merge
      lkg_parent: main
      pr_number: 42
      branches:
        - name: feature-b
          stack_method: apply_merge
          lkg_parent: feature-a
          pr_number: 43
          branches: []
  seen_remote_shas:
    - 1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0b
```

## 5. Restack semantics

`git stack restack` rebases a branch onto the current tip of its parent so the
branch contains only its own commits, replayed on top of up-to-date parent work.

- The **working tree must be clean** — restack refuses to run on a dirty tree.
- `apply_merge` branches (the default) replay commits via `format-patch`/`am`,
  which rewrites history; after a successful restack the branch is
  **force-pushed with lease** when `-p` is set. `merge` branches use `git merge`
  and never force-push.
- **Conflicts.** On a conflict (in *any* method — `am`, `rebase`, `merge`, or
  `squash`), restack stops, records a `pending_restack` in the state file, and
  leaves the tree in a conflicted state. Resolve with `git mergetool` (or
  standard git conflict resolution), `git add` the resolved files, then run
  **`git stack restack --continue`** to finish the conflicting branch and resume
  the remaining branches, or **`git stack restack --abort`** to restore the
  conflicting branch to its exact pre-restack SHA. `--abort` recovers even if you
  already ran a bare `git am --abort` / `git rebase --abort`, because the anchor
  SHA lives in git-stack's own record, not git's transient state.
- `-afp` is the common "sync the whole stack and push" combo: fetch (`-f`),
  restack all ancestors from trunk up (`-a`), and push each on success (`-p`).
- **Squash flow.** `git stack restack -s` squashes the branch's commits into
  one. Like the other methods, a conflict records a `pending_restack` (with
  `method: squash`) and stops; `--continue` / `--abort` behave as above.

## 6. Config and authentication

Commands that talk to GitHub (`sync`, `pr create`, `pr view`, `pr sync`, and the
PR columns in `status`) need a token. Config lives at
`~/.config/git-stack/github.yaml` (XDG config dir, prefix `git-stack`), written
`0600`. Fields: `default_token` (a PAT), per-host tokens under `hosts`, and
`oauth_token` / `oauth_scope` from the device-flow login.

**Token resolution order** (first source that yields a non-empty token wins):

1. `GITHUB_TOKEN` environment variable
2. `GH_TOKEN` environment variable
3. `git config --get github.token`
4. `github.yaml`: host-specific token → `default_token` (PAT) → `oauth_token`
5. Borrowed [`gh` CLI](https://cli.github.com/) token (`gh auth token`), if
   you're logged into `gh`

**Auth commands:**

- `git stack auth login` — interactive OAuth **device flow** (recommended):
  prints a code and URL, you approve in the browser, the token is stored as
  `oauth_token`.
- `git stack auth login --pat` — skip the browser menu and paste a personal
  access token (stored as `default_token`).
- `git stack auth status` — show which source the active token comes from (no
  token value is printed).
- `git stack auth logout` — clear git-stack's stored tokens. `--oauth` clears
  only the OAuth token; `--pat` clears only `default_token`. With neither, both
  are cleared. (This does not touch env vars, git config, or `gh`.)

Graceful degradation: with **no** resolvable token, read-only commands still
work — `git stack status` renders the tree, just without PR numbers/state.

`authors_filter` (list of GitHub logins; the old key `display_authors` still works
as a deprecated alias) in `github.yaml` — when non-empty,
`status`/`interactive` hide branches whose PR author isn't listed (case-insensitively), except the
current branch and its ancestor chain to trunk, and any branch whose author
can't be determined at all. A hidden branch's visible descendants reparent to
the nearest visible ancestor for display purposes only. `--show-all` disables
this for one invocation.

Author resolution for hiding isn't limited to open PRs: it also checks
closed/merged PRs (via the same on-disk watermark cache `sync` uses), and, for
branches with no PR match by exact branch name, falls back to asking GitHub
who authored the branch's tip commit. Only a branch whose author truly can't
be resolved by any of these (e.g. unpushed local work) stays protected as
"no PR yet."

## 7. PR workflow

- **`git stack pr create`** creates a GitHub PR for the current branch (or `-b`)
  using its **git-stack parent as the base branch** (not trunk), so the PR shows
  only that branch's own changes. It recursively **ensures every ancestor PR
  exists first**, pushing branches as needed, so the whole stack is represented.
  The title defaults to the branch's first commit message.
  - `-t`/`--title <title>` — override the title.
  - `-m`/`--body <body>` — set the PR body.
  - `--draft` — create as a draft.
  - `--web` — open the PR in the browser after creating.
  - You **cannot** create a PR for trunk.
- **`git stack pr view [branch]`** — open the branch's PR in the browser.
- **`git stack pr sync`** — retarget PR **bases** to match the current git-stack
  parents, processing **bottom-up** so bases are always valid. `-a`/`--all`
  syncs every PR in the stack (default: just the current branch);
  `-n`/`--dry-run` previews without changing anything. This only fixes PR bases;
  it does not push commits (use `restack -p` or `sync` for that).

## 8. Sync semantics

`git stack sync` reconciles local stack state with GitHub PRs using a
Terraform-style staged pipeline: **read** (gather local + remote state) →
**model** (build the target state) → **diff** (compute per-side changes) →
**validate** (ensure changes are non-lossy) → **apply** (execute only if safe).
It fetches with `--tags -f --prune` and never discards unpushed work.

- **Default: bidirectional** — a weak push followed by a weak pull. "Weak" means
  it only makes safe, non-lossy updates.
- `--push` — push-only (local → GitHub). `--pull` — pull-only (GitHub → local).
  The two are mutually exclusive.
- `-n`/`--dry-run` — run read→model→diff→validate and print the plan without
  applying.
- **Auto-prune.** Sync automatically removes local branches that have been
  **merged** or that merely **duplicate the remote** branch, using
  `seen_remote_shas` to confirm no unpushed work would be lost.

## 9. Worked walkthrough

Start on trunk (`main`) and build a two-branch stack:

```bash
git stack checkout feature-a     # create feature-a as a child of main
# ...edit, git add, git commit...

git stack checkout feature-b     # create feature-b as a child of feature-a
# ...edit, git add, git commit...

git stack                        # inspect the tree
git stack restack -afp           # fetch, restack the whole stack from trunk, push
git stack pr create              # PR for feature-b, based on feature-a,
                                 # ensuring feature-a's PR (based on main) exists
git stack sync                   # keep local state and PRs in agreement
```

A `git stack status` render for this stack looks roughly like:

```
main
└── feature-a  #42
    └── feature-b  #43  ← current
```

and the resulting `state.yaml` entry:

```yaml
/Users/you/src/myrepo:
  name: main
  stack_method: apply_merge
  lkg_parent: null
  branches:
    - name: feature-a
      stack_method: apply_merge
      lkg_parent: main
      pr_number: 42
      branches:
        - name: feature-b
          stack_method: apply_merge
          lkg_parent: feature-a
          pr_number: 43
          branches: []
```

Typical daily loop:

1. `git stack checkout <name>` to start a new branch on top of the current one.
2. Make commits normally with `git`.
3. `git stack restack` (or `-afp`) after the parent changes, to stay rebased.
4. `git stack pr create` once the branch is ready for review.
5. `git stack sync` to keep local state and GitHub PR bases consistent.

## 10. Gotchas

- **Dirty tree blocks restack.** Commit or stash first.
- **A pending restack blocks everything.** While `pending_restack` is set (after
  a conflict in any restack method), only `git stack restack --continue` and
  `git stack restack --abort` run; every other command errors until the restack
  is resolved or aborted.
- **Concurrent runs are serialized.** A repo-scoped advisory lock ensures only
  one `git stack` operation touches a repo at a time; a second invocation waits
  for the first.
- **Case-insensitive remote-ref collisions** (e.g. `Feature` vs `feature` on a
  case-insensitive filesystem) are detected and recovered from during fetch.
- **No token ≠ broken.** Without a GitHub token, read-only commands still work;
  only PR-related output/actions are unavailable.
- **Deletion is `git stack delete`.** To remove a branch from the tree, use
  `git stack delete <branch_name>`. There is no `unmount` subcommand — ignore
  any stale message that mentions one.
- **`mount`/`delete` are metadata-only.** They change the git-stack tree, not
  your git branches, history, or (for `delete`) any open PR. `mount` will,
  however, retarget an existing PR's base to the new parent.
