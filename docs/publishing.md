# Transactional publishing

`wookie publish` applies a reviewed set of page changes as one logical wiki
operation. Preview is the default; mutation requires explicit `--apply`.

## Prepare a manifest

A publish manifest declares its schema, expected wiki revision, and ordered
operations. A representative manifest is:

```toml
schema = "wookie.changeset/v1"
base_revision = "2d47c87434834af65be04b86af2066418bc05c51"

[[changes]]
op = "create"
id = "architecture/retrieval"
body = """
**Retrieval** returns a bounded, task-aware map before full page content.

File: `src/retrieval.rs`
"""
[changes.metadata]
title = "Retrieval architecture"
tags = ["architecture", "retrieval"]
sources = ["src/retrieval.rs"]

[[changes]]
op = "move"
from = "guides/old-prime"
to = "guides/priming"
rewrite_links = true

[[changes]]
op = "delete"
id = "guides/obsolete"
```

Use the exact schema emitted by the installed command when generating a
manifest; unknown fields and operations are errors. Bodies are literal
manifest content, not shell commands or template execution. By default,
`base_revision` is required and must be the full revision from
`snapshot.wiki.revision` in `wookie --json status`—do not abbreviate it.
JSON manifests are also accepted when their first non-whitespace character is
`{`.

## Preview the complete plan

Omitting `--apply` is a dry run. `--check` makes that intent explicit:

```sh
wookie publish changes.toml --check
```

The manifest path is optional. For generated plans, provide the same TOML on
standard input:

```sh
wookie publish --check < changes.toml
```

The plan should report pages to create, update, move, and delete, plus:

- broken links and newly orphaned pages;
- when provenance checking is enabled, missing or invalid source paths in the
  wiki's registered project checkout;
- writes that cross a locked rules boundary;
- health diagnostics predicted for the proposed page graph and applicable
  critique checks;
- the base/current revision comparison and the files Wookie would touch.

Machine previews expose these as two nested `wookie.report/v1` reports:
`report.data.expected_doctor` audits the complete proposed page overlay, while
`report.data.expected_critique` reports whether rules review is not required,
blocked by missing checks, or ready for manual execution. Critique does not
claim that natural-language rules passed automatically; it returns the exact
affected checks/rule page map and a truthful evaluation contract.
Human previews always summarize both outcomes, including doctor error/warning
counts, affected rules sections, and critique check readiness. Use `--json` for
the complete nested reports; `--full-diff` adds exhaustive page images and
top-level diagnostics without hiding the expected-check summary.

Preview must not update page timestamps, metadata, Git history, locks, or the
manifest. Review the human plan or use `--json` in automation. Normal previews
use compact line excerpts and are bounded by `publish.output_tokens`; override
that ceiling for one check with `--tokens`. Use `--full-diff` only when you
explicitly need exhaustive before/after page images. It bypasses the normal
response budget but does not change which validations run.

Every successful preview includes a `review_token`: a SHA-256 identity over
the exact manifest, complete deterministic plan, raw page catalog,
configuration snapshot, and effective publish policy. CI or an agent can bind
apply to that exact review:

```sh
wookie --json publish changes.toml --check
wookie publish changes.toml --apply --expect-plan sha256:<review-token>
```

`--expect-plan` is optional for ordinary publications and is available through
the MCP publish tool as `expect_plan`. Any intervening manifest, catalog,
configuration, policy, revision, or plan change rejects apply and requires a
new check. A manifest whose creates/updates/deletes/moves have no effective
page result is rejected rather than producing an empty transaction. Every
rendered after-image, including generated frontmatter, must fit the canonical
16 MiB page limit; an oversized image is reported during check and never enters
the plan, journal, or write set.

## Apply after review

```sh
wookie publish changes.toml --apply
```

Apply revalidates the manifest and base revision, acquires the wiki write lock,
journals the complete changeset and its surrounding environment, and applies
it only after all checks pass. The journal binds full before/after catalog
identities and page permissions, exact `wookie.toml` bytes, effective policy,
every configured lock-control file, and the required rules relocks. If
automatic history is enabled, it records the result in one wiki commit. On an
ordinary write, history, or validation failure, Wookie restores prior page
contents and permissions and reports the rollback outcome.

