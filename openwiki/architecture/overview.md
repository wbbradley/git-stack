---
type: Architecture Overview
title: Runtime Architecture
description: Runtime structure of git-stack, including command dispatch, Git access layers, state persistence, rendering, synchronization, and process locking.
resource: src/main.rs
tags: [architecture, rust, cli, git]
---

# Runtime architecture

`src/main.rs` is both the Clap entrypoint and the orchestration layer. It delegates durable topology to `src/state.rs`, graph queries to `src/git2_ops.rs`, mutating Git commands to `src/git.rs`, GitHub work to `src/github.rs` and `src/sync.rs`, and presentation to `src/render/` and `src/tui/`. Use the [source map](../source-map.md) for task-oriented navigation.

## Startup and dispatch

`inner_main` (`src/main.rs`) performs this lifecycle:

1. Parse global flags and commands. `completions` and `llms` return early because they do not require a repository.
2. Resolve and canonicalize the Git toplevel; the canonical path keys per-repository state.
3. Open `GitRepo` for read-oriented libgit2 operations and load YAML `State`.
4. Eagerly refresh relevant LKG boundaries.
5. If a pending restack exists, block every command except `restack --continue`, `--skip`, or `--abort`.
6. Auto-mount command targets where applicable, then dispatch to state, restack, sync, rendering, PR, auth, or cache behavior.

This lifecycle is constrained by the [stack-state invariants](../domain/stack-state.md): recovery is repository-wide, and a recorded replay boundary must not be moved backward merely because a selected parent ref is stale.

## Two Git layers

- `src/git.rs` shells out to native Git for worktree/index/ref mutations, fetch, checkout, patch application, rebase, merge, and user-facing porcelain. This keeps native operation and recovery semantics authoritative.
- `src/git2_ops.rs` wraps `git2::Repository` for fast branch/ref/commit-graph queries, diff statistics, patch-series analysis, and bounded revision walks. Merge-base and ancestry results use a persistent cache.

The split is intentional, not a migration midpoint. Changes that manipulate an in-progress `am` or `rebase` belong with native Git; pure graph questions normally belong in `GitRepo`.

## Major runtime paths

- Default `status` and `interactive` build one `RenderableTree` from local topology, Git facts, and PR metadata. CLI (`src/render/cli.rs`) and TUI (`src/tui/app.rs`) consume the shared flattened model in `src/render/tree_data.rs`. See [sync and status](../workflows/sync-and-status.md).
- `restack` remains orchestrated in `src/main.rs` because it coordinates state checkpoints with multiple Git mechanics and optional pushes. See [restack and recovery](../workflows/restack-and-recovery.md).
- `sync` delegates to `src/sync.rs`, whose explicit pipeline is read → model → diff → validate → apply.
- PR/auth/config calls cross the [GitHub and storage boundary](../integrations/github-and-storage.md).

## Persistence and consistency

Primary state is YAML under the XDG state directory. PR/identity and commit-graph caches are separate redb databases. Secure writers set owner-only permissions on Unix. State is saved at workflow consistency points—especially before returning a conflicted restack to the user—not simply once after dispatch.

`src/lock.rs` provides a crash-safe advisory `flock` on the common Git directory's `config`. Linked worktrees share the lock. Sync and restack hold it across their mutating workflows; external Git and other git-stack mutations do not honor it. Operational implications are tracked in the [runbook](../operations/testing-and-runbook.md).

## Why the architecture evolved this way

Recent history emphasizes correctness under rewritten history and scale:

- ApplyMerge gained an explicit `^lkg_parent` exclusion so a rewritten parent's superseded commits are not replayed into a child.
- Sync replaced per-PR-SHA ancestry probes with one stack-bounded revwalk, making work scale with the stack rather than all closed PRs.
- CLI and TUI share computed tree data, and the TUI can refresh in place while retaining selection.
- Repo-scoped locking and persistent caches were added to avoid concurrent ref races and repeated large graph/API scans.

## Change guidance

Before changing dispatch, identify whether the behavior changes topology, physical history, external PR state, or only presentation. Cross-layer changes should preserve the canonical model in [stack state](../domain/stack-state.md) and add checks from the [testing runbook](../operations/testing-and-runbook.md).
