# Audits and CI

Wookie's audit commands share a versioned report envelope so operators and CI
can consume the same diagnostics without scraping human text.

## Check overall health

For a compact operator view:

```sh
wookie status
```

It summarizes broken links, stubs, orphans, stale pages, missing rules checks,
locked sections, invalid source provenance, malformed protocols, interrupted
publications, and open or structurally invalid findings. Use `doctor` for
detailed health diagnostics:

```sh
wookie doctor --strict
wookie --json doctor --strict
```

`--strict` exits nonzero when the report contains error-severity diagnostics;
warnings and information remain non-blocking. Without it, all findings are
reported but the command remains suitable for local inspection.

## Stable JSON reports

`doctor`, `status`, `critique`, and `ingest` expose the same top-level schema:

```json
{
  "schema": "wookie.report/v1",
  "command": "doctor",
  "generated_at": "2026-07-22T15:04:05+00:00",
  "snapshot": {
    "wiki": {
      "slug": "example",
      "revision": "2d47c87434834af65be04b86af2066418bc05c51"
    },
    "project": {
      "root": "/workspace/project",
      "revision": "8a032da735436372a265bb5c12b217a95a0a7914",
      "mode": "revision"
    }
  },
  "summary": { "errors": 1, "warnings": 0, "info": 0, "total": 1 },
  "diagnostics": [
    {
      "code": "source_missing",
      "severity": "error",
      "message": "documented source does not exist at the audited revision",
      "page": "code/export",
      "source": "src/export.rs",
      "suggestion": "update the page sources or restore the file"
    }
  ],
  "data": { "page_count": 42 }
}
```

Consumers should branch on `schema`, `code`, and `severity`, not English
messages or field ordering. A new incompatible shape requires a new schema
version. Commands may add optional fields within `wookie.report/v1`; CI should
ignore fields it does not understand.

## Pin the audited snapshot

Audits must distinguish wiki state, an exact project revision, and the mutable
working-tree view. Use explicit snapshot options when reproducibility matters:

```sh
wookie doctor --project-root /workspace/project --revision HEAD
wookie status --project-root /workspace/project --revision HEAD
wookie critique --project-root /workspace/review \
  --revision 8a032da735436372a265bb5c12b217a95a0a7914
```

`critique` is compact by default: it returns a bounded projection of applicable
rule sections, checks, rules, and target files, with total/returned/omitted
counts and exact `wookie read <id>` continuations for returned entries. Missing
checks remain diagnostics even when their section is omitted by projection.
Use `--tokens N` to raise the compact response ceiling or explicit `--all` when
CI or a reviewer needs the complete briefing. Wookie never slices rule text
mid-body. Unlike critique, `doctor` is the intentional exhaustive diagnostic
command; use `status` for its compact operator projection.

For a range review, combine the target revision with an explicit base:

```sh
wookie critique --project-root /workspace/review \
  --since origin/main \
  --revision 8a032da735436372a265bb5c12b217a95a0a7914
```

Installations that predate revision-aware integration may expose `--since`
only for the current checkout and cannot provide the same clean-snapshot
guarantee.

At a Git revision, every code page's `sources` entry is checked against that
tree, not merely the current filesystem. Without `--revision`, the snapshot
mode is `working-tree` and sources are validated against the selected root;
that mode is intentionally mutable and must not be presented as a clean-commit
audit. Missing Git metadata is explicit rather than silently treated as a
clean revision.

Staleness uses the normalized, deduplicated union of frontmatter `sources` and
the page's `File:` line. The union prevents a documented change from becoming
“uncovered,” but it does not excuse disagreement: with source provenance
enabled, a missing declaration or mismatch between the two forms is an error
and therefore fails `doctor --strict`.

Doctor also loads every stored protocol. A malformed or unreadable catalog
emits `protocol_catalog_invalid`; a protocol naming an absent effective section
emits `protocol_section_invalid`. Pages tagged `finding` must have exactly one
non-empty `status/*` and one recognized severity. Missing or ambiguous tags are
errors; empty affected-file `sources` and missing `## Owner`, `## Remediation`,
or `## Verification evidence` sections are warnings. These checks operate on
ordinary pages and protocols rather than a separate findings database.

Git path discovery is NUL-delimited end to end. JSON reports preserve exact
valid UTF-8 names, including spaces, non-ASCII text, and control characters;
human output escapes controls such as newlines so a filename cannot forge a
second terminal line. Rename and copy records include both source and
destination paths because either endpoint can invalidate provenance. A Git
path that is not valid UTF-8 produces an explicit error diagnostic rather than
being silently replaced or omitted.

## Reconcile code changes

```sh
wookie --json ingest --since origin/main
```

The ingest report is a bounded worklist: changed source files, affected pages,
staleness confidence, uncovered changes, and suggested sections. Its
`data.worklist_receipt` is a SHA-256 identity over the complete (not merely
displayed) worklist, exact target commit, canonical wiki content, and effective
audit policy. `data.mark_command` contains the exact safe continuation.

Read the diffs, update each page, and rerun ingest after the documentation is
final. Then execute the newly emitted command, for example:

```sh
wookie ingest --mark-reconciled \
  --expect-worklist sha256:... \
  --since 0123456789abcdef... \
  --level standard
```

`--mark-reconciled` advances the sync point; `--mark` remains its shorter
compatible name. Neither form accepts a blind mark. Wookie recomputes the
receipt while holding the wiki mutation lock, requires a clean project at the
exact target HEAD, runs the audit error gate, and rechecks state before
updating metadata. Missing, stale, or malformed receipts fail without moving
the sync point.

If a Git hook or external Git writer makes the metadata commit outcome
ambiguous, Wookie retains `.ingest-reconciliation-recovery.json` and blocks
all ordinary wiki mutations. After inspecting wiki/project history, resolve it
explicitly with `wookie ingest --recover accept` (verify and keep the recorded
target) or `wookie ingest --recover rollback` (restore the pre-mark config). If
the exact mark commit already landed, rollback appends an exact compensating
config-only commit instead of rewriting history. Recovery is retry-safe, has
stable `wookie.ingest-recovery/v1` JSON output, and is also available through
MCP.

Normal human and JSON output is bounded by `retrieval.search_limit` and
`publish.output_tokens`; override one run with `--limit`/`--tokens` or request
an explicitly exhaustive display with `--all`. Projection counts report every
omission. Display limits never change the receipt.

## A small CI gate

```sh
wookie --json doctor --strict > wookie-doctor.json
wookie --json critique --project-root "$PWD" --revision HEAD > wookie-critique.json
```

Archive reports as build artifacts. Do not include private wiki content in
public logs: diagnostic excerpts, source paths, and finding evidence may be
sensitive even when the project repository is public.
