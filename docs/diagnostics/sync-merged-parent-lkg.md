# Merged-parent sync LKG diagnostic

On 2026-07-14, a real `git stack sync` in `/Users/wbbradley/src/langchainplus`
planned two overlapping topology changes for the same open child branch:

```text
Mount 'wbbradley/webhookdelivery-context-hub' on 'main'
Unmount 'wbbradley/webhookdelivery-package' (children -> 'main')
```

The mount ran first and replaced the child's trusted last-known-good (LKG) parent with stale local
`main`. The unmount then had no child left to reparent with LKG-preserving semantics. The subsequent
restack succeeded only because the removed parent had one commit and its squash commit was
patch-equivalent to that commit. A multi-commit or conflict-edited squash would allow the old
parent commits into the child's replay series and could manufacture conflicts.

## Incident provenance

- Repository: `/Users/wbbradley/src/langchainplus`
- Installed binary: `git-stack 0.5.0`
- Binary SHA-256: `9528c019d3a323e5b74bbabc273f7ceb31fb322198117c827b0769af3b3b86a4`
- git-stack source checkout: `0b4b0fcb7e5d9804ed7dcb1546443e4d5a583924`
- Commands:

  ```text
  git stack -v sync
  git stack -v restack -afp --branch wbbradley/webhookdelivery-context-hub
  ```

- Merged parent PR: `#30131`
- Open child PR: `#30299`
- Original parent tip: `5e7d1554c29aeef87b0eab18162c985e4a3cfca0`
- Parent squash commit: `bdcea486ee7b283aacbc7e16a5713e69c01f9048`

## Relevant state transitions

Before sync, the child was mounted beneath the merged parent with the old parent tip as its LKG:

```yaml
- name: wbbradley/webhookdelivery-package
  stack_method: apply_merge
  lkg_parent: 5070873ec858fe7c577a3894c26441b91744b57c
  pr_number: 30131
  branches:
  - name: wbbradley/webhookdelivery-context-hub
    stack_method: apply_merge
    lkg_parent: 5e7d1554c29aeef87b0eab18162c985e4a3cfca0
    pr_number: 30299
    branches: []
```

After sync, the child was directly on `main`, but its LKG had been overwritten with stale local
`main` (`origin/main` had advanced to `3bdd14cdce36d78977a3013d4b81659aa0511217`):

```yaml
- name: wbbradley/webhookdelivery-context-hub
  stack_method: apply_merge
  lkg_parent: 5070873ec858fe7c577a3894c26441b91744b57c
  pr_number: 30299
  branches: []
```

After restack, local `main` caught up and the child was rebuilt and pushed:

```yaml
- name: wbbradley/webhookdelivery-context-hub
  stack_method: apply_merge
  lkg_parent: 3bdd14cdce36d78977a3013d4b81659aa0511217
  pr_number: 30299
  branches: []
```

The rebuilt child tip was `f111b3b39baf3bcae582234ade4caf230638f848`. Its PR contained two
child commits and remained mergeable, but this successful outcome masked the planner bug.

## Preserved archive

The source archive is
`/Users/wbbradley/.local/share/git-stack/diagnostics/sync-merged-parent-lkg-2026-07-14/`.
The raw bundles remain outside this repository. SHA-256 checks and `git bundle verify` were run
successfully on 2026-07-14 before using the evidence.

| File | Size | SHA-256 |
| --- | ---: | --- |
| `refs.pre-sync.bundle` | 346,593,669 bytes | `015593651b29e2e54aad5ed806a9c24c5e1cd7749301e543aa9241b61a603351` |
| `refs.post-restack.bundle` | 346,730,186 bytes | `d97f48f417f11b8431e7663d83cc98eb9cf73a87bffb1cc00752e0c01d344870` |
| `state.pre-sync.yaml` | 5,191 bytes | `5ae5c3383e4717e05a69e2bfb68bebd76c1e8389476f9cd20d2c590e809019da` |
| `state.post-sync.yaml` | 4,884 bytes | `45376cb11486cfba44dbc32e1be440775e64218614b8ddfe0817751cc23a1c92` |
| `state.post-restack.yaml` | 4,929 bytes | `017d656547b48149f731d80d6891ebb54c5c065ef9e844dbf727b008cada33de` |
| `report.md` | 3,562 bytes | `0be904efa52d3f9dd706f8a1b877314324b2bf230f4dcc12085d11441d197aad` |

The bundles contain complete history for the incident refs. They intentionally include the large
monorepo history and must not be committed.

## Fix and regression model

The sync planner must compute the complete removal set and each removed branch's transitive
destination before finalizing mounts. When an existing child's current parent is being removed and
the requested target is exactly that natural destination, the planner omits the mount and lets the
unmount's `reparent_preserving_lkg` operation perform the move. Remote-only branches and genuine
moves to a different surviving parent retain ordinary mount semantics and record the selected
parent tip as the new LKG.

Regression coverage uses a two-commit parent and a combined squash commit so patch-id equivalence
cannot conceal an overwritten replay boundary. It applies the emitted local plan, verifies the
child's LKG remains the old parent tip, generates a patch series containing only child work, and
applies that series cleanly to squash-merged `main`.

## Natural observations

The fix remains under observation until two naturally occurring parent-removal syncs succeed,
including at least one multi-commit parent squash. The installed observation build is:

