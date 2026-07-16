---
type: Workflow
title: Restack and Conflict Recovery
description: How git-stack plans and applies branch restacks, bounds patch replay with LKG state, avoids history churn, and recovers from conflicts.
resource: src/main.rs
tags: [workflow, restack, git, recovery]
---

# Restack and conflict recovery

Restack transforms physical Git history so a branch is based on its logical parent from the [stack-state tree](../domain/stack-state.md). `src/main.rs::restack` orchestrates the workflow; `src/git2_ops.rs` answers graph and patch-selection questions; `src/git.rs` invokes native Git.

## Planning and execution

1. Acquire the repo-scoped advisory lock and optionally fetch.
2. Reject trunk; resolve the target branch and refresh relevant LKGs.
3. `State::plan_restack` creates parent-first steps, optionally including ancestors from trunk.
4. Ensure local trunk safely follows its remote.
5. For each branch, select and execute a method, then optionally push.
6. Restore the original checkout, summarize results, and refresh state.

A normal restack is a no-op when the parent is already an ancestor. Squash is also a no-op only when the branch is in sync and has at most one unique commit. This guard matters: replacing an equivalent commit with a fresh SHA breaks descendant ancestry and creates cascading force-push churn.

## Methods

### ApplyMerge fast path

When `lkg_parent` is valid, git-stack selects branch-owned patches with the equivalent of:

```text
git format-patch --cherry-pick --right-only <parent>...<branch> ^<lkg_parent>
```

It resets the branch to the new parent and applies patches with `git am --3way`. The symmetric difference removes changes already upstream, while the LKG exclusion removes superseded old-parent history. This directly depends on the [LKG invariants](../domain/stack-state.md).

If no valid LKG exists, ApplyMerge falls back to `git rebase <parent>`. `StackMethod::Merge` instead runs a merge. `--squash` uses a temporary branch and squash merge mechanics.

Rewriting methods push with force-with-lease. `restack_push_no_verify: true` bypasses pre-push hooks only for pushes emitted by restack; it does not affect sync or PR commands. Configuration is documented in [GitHub and local storage](../integrations/github-and-storage.md).

## Conflict checkpoint

Before returning a conflict to the user, git-stack saves `PendingRestackOperation` with:

- Git mechanism (`am`, `rebase`, `merge`, or `squash`);
- conflicting branch and target parent;
- exact pre-restack branch SHA;
- temporary squash state if applicable;
- original invocation flags needed to resume remaining steps.

The repository is then gated against unrelated commands.

## Continue, skip, abort

```bash
git stack restack --continue
git stack restack --skip
git stack restack --abort
```

- **Continue** refuses unresolved paths, finishes the recorded mechanism, clears the checkpoint, and resumes the remaining plan. For `am`, an empty resolved patch is auto-skipped. It also tolerates an `am` or rebase the user already finished manually.
- **Skip** is valid for `am` and rebase and drops the current empty/superseded patch before resuming.
- **Abort** best-effort aborts native Git state and then force-restores the named branch and checkout to `original_sha`.

`tests/restack_abort.rs` creates a real `git am` conflict and verifies abort restores HEAD, the branch ref, operation directories, and persisted state.

## Historical correctness lessons

Recent fixes establish three rules:

1. Exclude LKG from ApplyMerge replay so rewritten parent commits do not collide with their replacements.
2. Treat empty patches and manually completed Git operations as resumable states, not wedges.
3. Do not rewrite an already-correct or already-squashed branch merely because restack was invoked.

The [sync workflow](sync-and-status.md) is upstream of restack: when merged parents are removed, sync must preserve the child's LKG so restack still selects only child work.

## Change and verification guidance

Test patch contents and final ancestry. Include rewritten parents, changes already upstream, merged-trunk history, multi-commit squash parents, conflicts, manual Git continuation, push behavior, and no-op SHA stability. Run the commands in the [testing and operations runbook](../operations/testing-and-runbook.md).
