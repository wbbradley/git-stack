# Changelog

All notable changes to this project are documented in this file.

## [Unreleased]

### Changed
- Renamed the `display_authors` config key to `authors_filter`. The old name keeps working as an
  alias and is migrated to `authors_filter` on the next auth write. Author matching against the
  filter is now case-insensitive.

### Internal
- Test suite no longer emits spurious `nextest` LEAK warnings. The temp repos used in tests now
  disable git's background auto-maintenance (`maintenance.auto`/`gc.auto`), which previously spawned
  a detached `git maintenance` process that outlived the test and kept its output pipe open.

## [0.3.1] - 2026-07-09

### Changed
- `sync` is dramatically faster on large repositories. Instead of inspecting every closed-PR head
  SHA individually, it now does a single bounded history walk scoped to your stack's tracked
  branches, so its cost scales with the size of your stack rather than the repository's total
  closed-PR count.
- During `restack`, a branch that has no unique commits over its new parent is now reported as
  `restacked` in the summary instead of being silently skipped.

### Fixed
- `restack` no longer replays commits that are already upstream. Restacking a branch that had
  merged trunk into itself — or a branch stacked on a parent that was itself just rebased — could
  replay commits already present upstream, manufacturing spurious merge conflicts and duplicate
  commits. Restack now builds its patch series the same way `git rebase` does, dropping commits
  reachable from the new parent and commits whose change is already upstream by patch-id.
- `git stack diff` and `git stack log` now report accurate error messages on failure ("git diff
  failed" / "git log failed") instead of the incorrect "git format-patch failed".

### Removed
- The "Filtering PR SHAs" progress bar shown during `sync` was removed. It was cosmetic — the
  filtering step is now effectively instantaneous.

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
