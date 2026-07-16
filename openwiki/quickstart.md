---
type: Quickstart
title: git-stack Code Wiki Quickstart
description: Entry point for engineers working on git-stack, a Rust CLI for stacked Git branches, GitHub synchronization, safe restacking, and stack visualization.
resource: README.md
tags: [git-stack, rust, cli, stacked-branches]
---

# git-stack code wiki

`git-stack` is a Rust CLI for developing a sequence of dependent branches as a tree, keeping each branch based on its logical parent, and reflecting that topology in GitHub pull requests. The product combines local Git history rewriting, durable per-repository metadata, GitHub reconciliation, CLI/TUI status views, and recovery from interrupted restacks (`README.md`, `Cargo.toml`).

## Start here

- [Architecture overview](architecture/overview.md) explains startup, command dispatch, module boundaries, persistence, rendering, and concurrency.
- [Source map](source-map.md) maps engineering tasks to the smallest useful set of files.
- [Stack state and invariants](domain/stack-state.md) is the canonical model for branch topology, last-known-good (LKG) replay boundaries, safe deletion evidence, and pending recovery.
- [Restack and conflict recovery](workflows/restack-and-recovery.md) describes history rewriting and `--continue`/`--skip`/`--abort`.
- [Sync, status, and interactive views](workflows/sync-and-status.md) covers GitHub reconciliation, author scoping, rendering, and TUI refresh.
- [GitHub and local storage](integrations/github-and-storage.md) documents APIs, authentication resolution, configuration, caches, and offline behavior.
- [Testing and operations runbook](operations/testing-and-runbook.md) gives development checks, troubleshooting, diagnostics, and current operational gaps.

## Build and orient

```bash
cargo build
cargo test
git stack llms       # generated operator/agent reference; does not require a repo
git stack --help
```

The primary binary is `src/main.rs`. It uses Clap for commands, native `git` subprocesses for mutations, `git2` for graph/ref queries, `ureq` for GitHub, `redb` for caches, and `ratatui`/`crossterm` for the interactive view. See the [architecture overview](architecture/overview.md) before changing cross-module behavior.

For product usage, `README.md` is the concise user guide and `src/llms.md` is the exhaustive operator contract emitted by `git stack llms`. Keep both aligned with command and configuration changes; `src/llms.rs` has coverage that checks major commands and file paths remain represented.

## Core engineering mental model

1. The YAML [stack state](domain/stack-state.md) records a logical tree that Git itself does not encode.
2. A branch's logical parent and its `lkg_parent` serve different purposes: topology chooses the new base; LKG bounds which historical commits belong to the child.
3. [Restack](workflows/restack-and-recovery.md) transforms physical history to match the logical tree and persists recovery state before yielding on conflicts.
4. [Sync](workflows/sync-and-status.md) reconciles local topology with GitHub PR heads/bases and merged state, while status/TUI project the same model for users.
5. [GitHub caches and graph caches](integrations/github-and-storage.md) improve offline behavior and performance but should not redefine correctness.

## Change checklist

- Start from the relevant page and source anchors in the [source map](source-map.md).
- Preserve the domain invariants, especially LKG monotonicity and safe deletion.
- Add focused unit tests near the module; use a CLI integration test when real Git operation state matters.
- Run `cargo fmt --check`, `cargo test`, and preferably `cargo nextest run`; CI currently runs only build and standard tests.
- Update `README.md`, `CHANGELOG.md`, and `src/llms.md` for user-visible semantics.

## Backlog

- **HTTP resilience — `src/github.rs`:** retries, explicit timeouts, and rate-limit/reset behavior are not yet established.
- **Cache lifecycle — `src/pr_cache.rs`, `src/merge_base_cache.rs`:** growth, schema migration, corruption recovery, and eviction policies need definition.
- **Sync plan validation — `src/sync.rs::validate_plan`:** validation is currently a stub and deserves explicit invariants.
- **Cross-platform quality gates — `.github/workflows/rust.yml`:** CI lacks fmt, clippy, nextest, MSRV, and non-Linux coverage.
