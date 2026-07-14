# git-stack agent reference

`git stack llms` prints this self-contained guide. `git-stack` is the binary;
git exposes it as `git stack`. The forms are equivalent; this guide uses
`git stack`.

## Model and normal workflow

git-stack records a tree of ordinary git branches rooted at the remote's
default branch (usually `main`). Trunk is not a stacked branch and cannot have a
PR. Every other branch has one parent and zero or more children. A branch's own
work is the commits between it and its parent.

Typical loop:

```bash
git stack checkout feature-a   # child of the current branch
# edit; git add; git commit
git stack restack -afp         # fetch, restack ancestors from trunk, push
git stack pr create            # PR based on the git-stack parent
git stack sync                 # reconcile local state and GitHub
```

Each branch has a restack method:

- `apply_merge` (default): replay branch-only patches onto the new parent with
  `format-patch`/`am`. History is rewritten; `-p` force-pushes with lease. Best
  for solo work and linear history.
- `merge`: merge the parent into the branch. It does not rewrite history or
  force-push, so use it for shared branches.

Each branch also records an LKG (last-known-good) parent ref/SHA. Patch replay
uses the parent/branch symmetric difference and excludes the old LKG parent,
which drops upstream and superseded-parent commits while retaining the branch's
own work. LKGs refresh on load and after successful restacks.

Most commands that operate on the current branch (`status`, `interactive`,
`up`, `down`, `log`, `note`, `diff`, and `restack`) auto-mount it when absent by
inferring a parent. Use `mount` to choose a parent explicitly.

## Complete command reference

`git stack` with no subcommand means `git stack status`.

| Command | Flags and behavior |
|---|---|
| `git stack status` | Render the tree. `-f`/`--fetch` fetches first. Without usable GitHub data, PR columns may be omitted; see Authentication. |
| `git stack interactive` | Open the navigation/checkout TUI. Arrow keys navigate, Enter checks out, `o` opens the selected PR, `r` refreshes local state in place, and `q`/Esc quits. |
| `git stack up` | Check out the current branch's parent. |
| `git stack down` | Check out its child; errors unless it has exactly one child. |
| `git stack edit` | Open `state.yaml` in `$EDITOR`; `--config` opens `github.yaml`. |
| `git stack restack` | Restack the current branch. `-b`/`--branch <name>` selects another branch; `-f`/`--fetch` fetches first; `-p`/`--push` pushes successful branches; `-a`/`--ancestors` processes ancestors from trunk upward; `-s`/`--squash` makes one commit. Recovery flags: `--continue`, `--skip`, `--abort`; see Restack. |
| `git stack log [branch]` | Show the parent..branch commit log (current branch by default). |
| `git stack note [branch]` | Print the branch note; `-e`/`--edit` opens it in `$EDITOR`. |
| `git stack diff [branch]` | Show the parent..branch diff (current branch by default). |
| `git stack checkout <branch>` | If absent, create the branch as a child of the current branch; otherwise check it out. |
| `git stack mount [parent]` | Mount the current branch on `parent` (trunk by default). If it has a PR, retarget the PR base. This changes metadata, not git history. |
| `git stack delete <branch>` | Remove only stack metadata; do not delete the git branch or PR. There is no `unmount` command. |
| `git stack cleanup` | Remove tree branches missing locally and remotely, remounting children on the grandparent; with author filtering, also confirm-prune out-of-scope branches (and refuse that prune non-interactively). `-n`/`--dry-run` previews. `-a`/`--all` cleans every stored repo (missing branches and invalid repos only; no author prune). |
| `git stack pr create` | Create the current branch's PR. `-b`/`--branch <name>`, `-t`/`--title <title>`, `-m`/`--body <body>`, `--draft`, `--web`. |
| `git stack pr view [branch]` | Open the branch PR in a browser. |
| `git stack pr sync` | Retarget PR bases to stack parents, bottom-up. `-a`/`--all` handles the whole stack; `-n`/`--dry-run` previews. Does not push commits. |
| `git stack auth login` | OAuth device flow. `--pat` instead prompts for a personal access token. |
| `git stack auth status` | Show the active token source without printing the token. |
| `git stack auth logout` | Clear stored OAuth and PAT tokens; `--oauth` or `--pat` limits what is cleared. Does not change env, git config, or `gh`. |
| `git stack cache clear` | Clear the PR cache and seen-SHA set. |
| `git stack completions <shell>` | Print completions for `bash`, `zsh`, `fish`, `elvish`, or `powershell`; works outside a repo. |
| `git stack sync` | Bidirectional sync by default. `--push` is push-only; `--pull` is pull-only (mutually exclusive); `-n`/`--dry-run` plans without applying. |
| `git stack llms` | Print this guide; works outside a repo. |

Global flags: `-v`/`--verbose`; `--benchmark` for git-command timings;
`--json` for JSON timings (implies `--benchmark`); `--show-all` to bypass
author filtering for this invocation.

## Restack and conflict recovery

