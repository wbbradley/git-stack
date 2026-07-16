---
type: Runbook
title: Testing and Operations Runbook
description: Development checks, test strategy, CI and benchmark behavior, restack recovery, cache and lock operations, diagnostics, and known operational hazards.
resource: .github/workflows/rust.yml
tags: [testing, operations, runbook, ci, diagnostics]
---

# Testing and operations runbook

Use this page when validating changes or diagnosing user-visible failures. The [source map](../source-map.md) locates module-specific tests; the [architecture overview](../architecture/overview.md) explains runtime boundaries.

## Standard development checks

```bash
cargo fmt --check
cargo build
cargo test
cargo nextest run          # when cargo-nextest is installed
cargo clippy --all-targets --all-features
```

Committed CI (`.github/workflows/rust.yml`) runs `cargo build --verbose` and `cargo test --verbose` on Ubuntu. It does not enforce fmt, clippy, nextest, MSRV, audit, or other platforms, so run relevant checks locally.

`.config/nextest.toml` caps test threads at 16 and uses a four-second fatal leak timeout. The setting addresses scheduler-starvation false positives while retaining failure for genuine orphaned handles. Test repositories disable Git auto-maintenance to avoid detached processes holding output pipes.

## Test strategy

Most coverage is colocated with modules. Prefer:

- pure tests for tree transformations, target models, plans, URL/config parsing, and cache semantics;
- temporary real repositories for graph, ref, patch-series, and Git-operation behavior;
- binary integration tests when persisted state and native Git recovery interact.

`tests/restack_abort.rs` is the model integration test: it enters a real `git am` conflict, seeds a pending operation, invokes the binary, and verifies HEAD/ref restoration and checkpoint removal.

For [LKG](../domain/stack-state.md) changes, use a multi-commit parent replaced by a combined squash and assert the generated series contains only child work. Patch-equivalent single commits can conceal a broken boundary.

## Restack incident recovery

When restack pauses:

```bash
git status
# resolve files and git add them
git stack restack --continue
# or, for an empty/already-present am/rebase patch:
git stack restack --skip
# or restore the recorded original branch tip:
git stack restack --abort
```

Pending recovery blocks unrelated git-stack commands. If investigating a failure, preserve command output, `git status`, relevant reflogs, and a sanitized copy of the state topology; never copy credentials. The full mechanics are in [restack and recovery](../workflows/restack-and-recovery.md).

## Sync and network triage

- Use `git stack sync --dry-run` to inspect the plan.
- Remote changes require an interactive confirmation; automation should not expect them to apply on a non-TTY.
- Verify `origin` is a supported GitHub URL and use `git stack auth status` to identify the token source, remembering this does not validate permissions.
- If default author filtering cannot resolve identity, authenticate, explicitly set `authors_filter`, use `authors_filter: []`, or use `--show-all` for display.
- Cached status output is marked; sync can be stricter than status if the PR redb database cannot open.

See [GitHub and local storage](../integrations/github-and-storage.md) for paths and auth precedence and [sync/status](../workflows/sync-and-status.md) for plan semantics.

## Caches and locking

`git stack cache clear` clears repository PR/graph cache data and seen-SHA state, but command setup currently expects a parseable GitHub `origin`. Directly deleting storage should be a last resort and may discard useful offline identity/PR evidence.

Sync and restack serialize through an advisory lock on the common Git directory's `config`. The lock is automatically released on descriptor close, including crashes, and covers linked worktrees. It does not block external Git and does not currently cover every state-mutating git-stack command.

## Performance checks

```bash
./scripts/benchmark.sh --iterations 10
./scripts/benchmark.sh --json --iterations 10
```

The script builds release mode and creates synthetic stacks. It assumes an initial branch named `main`, uses common shell utilities, suppresses mount failures, and constructs JSON from command output; inspect failures/noise before trusting machine-readable results. Runtime `--benchmark` and `--json` instrumentation lives in `src/stats.rs`.

## Open merged-parent observation

`docs/diagnostics/sync-merged-parent-lkg.md` records a real sync defect and a failed first fix observation. The replacement build preserves forward LKG boundaries, with regression coverage in state/sync and CLI abort coverage. The document still requires two naturally occurring successful parent-removal observations, including one multi-commit squash. Record the build identity, commands, pre/post topology and LKG, printed plan, replay range, and conflict outcome. Do not commit the large external diagnostic bundles.

## Automation review note

`.github/workflows/openwiki-update.yml` is currently untracked. It schedules a write-capable documentation job, installs `openwiki` globally without a pinned package version, sends model/tracing traffic to configured external providers, and opens a PR. Before committing, review package pinning, least-privilege permissions, secret configuration, repository-data governance, and whether tracing is appropriate.

## Known test/operations gaps

There is no transport-level GitHub test suite, explicit timeout/retry/rate-limit coverage, cache corruption/contention recovery test, GitHub Enterprise OAuth flow, cross-platform CI, or direct `src/stats.rs` test coverage. Treat these as risk areas when changing the corresponding subsystem.
