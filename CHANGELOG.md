# Changelog

All notable changes to this project are documented in this file.

## [Unreleased]

### Added
- `git stack restack --continue` / `--abort` now recover from a conflict in *any* stack method
  (am, rebase, merge, squash), driven by git-stack's own recovery record: `--continue` finishes the
  conflicting branch and resumes the rest of the stack, `--abort` restores the branch to its
  pre-restack state (even after a bare `git am --abort` / `git rebase --abort`).
- `git stack llms` — an agent/LLM reference subcommand that prints the full command and semantics
  documentation.
- `git stack cleanup` now prunes out-of-scope branches — those confidently attributed to an author
  outside `display_authors` — in addition to removing branches missing from git, prompting for
  confirmation before persisting.
- Interactive TUI: open the selected branch's PR in the browser with the `o` key.
- Repo-scoped advisory lock serializes concurrent `git stack` runs so they can't race on ref
  updates.

### Changed
- Renamed the `restack --all-parents` flag to `--ancestors` (`-a`).
- When `display_authors` is set, branches belonging to unlisted authors are hidden by default
  (use `--show-all` to bypass).

### Fixed
- Recover from case-insensitive remote-ref collisions during fetch.
- Resolve a branch's author via commit lookup when no PR matches it by name, so author-based
  hiding no longer misclassifies such branches.

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
