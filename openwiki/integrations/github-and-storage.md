---
type: Integration Guide
title: GitHub Integration and Local Storage
description: GitHub REST and GraphQL boundaries, authentication precedence, configuration semantics, XDG state, redb caches, and offline behavior in git-stack.
resource: src/github.rs
tags: [github, authentication, storage, cache, integration]
---

# GitHub integration and local storage

GitHub supplies pull-request topology and identity; local state remains the source of truth for the user's stack tree. `src/github.rs` owns remote parsing, auth/config, and API types/client behavior. `src/pr_cache.rs` and `src/merge_base_cache.rs` provide persistent accelerators used by [sync and status](../workflows/sync-and-status.md).

## Repository and API boundary

The GitHub repository identifier is parsed from `origin` for common SSH, HTTPS, `ssh://`, and `git://` URLs. GitHub.com uses standard REST/GraphQL endpoints; Enterprise hosts derive `/api/v3` and `/api/graphql` endpoints.

The reusable `ureq::Agent` calls REST for identity and PR create/read/update operations. GraphQL author search discovers open PRs. Fork PRs are excluded from stack PR maps because their head refs are not available on `origin`, though author information may still support filtering.

## Authentication resolution

The first usable source wins:

1. `GITHUB_TOKEN`
2. `GH_TOKEN`
3. `git config --get github.token`
4. host-specific token in `github.yaml`
5. YAML default PAT
6. YAML OAuth token
7. `gh auth token --hostname <host>`

Never record token values in documentation or diagnostics. `git stack auth status` reports that a source resolves; it does not prove the token is accepted or sufficiently authorized. Logout clears stored PAT/OAuth fields but cannot clear environment, Git config, host-specific, or `gh` credentials.

The device OAuth flow targets GitHub.com and requests `repo`; Enterprise users should currently rely on another supported token source.

## Configuration

Configuration is XDG-based, normally `~/.config/git-stack/github.yaml`. Unknown YAML keys are rejected, and `git stack edit --config` reopens the editor until the result validates. Important fields include host tokens, default PAT/OAuth state, `authors_filter`, and `restack_push_no_verify`.

`authors_filter` is a three-state product setting described in [stack state](../domain/stack-state.md). The authenticated login is cached by host because GitHub.com and Enterprise identities can differ.

## Persistent files

| Data | Normal location | Purpose |
| --- | --- | --- |
| Stack YAML | `~/.local/state/git-stack/state.yaml` | Tree, LKGs, notes, PR numbers, seen SHAs, pending restack |
| GitHub config | `~/.config/git-stack/github.yaml` | Auth and behavior settings |
| PR cache | XDG state `git-stack/pr_cache.redb` | Open/closed PR metadata, watermarks, host identity |
| Graph cache | XDG state `git-stack/merge_base_cache.redb` | Merge-base and ancestry results scoped to common Git directory |

Paths follow XDG environment overrides. Files containing state/config/cache data are set to mode `0600` on Unix.

## Cache semantics

The PR cache is keyed by repository for PR rows and by host for identity. Scoped open refresh merges successful results and retains errored branches as LKG cache entries; a full refresh replaces the repository slice. Closed PR refresh uses a watermark. Clearing a repository removes PR rows/watermark but deliberately retains cross-repository host identity.

The graph cache keys immutable commit OIDs. Merge-base keys are symmetric; ancestry keys are directional. Open/read/write failure normally falls back to live graph calculation. There is currently no eviction policy, so rewritten histories can leave unreachable rows.

## Error and offline behavior

- Status can use cached open PRs when network/token lookup fails and discloses the fallback.
- Author discovery and many cache writes are best-effort.
- Default author filtering cannot safely guess identity: if no live or cached login exists, author-aware commands error with corrective options.
- API errors surface GitHub's message; invalid credentials and classic-PAT organization denials receive targeted guidance.
- Sync currently aborts if its closed-PR cache handle cannot open, unlike more permissive status/graph paths.
- Retry, explicit timeout, and rate-limit reset policies are not yet defined.

## Change guidance

Preserve auth precedence, host/repository cache isolation, secure permissions, and explicit degradation messages. Add unit tests for config and mapping logic; transport changes need mock/live-boundary coverage, a current gap noted in the [testing runbook](../operations/testing-and-runbook.md). Any PR model change must be reconciled with [sync/status](../workflows/sync-and-status.md) and the shared [domain state](../domain/stack-state.md).
