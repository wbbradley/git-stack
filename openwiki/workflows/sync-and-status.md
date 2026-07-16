---
type: Workflow
title: Sync, Status, and Interactive Views
description: GitHub reconciliation and stack presentation workflows, including sync planning, author scope, safe branch removal, shared rendering, caching, and TUI refresh.
resource: src/sync.rs
tags: [workflow, sync, status, tui, github]
---

# Sync, status, and interactive views

Sync reconciles local [stack state](../domain/stack-state.md) with GitHub PR state. Status and the TUI present that reconciled model. They share author scoping, PR metadata, and local Git facts, but status does not apply the sync plan.

## Sync pipeline

`src/sync.rs::sync` holds the repository lock and implements read → model → diff → validate → apply:

1. Parse the `origin` GitHub identity, resolve a client, and fetch with force/prune.
2. Flatten local topology and remote-presence facts.
3. Compute a bounded branch scope. On a fresh/trunk-only tree, reconstruct scope from PR base chains.
4. Resolve effective author filtering and best-effort discover those authors' open PRs through GraphQL.
5. Read scoped open PRs and cached/refreshed closed PRs; persist discovered open PRs for offline rendering.
6. Update seen remote SHAs with one bounded revision walk from tracked tips excluding `origin/<trunk>`.
7. Build the target model, compute typed local and remote changes, validate, and print the plan.
8. Apply local topology/state changes, remote pushes/base updates, and safe deletions in controlled order.

`--push` and `--pull` constrain direction; `--dry-run` prints without applying. Any remote mutation requires interactive confirmation, and non-interactive execution fails with guidance. Local-only plans apply without a prompt.

`validate_plan` currently returns success without substantive validation; this is a documented backlog item in the [quickstart](../quickstart.md).

## Author and stack scope

Remote-only branches are injected only when stack-scoped and allowed by `authors_filter`. Author discovery is skipped for push-only sync and when the explicit empty list means “everyone.” Discovery failure is nonfatal; the primary scoped PR read and default identity resolution can still fail the workflow.

The same effective filter governs status visibility and cleanup pruning, reducing disagreement between what users see and what sync manages. Detailed three-state semantics are in [stack state](../domain/stack-state.md), with token/identity resolution in [GitHub and local storage](../integrations/github-and-storage.md).

## Merged-parent removal

When merged or closed parents leave the tree, the planner computes the complete removal set and resolves each surviving child's first surviving ancestor. If that destination is the natural result of removing the parent chain, it omits a redundant ordinary mount and lets unmount use `reparent_preserving_lkg`.

This distinction is critical: an ordinary mount records the destination tip as a new LKG, whereas topology-only removal must preserve the trusted old-parent boundary. The incident and acceptance criteria are in `docs/diagnostics/sync-merged-parent-lkg.md`; downstream replay behavior is explained in [restack and recovery](restack-and-recovery.md).

## Status rendering

`src/main.rs::status` optionally fetches, ensures a trunk tree, cleans branches absent locally and remotely, resolves filtering, and builds a `RenderableTree`. `src/render/tree_data.rs` combines:

- true parent/child topology and current branch;
- local/remote-only and upstream synchronization state;
- parent descent/divergence and memoized diff statistics;
- notes and PR badges/URLs;
- author visibility and deterministic display ordering.

Hidden intermediate nodes can be collapsed for presentation, but calculations retain the real parent. `src/render/cli.rs` formats the CLI tree.

## Interactive TUI

`src/tui/app.rs` owns terminal setup/restoration and the event loop; `src/tui/input.rs` maps navigation, checkout, PR browser opening, refresh, and quit. Checkout is deferred until after terminal restoration. Refresh reloads state and recomputes the shared render tree in place, preserving selection by branch name where possible.

## Offline and degraded behavior

Status can fall back to last-known-good cached open PR data and explicitly tells the user. Missing caches or graph-cache failures generally trigger live work or reduced metadata rather than changing topology. Sync is stricter in some paths, notably when the PR cache database cannot open. See [integration details](../integrations/github-and-storage.md) and operational caveats in the [runbook](../operations/testing-and-runbook.md).

## Change guidance

Test the target model and emitted plan separately from application. Cover fresh clones, remote-only branches, forks, author filters, merged chains, stale local trunk, dry-run, non-interactive remote plans, and preservation of subtree metadata/LKG boundaries.