Restack requires a clean working tree. `-afp` is the common whole-stack form:
fetch, process ancestors from trunk upward, then push. `apply_merge` pushes use
force-with-lease; `merge` pushes do not. An already-correctly-stacked branch is
a no-op (including an already-single-commit branch under `--squash`), except
that `-p` pushes it if its remote is out of sync.

`github.yaml` supports `restack_push_no_verify: true` to add Git's
`--no-verify` option to every push actually emitted by `restack -p`, bypassing
the local pre-push hook. It defaults to `false`, does not cause otherwise
unneeded pushes, and does not affect pushes from `sync`, `pr create`, or other
commands.

On an `am`, rebase, merge, or squash conflict, git-stack records
`pending_restack` and pauses. Resolve normally, `git add` the result, then:

- `git stack restack --continue` finishes the branch and resumes the saved plan.
  For `am`, it automatically skips a patch that resolved to empty.
- `git stack restack --skip` explicitly skips the current `am`/rebase patch and
  resumes. It is invalid for merge/squash conflicts.
- `git stack restack --abort` restores the branch's exact pre-restack SHA.

Continue/skip also recover if the underlying `git am` or `git rebase` was
finished by hand. Abort still works after a bare `git am --abort` or
`git rebase --abort`. While recovery is pending, all other commands are blocked.

## PRs and sync

`pr create` bases a PR on its git-stack parent, not trunk, so it contains only
that branch's changes. It recursively ensures ancestor PRs exist, pushing as
needed. The default title is the first commit message. Trunk cannot have a PR.

`sync` runs a staged read -> model -> diff -> validate -> apply pipeline and
fetches with tags, force-update, and prune. It never discards unpushed work.
Default sync is a weak (safe, non-lossy) push followed by a weak pull. It also:

- removes local branches that are merged or duplicate their remote, using
  `seen_remote_shas` to prove pruning is safe;
- on pull-direction runs, discovers open PRs by effective `authors_filter` and
  reconstructs remote stacks from their base chains, even from a trunk-only
  tree. Discovery is skipped for `--push` and `authors_filter: []`; failures
  fall back to stack-scoped data without aborting;
- best-effort caches discovered open PRs, so later offline status/TUI renders
  can retain their badges and URLs.

## Authentication and author filtering

GitHub commands and PR display use the first non-empty token from:

1. `GITHUB_TOKEN`
2. `GH_TOKEN`
3. `git config --get github.token`
4. `github.yaml`: host token, then `default_token` (PAT), then `oauth_token`
5. `gh auth token`

Config is `~/.config/git-stack/github.yaml` (mode `0600`). Besides tokens, it
supports `authors_filter` (the deprecated `display_authors` alias is migrated on
the next auth write):

```yaml
default_token: <PAT>
hosts: {github.example.com: <host-PAT>}
oauth_token: <device-flow-token>
oauth_scope: repo
authors_filter: [octocat]
restack_push_no_verify: false
```

All fields are optional.

- absent (default): filter to your GitHub login, obtained from `/user` and
  cached per host;
- `authors_filter: []`: show everyone and require no login resolution;
- `authors_filter: [a, b]`: show those logins (case-insensitive).

Filtering affects status/TUI display, cleanup's confirmed author pruning, and
sync's remote-branch injection/discovery. The current branch, its ancestors,
trunk, and branches whose author cannot be resolved stay visible. Hidden
branches' visible descendants reparent to the nearest visible ancestor for
display only. Author lookup considers open and closed/merged PRs, then the tip
commit author. `--show-all` disables filtering only for that invocation.

With no token, non-GitHub operations still work and cached PR data may render.
However, when the filter is absent in a GitHub repo and the user's login is not
cached, `status`, `interactive`, `cleanup`, and `sync` must resolve that login;
if offline/tokenless they fail with guidance rather than guessing. Authenticate,
set `authors_filter` explicitly, or use `authors_filter: []`/`--show-all`.

## Files and invariants

State is `~/.local/state/git-stack/state.yaml` (XDG state dir, mode `0600`), a
map keyed by canonical repo path. Each repo value contains the trunk `Branch`
and `seen_remote_shas`; it temporarily contains `pending_restack` during
recovery. A branch has `name`, `stack_method` (`apply_merge` or `merge`),
nullable `lkg_parent`, child `branches`, and optional `note` and `pr_number`.
`pending_restack` records `method` (`am`, `rebase`, `merge`, or `squash`),
`branch_name`, `parent`, `original_sha`, optional squash temp/message fields,
and `resume` with the original target/return branches and flags.

```yaml
/Users/you/src/repo:
  name: main
  stack_method: apply_merge
  lkg_parent: null
  branches:
    - name: feature-a
      stack_method: apply_merge
      lkg_parent: main
      pr_number: 42
      branches: []
  seen_remote_shas: [1a2b3c4d5e6f]
```

`git stack edit` permits manual repair. PR/login caches live in
`~/.local/state/git-stack/pr_cache.redb`. Repo operations use an advisory lock,
so concurrent invocations serialize. Fetch detects and recovers from
case-insensitive remote-ref collisions.
