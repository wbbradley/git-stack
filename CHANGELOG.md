# Changelog

All notable changes to this project are documented in this file.

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