With automatic history enabled, every target path must be clean before apply:
pre-existing staged, unstaged, and untracked target changes are all conflicts.
That check runs while holding the publication lock. A publication is capped at
512 effective page paths and a conservative aggregate argument budget, and the
history layer also checks the combined root, message, flags, and paths against
a Windows-safe command-line ceiling.

The journal records the exact pre-finalizer Git HEAD and canonical commit
message. A successful apply must leave exactly one child commit from that HEAD,
with exact after-images, the expected message, and exactly the journal's page
path set—no missing or hook-added paths. Git tree entries must also remain
regular blobs with executable mode matching the journaled permissions; content
equality or a clean status alone is insufficient. Commit cleanup is verbatim:
comment-looking lines and trailing spaces are preserved, while terminal LF
characters are normalized before commit. Manifest messages allow ordinary LF
and tab formatting but reject CR, terminal controls, and bidirectional display
marks. Hooks that create extra commits, stage unrelated files into the commit,
change any page in the catalog, alter configuration/effective policy, reopen a
relocked rules section, or otherwise make history ambiguous cause Wookie to
keep the journal for explicit recovery.

Git verification output is bounded independently of repository contents:
revision reads retain at most 1 KiB, status and changed-path reads 1 MiB,
commit objects 1 MiB, tree entries 16 KiB, page blobs 16 MiB, and stderr 64
KiB. Oversized read-only output is terminated and reaped. Mutating history
commands instead run to completion while excess stderr is drained and
discarded, so noisy hooks neither deadlock Wookie nor make commit success
ambiguous.

Apply output remains compact and uses the configured response ceiling. JSON
keeps the full plan for small publications; large publications return the
total counts, a limited plan, explicit omission counts, and a `wookie status`
follow-up without making a successful mutation look like a failed command.

This is a logical transaction over filesystem and Git operations, not a claim
that a multi-file filesystem is crash-atomic. A process or machine failure can
interrupt writes between syscalls. Wookie therefore uses a journal/recovery
record. Inspect the interrupted operation, then either restore the recorded
before-images or accept the fully written after-images:

```sh
wookie publish --recover rollback
wookie publish --recover accept
```

Recovery refuses to remove a live publication lock. `--force-stale-lock` is an
explicit last resort after confirming no publisher is running. An ownerless
lock must also exceed `history.lock_stale_seconds` and remain ownerless and the
same directory across a recheck. A mismatched lock can be forced only when its
recorded process is demonstrably dead. Lock owner records are bounded regular
non-symlink files; malformed, oversized, or redirected records are never
trusted. Keep the wiki filesystem and Git history backed up when the knowledge
base is production-critical.

Recovery classifies Git before changing it. `accept` commits only when HEAD is
still the journal's exact pre-finalizer revision; if HEAD is already the exact
reviewed publication child, acceptance only verifies it. Unrelated or extra
lineage is rejected before staging or committing. `rollback` applies the same
lineage check and, when publication history already exists, creates one
verified compensating child. If a crash leaves the journal after that child is
committed, a retry recognizes only the exact reviewed-publish-plus-rollback
chain and removes the journal without another commit. Exact worktree, index,
HEAD, content, permission, parent, and message checks run before the journal is
removed. Recovery also exact-compares the full catalog, configuration,
effective policy, and all relevant lock controls; an unrelated page or control
change must be reconciled explicitly rather than being overwritten. Once apply
has begun, unwinding never guesses that rollback is safe: uncertainty retains
the journal.

MCP clients use the same recovery implementation and safeguards. Call the
read-only `publish_recovery_status` tool first; it returns
`recovery_required` plus compact journal metadata and never includes page
bodies. Then, only after choosing the intended outcome, call
`publish_recover` with `action: "rollback"` or `action: "accept"`.
`force_stale_lock: true` has the same narrow meaning as the CLI flag and does
not override live, malformed, or unverifiable lock ownership. Their structured
results use `wookie.publish-recovery-status/v1` and
`wookie.publish-recovery/v1`, respectively.

## Concurrency and conflicts

`base_revision` prevents a checked plan from silently applying over newer wiki
changes. Regenerate or deliberately rebase a manifest when it is stale. The
write lock serializes publishers, but it does not decide how conflicting edits
should be merged.

Never bypass rule locks through a publish manifest. Rules changes use the
proposal/review/approval workflow described in
[Rules and findings](rules-and-findings.md).