- Installed 2026-07-14:
  - version: `git-stack 0.5.0`;
  - source SHA: `5f742d93798a33402ad942345b302da3b58141ca`;
  - installed binary: `/Users/wbbradley/.cargo/bin/git-stack`;
  - binary SHA-256: `f1792ac12fdba0d2010e868899ad2dca2336d733909dced1ed780c397c181cf8`.

For each natural case, capture:

- git-stack version, installed binary SHA-256, and source SHA;
- exact sync/restack commands;
- parent commit count and merge/squash SHA;
- pre-sync topology and child LKG;
- printed sync plan;
- post-sync topology and child LKG;
- child-only restack range or patch series; and
- whether any conflict occurred.

Acceptance requires both cases to show no redundant child mount, an unchanged trusted child LKG
after sync, and a restack containing only child work.

### Failed observation: eager refresh moved the preserved LKG backward

On 2026-07-14, the observation build failed on a second real parent removal in
`/Users/wbbradley/src/langchainplus`. This case does not count toward the two required successful
observations. It used the installed build above from source
`5f742d93798a33402ad942345b302da3b58141ca` and these commands:

```text
git stack sync
git stack restack -afp
git stack restack --abort
# Manual recovery after the abort left the branch at trunk:
git reflog
git checkout -B wbbradley/context-hub-webhook-cap 915e26d052
```

Before the parent merged, the relevant physical history and recorded replay boundary were:

```text
main 3bdd14cdce36d78977a3013d4b81659aa0511217
  wbbradley/webhookdelivery-context-hub
    9973011bf50059803972e3e16a6f104c2ef983ae
    f111b3b39baf3bcae582234ade4caf230638f848
      wbbradley/context-hub-webhook-cap
        915e26d052537c46ed76454a05ea608206c4ada4
        lkg_parent: f111b3b39baf3bcae582234ade4caf230638f848
```

PR `#30299` combined the two parent commits into squash commit
`6b4b3f43b18aa17a75344e4dbe1a8c43dd9f2edb`. After sync removed the old parent refs, the intended
topology was `main <- wbbradley/context-hub-webhook-cap` with the child's LKG still at `f111b3b39b`.
The redundant child mount was correctly absent, but the next command's eager LKG refresh selected
stale local `main` and changed the child's LKG to
`3bdd14cdce36d78977a3013d4b81659aa0511217`. Because that SHA is also an ancestor of the removed
parent, the child remained a valid descendant and the old refresh logic unconditionally replaced
the newer replay boundary. `git stack log` consequently classified both old parent commits and the
child commit as child work.

The surviving reflogs record the failed restack and recovery exactly:

```text
31ee80f797343a66c793f8c78474c04c9c9fa522 origin/main@{2026-07-14 19:52:56 -0600} fetch --tags -f --prune origin: fast-forward
31ee80f797343a66c793f8c78474c04c9c9fa522 wbbradley/context-hub-webhook-cap@{2026-07-14 19:53:10 -0600} branch: Reset to main
31ee80f797343a66c793f8c78474c04c9c9fa522 HEAD@{2026-07-14 19:53:18 -0600} am --abort
915e26d052537c46ed76454a05ea608206c4ada4 HEAD@{2026-07-14 19:53:18 -0600} checkout: moving from 915e26d052537c46ed76454a05ea608206c4ada4 to wbbradley/context-hub-webhook-cap
915e26d052537c46ed76454a05ea608206c4ada4 wbbradley/context-hub-webhook-cap@{2026-07-14 19:53:18 -0600} branch: Reset to HEAD
```

The branch ref was manually recovered to `915e26d052`; its current metadata still records the
regressed LKG `3bdd14cdce`. The exact sync plan text and restack/abort stdout were not preserved, so
this record does not reconstruct them beyond the commands and reflog evidence above.

### Follow-up implementation and abort investigation

The LKG refresh now treats a valid replay boundary monotonically: when the selected parent's tip is
an ancestor of that boundary, refresh preserves it; when the parent tip is a descendant, refresh
may advance it. Regression coverage models stale local trunk, a two-commit removed parent, its
combined squash on updated remote trunk, and one child-only commit. It performs the LKG-preserving
unmount, runs the next command's eager refresh, and verifies that only child work is replayed cleanly
onto the squash-updated trunk. A complementary test proves descendant parent tips still advance an
older LKG.

The abort symptom was investigated separately. The force-restore implementation predates this
incident (commit `506b1e6494979257b6803bc799279988d63f7975`, 2026-07-09), and a new CLI integration
regression enters a real `git am` conflict before invoking `git stack restack --abort`. It confirms
successful exit, removal of the pending operation, and restoration of both `HEAD` and the named
branch ref to `pending.original_sha`. No remaining abort code defect was reproduced; the real
reflog proves that the underlying `git am --abort` returned to trunk, but the unavailable command
output prevents determining why the subsequent force-restore was not reflected in that run.

The replacement observation build was installed on 2026-07-14:

- version: `git-stack 0.5.0`;
- source SHA: `4d5dd72b8147ed80ef5a57a6a8e51d41bcea9ade`;
- installed binary: `/Users/wbbradley/.cargo/bin/git-stack`;
- binary SHA-256: `490bcfbb9a5454545ed800d9114d9146fd3aacb0514364045b24eb904ce59399`.

Future observations must report this source SHA and digest (or a later build containing the fix) to
count toward acceptance.
