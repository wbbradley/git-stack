# Changelog

All notable changes to this project are documented in this file.

## [0.3.0] - 2026-07-09

### Breaking Changes
- Renamed the `restack --all-parents` long flag to `--ancestors`. The `-a` short flag is
  unchanged; update any scripts that used the long form (no alias was kept).

### Added
- `git stack llms` — an agent/LLM reference subcommand that prints a complete, self-contained
  reference so an agent can drive git-stack without reading the source.
- Interactive TUI: open the selected branch's PR in the browser with the `o` key.
- `git stack cleanup` now also prunes out-of-scope branches — those confidently attributed to an
  author outside `display_authors` — in addition to removing branches missing from git, with a
  preview and a confirmation prompt before persisting.
- Repo-scoped advisory lock serializes concurrent `git stack` runs so they can't race on ref
  updates, with an actionable hint when git reports a locked ref.
- Offline fallback for PR status: when a live PR fetch fails, the render falls back to cached
  last-known-good data and notes that it's showing cached results.

### Changed
- `restack --continue` / `--abort` now recover from a conflict in *any* stack method (am, rebase,
  merge, squash), not just squash. Recovery is driven by git-stack's own persisted record:
  `--continue` finishes the conflicting branch and resumes the rest of the stack; `--abort`
  restores the branch to its exact pre-restack state, even after a bare `git am --abort` /
  `git rebase --abort`.
- When `display_authors` is set, branches belonging to unlisted authors are now hidden by default
  (previously they were only dimmed); use `--show-all` to reveal them. The current branch, its
  ancestor chain to trunk, and branches with no PR yet always stay visible.
- `status`, interactive, and `sync` are substantially faster in large or PR-heavy repos, with no
  change in output for the common case: merge-base/is-ancestor results and closed-PR history are
  now cached in a per-repo redb store, the open-PR fetch is scoped to your stack (and overlaps the
  local git walk), GitHub calls reuse a keep-alive connection, and diff stats are memoized within a
  render.
- `sync` no longer pulls unrelated open PRs into your local stack — remote-only branches are
  injected only if they're within your stack's scope, and, when `display_authors` is set, authored
  by a listed author.
- `git stack cache clear` now also clears the merge-base cache and the PR cache.
- `--benchmark` output now accounts for GitHub REST I/O and namespaces the git timing rows.

### Fixed
- Recover from case-insensitive remote-ref collisions during fetch, so two remote branches
  differing only in case no longer abort the whole `git fetch --prune`.
- Author-based hiding now classifies branches correctly: it resolves a branch's author via a
  commit-tip lookup when no PR matches the branch by name, and consults merged/closed PRs (not just
  open ones) so a branch whose PR was closed by someone else is no longer mistaken for your own
  unpublished WIP.
- Diff stats for never-restacked branches are backfilled once via merge-base instead of being
  reported as perpetually "unreliable."
- The `--benchmark` summary is rendered as an aligned table, so long row labels no longer misalign
  the following columns.

## [0.2.0] - 2026-07-06

First tagged release.

### Added
- Stacked-branch management: view your stack tree (`git stack` / `status`), create stacked
  branches (`checkout`), restack onto parents (`restack`, with `-a`/`-f`/`-p` for recursive
  ancestor restacking, fetch, and push), diff against parent (`diff`), re-parent a branch
  (`mount`), and delete branches (`delete`).
- Interactive TUI status mode with up/down navigation and a current-HEAD indicator.
- GitHub integration: `sync` (push/pull local state, auto-prune merged or duplicate branches,
  retarget PR bases on mount) and `pr create` (opens PRs with the correct base branch).
- GitHub authentication via `git stack auth login` (OAuth device flow or `--pat` paste),
  `auth status`, and `auth logout`.
- Token resolution from `GITHUB_TOKEN`, `GH_TOKEN`, `git config github.token`, or the config
  file at `~/.config/git-stack/github.yaml`.
- Last-resort auth fallback: borrows the `gh` CLI's token (via `gh auth token`) when no other
  source resolves, so users already logged in with `gh` need no separate setup.
- Shell completions.

### Security
- Config and state files are written with `0600` permissions.
