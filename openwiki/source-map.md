---
type: Source Map
title: Engineering Source Map
description: Task-oriented map of git-stack source files, tests, documentation, scripts, and workflows for efficient repository navigation.
resource: src/main.rs
tags: [source-map, navigation, engineering]
---

# Engineering source map

Use this map to minimize broad source exploration. The [architecture overview](architecture/overview.md) explains how these pieces interact.

## Command and workflow entrypoints

| Task | Start here | Continue with |
| --- | --- | --- |
| Add/change a command or flag | `src/main.rs` (`Args`, `Command`, `inner_main`) | `README.md`, `src/llms.md`, `CHANGELOG.md` |
| Change restack or recovery | `src/main.rs` (`restack`, recovery handlers) | `src/git2_ops.rs`, `src/git.rs`, `src/state.rs`, `tests/restack_abort.rs` |
| Change sync reconciliation | `src/sync.rs` (`sync`, target model, plan, apply) | `src/state.rs`, `src/github.rs`, merged-parent diagnostic |
| Change status data | `src/main.rs::build_renderable_tree` | `src/render/tree_data.rs`, `src/git2_ops.rs`, PR cache |
| Change CLI output | `src/render/cli.rs`, `src/render/colors.rs` | shared `src/render/tree_data.rs` |
| Change interactive behavior | `src/tui/app.rs`, `src/tui/input.rs` | shared render model and `src/main.rs::interactive` |
| Change stack topology/state | `src/state.rs` | [domain model](domain/stack-state.md), sync and restack callers |

## Boundaries and infrastructure

- `src/git.rs`: native Git subprocess execution and mutating operations.
- `src/git2_ops.rs`: `GitRepo`, graph/ref/diff queries, restack patch-series selection, graph-cache integration.
- `src/github.rs`: remote parsing, token/config resolution, REST/GraphQL client, PR/auth operations.
- `src/pr_cache.rs`: redb PR and host-identity cache.
- `src/merge_base_cache.rs`: repo-scoped redb merge-base/ancestry cache.
- `src/lock.rs`: common-Git-dir advisory process lock.
- `src/stats.rs`: benchmark timing collection and human/JSON output.
- `src/llms.rs` + `src/llms.md`: compiled-in operator reference.

See [GitHub and local storage](integrations/github-and-storage.md) before changing credentials, API calls, cache keys, or XDG paths.

## Verification evidence

Most tests are colocated `#[cfg(test)]` modules. High-value areas include state tree operations and LKG refresh (`src/state.rs`), sync planning and merged-parent regressions (`src/sync.rs`), Git graph and restack mechanics (`src/git2_ops.rs`), auth/API/config behavior (`src/github.rs`), and cache isolation/persistence (`src/pr_cache.rs`, `src/merge_base_cache.rs`).

`tests/restack_abort.rs` is the integration test that creates a real `git am` conflict and verifies abort restores both HEAD and the branch ref. The [testing runbook](operations/testing-and-runbook.md) explains standard checks and known gaps.

## Product and historical evidence

- `README.md`: user-facing installation, command examples, auth, filtering, storage, troubleshooting.
- `CHANGELOG.md`: release semantics and concise rationale for recent fixes.
- `docs/diagnostics/sync-merged-parent-lkg.md`: incident evidence and acceptance criteria for LKG preservation during merged-parent removal.
- `AGENTS.md`: directs agents to `git stack llms`; its OpenWiki block is currently an uncommitted local change.
- `PLAN.md` and `COMPLETED.md`: untracked local planning artifacts, not authoritative product documentation.

## Automation and performance

- `.github/workflows/rust.yml`: Ubuntu build and standard test CI.
- `.config/nextest.toml`: local nextest concurrency/leak configuration.
- `scripts/benchmark.sh`: synthetic multi-stack status benchmark.
- `.github/workflows/openwiki-update.yml`: currently untracked scheduled documentation update workflow; review its external service and package-install assumptions before committing.

The [workflow pages](workflows/restack-and-recovery.md) and [sync/status page](workflows/sync-and-status.md) provide semantic context that this file map intentionally omits.
