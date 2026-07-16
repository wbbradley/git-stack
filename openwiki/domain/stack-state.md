---
type: Domain Model
title: Stack State and Invariants
description: Canonical domain model for git-stack branch trees, stack methods, LKG replay boundaries, safe deletion evidence, pending restacks, and author scope.
resource: src/state.rs
tags: [domain, state, git, invariants]
---

# Stack state and invariants

Git records commit ancestry but not the user's intended stacked-review topology. `src/state.rs` supplies that missing domain model, persisted per canonical repository path. Both [restack](../workflows/restack-and-recovery.md) and [sync](../workflows/sync-and-status.md) depend on it.

## Data model

- `State.repos`: map from canonical repository path to `RepoState`.
- `RepoState.tree`: recursive `Branch` rooted at trunk.
- `RepoState.seen_remote_shas`: evidence that commits have existed remotely, used in safe cleanup.
- `RepoState.pending_restack`: durable recovery checkpoint.
- `Branch`: name, `StackMethod`, optional note, optional PR number, `lkg_parent`, and children.

`StackMethod::ApplyMerge` uses patch replay for clean solo history. `StackMethod::Merge` merges the parent and is safer for collaboratively shared branches where force-push rewriting is undesirable.

## Logical parent versus LKG parent

A branch's tree parent says where it should be stacked now. `lkg_parent` is a commit SHA recording the trusted previous parent boundary used to separate inherited parent history from branch-owned work. They may intentionally differ after a parent is rewritten, merged, removed, or replaced by a squash.

For ApplyMerge, the replay series is based on `parent...branch` and excludes `^lkg_parent`. The symmetric boundary drops commits already upstream; the explicit LKG exclusion drops superseded commits belonging to the old parent. This boundary is consumed by [restack and recovery](../workflows/restack-and-recovery.md).

### LKG invariants

- Ordinary user mounting records the selected parent's current tip as a new LKG.
- Topology-only reparenting uses `reparent_preserving_lkg` and preserves the boundary verbatim.
- Refresh is monotonic when an existing LKG remains valid: a stale selected parent that is an ancestor must not move the boundary backward; a descendant parent tip may advance it.
- A missing or invalid LKG may be reconstructed from a safe merge base; restack falls back to rebase when a valid ApplyMerge boundary is unavailable.

These rules arose from a real merged-parent sync incident documented in `docs/diagnostics/sync-merged-parent-lkg.md`: an overlapping mount/unmount and then an eager refresh overwrote a child's trusted boundary, allowing removed-parent commits into the child's replay set.

## Tree invariants

- Trunk is the root and cannot be mounted under another branch.
- A branch cannot parent itself.
- Moving an existing branch moves its complete subtree and metadata.
- Auto-mounting fills missing local topology but must not silently rewrite an already tracked branch's meaning.
- Display filtering may collapse hidden nodes, but it does not alter the stored tree or the true parent used for Git calculations.

## Safe deletion evidence

Sync can prune merged or duplicate local branches only when work is known to survive. Relevant evidence includes a previously seen remote SHA, ancestry into trunk, or a local tip already contained by its remote counterpart. Seen-SHA collection uses a bounded walk from tracked tips excluding remote trunk, then garbage-collects stale entries under a short time budget.

This safety model is reconciled by [sync](../workflows/sync-and-status.md) and persisted alongside the tree through [local storage](../integrations/github-and-storage.md).

## Pending restack as a repository gate

`PendingRestackOperation` records method, conflicting branch, new parent, original SHA, squash metadata when needed, and enough invocation parameters to resume the remaining plan. Once present, command dispatch permits only continue, skip, or abort. This prevents unrelated commands from mutating refs while recovery assumptions are active.

## Author scope

`authors_filter` affects visibility, cleanup pruning, remote-only sync injection, and open-PR discovery:

- absent: resolve and use the authenticated user's login;
- empty list: show/sync everyone;
- nonempty list: exactly those authors, case-insensitively.

Trunk, current branch, its ancestor chain, and branches without confident author attribution remain protected in display logic. Identity resolution and caching live at the [GitHub integration boundary](../integrations/github-and-storage.md).

## Change guidance

Changes to mount, removal, LKG refresh, or sync planning require tests for subtree metadata preservation and replay content—not only final topology. Use multi-commit rewritten/squashed parents so patch equivalence cannot hide a bad boundary. See the [testing runbook](../operations/testing-and-runbook.md).
