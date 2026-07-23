# wookie specification

Wookie is an LLM-first local wiki manager and cooperative agent-coordination
system. One Rust binary owns the conventions and storage invariants. CLI and
MCP entry points call the same command and storage layers.

## Goals and boundaries

Wookie provides:

- one project wiki shared by a checkout and its linked worktrees;
- Markdown pages with deterministic metadata and wikilink behavior;
- configurable information and rules sections, protected normative content,
  bounded task-aware retrieval, ingest worklists, and critique briefings;
- inert project-scoped page protocols and journaled transactional publishing;
- durable agent sessions and append-only notifications with receiver-local
  acknowledgement state;
- safe concurrent local mutation and serialized Git history;
- typed CLI JSON and MCP structured results.

Wookie contains no LLM and performs no remote synchronization. It inventories,
validates, stores, and emits worklists or briefings; the invoking agent supplies
judgment and prose. Separate clones do not merge automatically by remote URL.

## Storage layout

```text
<WOOKIE_HOME>/                         default: ~/.wookie
  config.toml                          global registry and defaults
  <slug>/
    wookie.toml                        per-wiki config and sparse overrides
    .git/                              local wiki history
    .gitignore
    .history.lock/                     transient, ignored
    .publish.lock/                     shared writer/publisher lock, ignored
    .publish-journal.json              interrupted transaction state, ignored
    .unlocks/                          per-section transient state, ignored
    .cache/retrieval-v1.json           disposable incremental parse index, ignored
    protocols/
      findings/finding.md
    pages/
      scheduler.md
      internals/retry-policy.md
    sessions/
      session-20260721-143052-7f3a/
        session.toml
        activity/
          activity-20260721-143100-....toml
        notifications/
          notify-20260721-144210-....md
        inbox/
          notify-....read
          notify-....dismissed
        inbox.toml                     optional legacy state, ignored
```

`WOOKIE_HOME` overrides the default. `HOME`, `USERPROFILE`, or
`HOMEDRIVE`/`HOMEPATH` locate the user home when no override is present.

Page id is the relative path below `pages/` without `.md`. Ids are lowercase
and each segment permits ASCII letters, digits, `-`, `_`, and `.`. Absolute
paths, empty/hidden/dot/parent segments, leading or trailing slashes, and
uppercase aliases are rejected. Wiki slugs permit lowercase ASCII letters,
digits, `-`, and `_` only.

## Wiki resolution

Resolution order is:

1. CLI `--wiki <slug>` or MCP `wiki`.
2. Longest current-directory prefix match against every wiki's
   `project_roots`.
3. Git main-worktree fallback via `git rev-parse --git-common-dir`, followed by
   the same longest-prefix match.
4. An error listing known wikis.

The per-wiki `wookie.toml` is authoritative for roots. The global registry is
maintained for lifecycle metadata but is not the root-resolution source of
truth. `wookie init` from a linked worktree records the main worktree. Non-Git
projects are supported. Independent clones only share a wiki when their roots
are explicitly registered or the caller selects a slug.

## Page format and link model

```markdown
---
title: Run Lifecycle
description: One-line summary used by context and tocs
tags: [scheduler, core]
sources: [src/scheduler/]
created: 2026-07-17
updated: 2026-07-21
pin: false
---

The first paragraph is a standalone summary.

Body links to [[scheduler]] or [[internals/retry-policy|retry policy]].
```

Wookie owns known frontmatter fields, updates timestamps on content writes,
uses `status: stub` for unfilled pages, and clears stub status when real content
is written. Unknown lines round-trip. Frontmatter values are sanitized so
callers cannot break the delimiter block. Malformed page frontmatter becomes a
doctor finding instead of crashing a whole scan.

Wikilinks inside inline code or fenced code blocks do not participate in the
graph. `wookie expand [<id>]` creates every eligible stub page for broken links
and prints a bounded fill worklist. `--limit` caps returned IDs per category,
`--tokens` caps the estimated response, and neither changes creation semantics.
Totals and omissions remain machine-readable; `--all` explicitly restores the
exhaustive current worklist. `wookie read <id> --expand[=N]` performs a
deduplicated breadth-first traversal and inlines compact linked title,
description, summary, and stub state. Depth is capped at 5 and at most 100
linked pages are loaded; deterministic omission counts direct the caller to
explicit `read` commands.

## Sections, pinned pages, critique, and ingest

Sections are top-level page namespaces in `[sections.<name>]`, with
`description`, `kind`, optional `locked`, and `required` page names. Entries
overlay built-in `architecture`, `code`, `decisions`, `guides`, `findings`,
`style`, and `workflow` sections. `style` and `workflow` are `rules`;
the others are `info`.

Rules sections are locked unless overridden. The storage-layer `save_page` and
`delete_page` checks are authoritative; command-level checks only improve
diagnostics. Commands that require an internal raw save must first authorize
every affected page. Consequently, `expand` skips locked targets, `mv`
preflights the source, destination, and every backlink it would rewrite, and
`doctor --fix` refuses to repair a locked rules page. Raw saves are reserved
for already-authorized transactional work and rollback; they are not a lock
bypass.

`wookie unlock <section> [--minutes N]` writes an RFC3339 expiry to the ignored
`.unlocks/<section>.toml`; the interval is clamped to 1 minute through 24 hours
and defaults to 15 minutes. `wookie lock` writes an authoritative locked
marker. MCP unlock requires
`user_approved: true`. Generic `config set/unset sections.*` carries the same
explicit-approval requirement.

Each rules section should contain `<section>/checks`, describing scope,
procedure, violations, and exceptions. `wookie critique` collects selected
rules sections, their checks, target changes (uncommitted, staged, since a ref,
or explicit paths), and a fixed output contract. Compact output projects
bounded summaries with omission counts and read continuations; `--all` includes
complete rule bodies. It is read-only; the agent executes the review.

Pins have `instruction`, `summary`, and `discoverable` levels. Legacy
`pin: true` maps to `instruction`. Prime extracts `## Agent instructions` when
present; otherwise it uses the instruction page's first-paragraph summary.
Summary pins contribute only their first standalone paragraph. Discoverable
pins contribute compact title/description metadata and an explicit read
command, never body or summary text and never instruction-budget tokens.
Instruction and summary pins must be non-stub and non-placeholder; prime fails
closed and doctor emits a stable error when legacy data violates that
invariant. The instruction set has its own validated hard budget; Wookie fails
instead of dropping an oversized standing rule.

`wookie ingest --level quick|standard|deep` inventories tracked project files
(or a filtered filesystem walk), seeds `code/` stubs with `sources`, and emits
a level-specific documentation worklist. `ingest --mark-reconciled` (`--mark`)
records the current project HEAD only after the agent completes the work.
Subsequent ingests diff
from that sync point and map changed files to the most-specific matching page
source, also reporting uncovered changes and new modules. `--full` and
`--since` override update behavior.

## Bounded retrieval

`wookie prime --query <task>` is the task-start surface. Its complete human or
JSON representation is constrained by `retrieval.prime_tokens`, independently
of wiki size. It contains wiki identity/freshness, opaque state and context
hashes, budgeted standing instructions, compact discoverable-pin and section
metadata, ranked page metadata without body excerpts, explainable match
reasons, omissions, a continuation cursor, and telemetry. If mandatory
metadata and standing instructions cannot fit, the command fails instead of
truncating them. Optional discoverable metadata is compacted and omitted only
with explicit counts and a complete-map command.

Ranking is deterministic and combines exact id/title/tag/source/text matches,
one-hop link proximity, and known staleness. It has stable id tie-breaking and
no embedded model, remote service, or embedding index. `max_per_section`
creates bounded ranked windows; cursors always identify a contiguous next
window so pages are not silently skipped.

`wookie search` is ranked and bounded by default. Limits cover the final
serialized response, including excerpts, telemetry, and continuation
instructions. `--regex` selects bounded regular-expression matching; `--all`
returns every matching page without the normal response budget while retaining
at most five matching body lines per page. `wookie context` remains the
explicit exhaustive catalog.

Prime's query-independent `state_hash` covers canonical catalog identity, pin
state, wiki/configuration and effective sections, and project freshness.
`--since <state-hash>` omits unchanged section structure while still returning
guaranteed instructions, discoverable metadata, and suggestions reranked for
the current query. Its separate `context_hash` covers that state plus the query
and effective window options; every nonzero cursor requires it through
`--context-hash`. Search returns its own query/options/catalog hash with the
same cursor rule. State or query drift fails explicitly instead of skipping or
repeating pages.

Prime and search maintain one disposable, ignored JSON parse cache. Strong
stat signatures reuse unchanged parsed pages; exact per-page raw SHA-256 leaves
form the canonical framed catalog content hash used by reports and publish.
Corrupt, oversized, wrong-version, symlinked, or temporarily unwritable cache
state is bypassed safely. Unchanged strong-signature entries reuse their
integrity-checked parsed projection without reopening page bodies. Prime
canonically rereads only instruction/summary bodies before output and then
rechecks the complete file generation; discoverable pins remain metadata-only.
The wiki-owning local account is the trust boundary: hashes detect accidental
corruption, not a same-user actor able to rewrite both source and derived cache.
This remains a deterministic parse/catalog cache: ranking scans cached text and
no daemon, database, embedding, or semantic index is introduced.

## Page protocols and findings

Protocols are Markdown files below `protocols/`, addressed by safe lowercase
namespaced names. Existing path components must be real directories and the
protocol file must be a regular non-symlink within a bounded size. An optional
strict TOML header accepts only `description`, `section`, and `tags`; the body
supports one-pass substitution of `{{id}}`, `{{title}}`, and `{{date}}`.
Unknown variables and fields fail closed.

`protocol list/show/write/remove` own protocol lifecycle. `new --protocol`
renders once and then performs ordinary id, section, rules-lock, page, and
history validation. Protocols cannot execute processes, install dependencies,
inherit other protocols, migrate existing pages, or bypass rules locks.

Findings are normal pages under the built-in information section `findings/`.
The built-in `findings/finding` protocol supplies the shared scaffold. The
`finding` tag identifies a finding; exactly one `status/*` tag represents its
lifecycle, while `severity/*`, source metadata, owners, remediation, and
verification evidence remain ordinary searchable page data. Frontmatter
`status` remains reserved for page stubs.

## Stable audit and revision model

`doctor`, `critique`, `expand`, and `ingest` emit additive
`wookie.report/v1` JSON. The stable envelope contains `schema`, `command`,
`generated_at`, a wiki/project snapshot, severity counts, diagnostics with
stable codes, and command-specific data. Human wording is not an API. `status`
projects the same audit into a compact health dashboard covering broken links,
stubs, orphans, staleness, source provenance, missing rules checks, locks,
recovery state, sessions, and unresolved findings.

The `expand` report keeps its `created`, `stubs`, and `skipped_locked` arrays
for compatibility and adds `totals`, `omissions`, `continuation`, and bounded
output telemetry. A bounded response may omit IDs but never suppresses their
creation. Omitted `created`/`stubs` entries remain directly readable;
`skipped_locked` targets were intentionally not created and remain visible
through the explicit `expand --all` worklist.

Audit commands accept an explicit project root and, where relevant, a Git
revision. Revisions reject option-like/control-character input and resolve to
full commit ids before inspection. Source paths must be normalized
project-relative paths. Code pages without sources and sources absent from the
selected working tree or revision become diagnostics. Critique separates an
exact revision from dirty-worktree review and returns the applicable rules and
checks with `evaluation = "not_executed"`; Wookie never fabricates an LLM
verdict.

Ingest JSON is a reconciliation worklist: each stale page includes changed
files, deterministic confidence, suggested sections, and a safe next command.
The sync point changes only through `--mark-reconciled` after the work is done.

## Transactional publish and rules lifecycle

A `wookie.changeset/v1` TOML or JSON manifest contains an optional full
`base_revision`, optional message, and ordered create/update/delete/move
operations. Parsing rejects unknown fields, unsafe ids/sources, oversized
input, and executable behavior. Check mode is the default and materializes an
overlay, page diffs, source/link/orphan diagnostics, applicable rules checks,
and a deterministic `wookie.publish-plan/v1` without mutation. Its normal
human and JSON projections use compact diff excerpts and the configured
`publish.output_tokens` ceiling; explicit `--full-diff` output is exhaustive
and intentionally outside that bounded contract. Every preview also returns a
SHA-256 `review_token` over the manifest, complete plan, raw catalog,
configuration snapshot, and effective policy. `--apply --expect-plan` (and
MCP `expect_plan`) optionally requires that exact reviewed identity. Effective
no-op plans are invalid. Every rendered after-image, including generated
frontmatter, must fit the canonical 16 MiB page bound before it can enter a
plan or journal.

Apply revalidates the complete catalog and base revision after acquiring the
shared writer/publisher lock. Before touching pages it records exact before and
after bytes plus portable permissions, full before/after catalog identities,
exact wiki configuration, effective policy, configured lock controls, and
required rules relocks in a transaction-token-bound journal.
Automatic-history targets must have no pre-existing staged, unstaged, or
untracked changes. All pages are then written and committed as one path-scoped
history unit whose parent is the journal's exact pre-finalizer HEAD and whose
canonical verbatim message, after-images, and exact changed-path set are
verified. Tree entries must be regular blobs whose Git executable bit matches
the journaled mode. A hook-added/missing path, mode change, unrelated catalog
mutation, configuration/policy change, or reopened rules lock fails closed.
Ordinary write or finalizer failure restores page bytes, metadata, permissions,
and path-scoped Git index state. The operation is logically transactional, not
a claim of multi-file filesystem crash atomicity: a surviving journal requires
explicit rollback or acceptance. Recovery refuses live, malformed, or
token-mismatched locks and only force-clears a demonstrably dead owner. It
classifies HEAD before history mutation: acceptance commits only from the
recorded base, treats an exact existing reviewed child as verification-only,
and rejects unrelated lineage before staging. Rollback uses the symmetric
lineage rule and verifies any compensating commit. A retry after that commit
recognizes only the exact base/publish/rollback chain and is verification-only.
Recovery exact-compares unrelated pages and all journaled control state before
mutation, then verifies the complete selected before/after state again before
removing the journal. Ambiguous image, index, history, catalog, configuration,
policy, or lock state retains the journal. Git subprocess capture is bounded:
read-only verification fails fast on oversized stdout, while mutating history
commands drain excess stderr and complete without being killed after an
ambiguous commit boundary. Publication messages allow LF and tab but reject
CR, terminal controls, and bidirectional display marks.

All ordinary page/protocol/config writers use the same mutation lock, so they
cannot overlap a publisher. Readers do not take that lock and may observe an
intermediate multi-file state during apply; the journal and health dashboard
make an interrupted state explicit.

Rules changes store bounded manifests as proposals, re-run the same preflight
during review, and persist a strict receipt binding the raw proposal, exact raw
catalog, configuration, effective policy, deterministic plan, and revisions.
They require `--user-approved` to apply. Apply reprepares exactly once, requires
an exact receipt match, and authorizes only those locked pages while the
publication lock is held. Already-unlocked affected rules sections are relocked
under that same lock; custom locked information sections are not treated as
rules. Approval is an assertion at the CLI/MCP boundary; agent guidance still
requires actual explicit user permission for the exact change.

## Configuration model

Global configuration is a typed `GlobalConfig`:

```toml
[wikis.my-project]
project_roots = ["/path/to/project"]

[defaults]
auto_commit = true

[defaults.sessions]
# complete SessionSettings

[defaults.history]
# complete HistorySettings

[defaults.retrieval]
prime_tokens = 1500
instruction_tokens = 700
search_limit = 10
search_tokens = 2000
excerpt_lines = 2
max_per_section = 5

[defaults.audit]
source_provenance = true
critique_tokens = 4000

[defaults.publish]
require_base_revision = true
orphan_policy = "warn"
output_tokens = 4000
```

Per-wiki configuration is a typed `WikiConfig`:

```toml
name = "my-project"
description = "Project knowledge"
project_roots = ["/path/to/project"]
auto_commit = false                  # optional

[sessions]
poll_limit = 50                      # sparse optional override

[history]
fail_on_commit_error = true          # sparse optional override

[retrieval]
prime_tokens = 1800                  # sparse optional override

[audit]
source_provenance = true             # sparse optional override
critique_tokens = 4000               # sparse optional override

[publish]
orphan_policy = "error"              # sparse optional override
output_tokens = 5000                 # sparse optional override
```

Effective settings are resolved on open as:

```text
global defaults -> apply each present per-wiki override -> validate
```

Retrieval token budgets are positive and capped at the immutable 1,000,000
token safety ceiling. `search_limit` and `max_per_section` are each limited to
1–1,000; `excerpt_lines` is limited to 1–20. CLI and MCP one-off overrides use
the same ceilings before page materialization.

All feature override structures make every field optional. Therefore one local
override does not copy or freeze unrelated global values. Per-wiki
`retention_days = 0` explicitly disables a global retention period; an absent
value inherits it.

`wookie config show|get|set|unset|keys` edits dotted TOML paths. `--global`
limits mutation to `defaults.*`; wiki registration is left to lifecycle
commands. Per-wiki `name`, `project_roots`, and `last_ingest_commit` are also
protected. `--string` bypasses scalar TOML parsing. A candidate document is
deserialized into the real type and validated before atomic replacement;
unknown fields and wrong types fail.

Default session settings are:

| Key | Default |
|---|---:|
| `enabled` | `true` |
| `initial_lookback_hours` | `0` |
| `stale_after_minutes` | `120` |
| `activity_debounce_seconds` | `30` |
| `retention_days` | unset |
| `auto_prune_on_start` | `false` |
| `poll_limit` | `100` |
| `max_summary_bytes` | `512` |
| `max_agent_bytes` | `128` |
| `max_label_bytes` | `1024` |
| `max_body_bytes` | `65536` |
| `max_paths` | `64` |
| `max_path_bytes` | `4096` |
| `max_targets` | `32` |
| `max_idempotency_key_bytes` | `256` |
| `max_metadata_entries` | `32` |
| `max_metadata_key_bytes` | `64` |
| `max_metadata_value_bytes` | `1024` |
| `max_git_dirty_paths` | `256` |
| `max_git_branch_bytes` | `512` |
| `max_git_commit_bytes` | `128` |
| `max_git_worktree_bytes` | `4096` |
| `include_git_context` | `true` |
| `heartbeat_on_activity` | `true` |
| `default_kind` | `note` |
| `default_importance` | `normal` |

Default history settings are a 30,000 ms lock wait, 60-second stale-lock
threshold, `commit_sessions = true`, and `fail_on_commit_error = false`.

## Session model

### Identity and activity

`session start` creates
`session-<UTC YYYYMMDD>-<UTC HHMMSS>-<unique lowercase suffix>`. The exclusive-
create base `session.toml` records id, agent, optional label, creation/update
timestamps, initial lookback, activity debounce, heartbeat preference, and
`active` status.

The base file is never rewritten for normal activity. Heartbeats, close events,
and automatically observed activity create unique files beneath `activity/`.
Loading a session sorts valid events by timestamp and id, then derives
`updated_at`, `last_seen_at`, and status. Missing or malformed legacy activity
is tolerated. Automatic events are debounced per session; `heartbeat --force`
and close bypass the debounce. `WOOKIE_SESSION` supplies omitted CLI session
arguments.

Session listing filters status, agent, label text, creation time, and activity
time, with limit and ordering. CLI `--active` and `--stale` derive their cutoff
from `sessions.stale_after_minutes`. A corrupt session entry becomes a warning
while other sessions remain listable.

### Pruning

`session prune` selects by last observed activity, closed status, age or an
explicit RFC3339 cutoff, and a `keep_latest` guard. It is a dry run unless CLI
`--apply` (or MCP `dry_run: false`) is explicit. Only closed sessions are
eligible by default; CLI `--include-active` / MCP `closed_only: false` expands
the set. An unbounded request that includes active sessions is rejected.
Configured retention defaults to 30 days when no cutoff is supplied. Optional
auto-prune runs on session start only when both `auto_prune_on_start` and a
retention period are configured.

## Notification model

### Record and publication

Each notification is an immutable Markdown file with TOML frontmatter. Its
metadata contains:

- generated id, source session, one-line summary, kind, importance, and
  RFC3339 timestamp;
- affected project paths;
- zero or more target session ids (zero means broadcast);
- optional idempotency key;
- optional `GitContext` (`branch`, `commit`, `worktree`, bounded dirty paths);
- caller-defined, single-line string metadata.

Kinds are `code-change`, `decision`, `blocker`, `handoff`, `warning`, and
`note`. Importance is ordered `low < normal < high`. The source session must
exist and remain active. Target ids must identify other active sessions; a
source cannot target itself. Lists are deduplicated and normalized; configured
byte/count bounds are enforced. An empty body becomes the summary.

CLI publication captures Git context from the invocation directory by default;
`--no-git-context` disables it. MCP follows configuration unless
`include_git_context` is supplied. Failure to obtain Git context outside a
worktree is non-fatal and simply omits it.

An idempotency key is scoped to a source session. Its stable hash determines a
repeatable notification id. If a matching key already exists with the same
semantic payload, publication returns that existing notice. A different
payload with the same key fails. Git context is deliberately not part of the
payload equality check, so an operation retry after the worktree changes still
resolves to the first notice.

### Delivery, lookback, and filters

Polling excludes the receiver's own notifications and any targeted notice not
addressed to it. Normal unread polling applies a cutoff of:

Targeting is cooperative routing, not an authorization or confidentiality
boundary: a user with local wiki access can read the underlying Markdown.

```text
session.created_at - effective_lookback
```

The effective lookback comes from the request override or session metadata.
Zero therefore starts caught up. Full history (`--all` /
`include_acknowledged`) skips that cutoff and includes read/dismissed states.

Filters can match source sessions, kinds, minimum importance, affected path
prefixes, Git branches, exact metadata pairs, created-after/before timestamps,
maximum age, and case-insensitive text across summary and body. Results sort by
creation time and id newest-first by default, can reverse to oldest-first, and
are truncated to a request limit no greater than configured `poll_limit`.
Responses include total, returned, and omitted counts plus an offset
continuation bound to the same filters and ordering.

Automatic Git context is best-effort and project-bound. It is captured only
when the invocation directory is inside the wiki's registered project root or
a linked worktree that shares that project's Git common directory. Resolving a
wiki explicitly from an unrelated checkout never attaches that checkout's Git
metadata.

The poll surface returns compact metadata only. Every collection scan reads
bounded frontmatter and validates metadata, but it does not load bodies.
Bodies are loaded for a direct read, an idempotent-publish payload
comparison, and a text filter only when the summary does not already match.
If a body-dependent filter encounters an invalid notice, the poll reports a
warning and skips that notice rather than aborting the healthy remainder.
`notification read` returns the body and acknowledges it; `notification
dismiss` acknowledges irrelevance without exposing the body. A receiver
cannot read or dismiss a notification targeted elsewhere.

### Append-only acknowledgement and compatibility

Each acknowledgement exclusively creates a
`inbox/<notification>.read` or `.dismissed` marker for that action. Separate
files eliminate the read-modify-write race of a shared inbox, and repeating the
same acknowledgement is idempotent. If opposing actions race, both immutable
markers can exist and `dismissed` has precedence. The entire `inbox/` directory
is Git-ignored.

Legacy `inbox.toml` state is merged at read time. A missing, invalid, symlinked,
or non-file legacy inbox becomes an isolated warning rather than blocking the
new store. Wookie does not rewrite or migrate the legacy file in place.

### Corruption isolation

Notification scans consider only regular, non-symlink `.md` files beneath real
session notification directories. Each file's size, frontmatter, and metadata
are checked independently. Unusable metadata records and duplicate ids are
omitted and returned as `StorageWarning` entries with paths and messages, so
they do not hide the remaining collection from inbox polling, session show, or
doctor diagnostics. Because bodies are lazy, body-only decoding or size errors
are not doctor findings and do not prevent ordinary metadata polling; a direct
read, text search, or idempotent comparison that needs such a body
fails. A direct operation on a missing, corrupt, duplicated, or improperly
targeted id also fails.

## Filesystem containment and atomicity

The canonical wiki directory is a real direct child of the canonical Wookie
home. A symlink at the wiki slug is rejected.

Every managed relative path is resolved one segment at a time below that trust
boundary. Absolute, root, prefix, parent, and dot components are rejected.
Every existing descendant is inspected with `symlink_metadata`; symlinks are
rejected, and non-directory ancestors fail. Directory creation repeats the
check after each component, preventing known symlink redirection through
`pages/`, `protocols/`, `sessions/`, publish state, configuration, unlock, and
ignore paths.

Mutable files are written to an exclusively created same-directory temporary,
flushed, permission-preserved when replacing, and atomically installed. Unix
uses `rename` and best-effort parent-directory sync. Windows calls
`MoveFileExW` with replace-existing and write-through flags because standard
rename cannot replace an existing file there. Failed writes remove the
temporary. Existing symlink destinations are never replaced.

Immutable session, activity, notification, and acknowledgement records are
written and synced under temporary names, then published without replacement:
Unix uses a hard link and Windows uses no-replace rename. This removes shared
mutable session state; Unix storage must support same-filesystem hard links.

Page move is a small recoverable transaction:

1. validate both ids and the locks for the source, destination, and backlinks;
2. load every page and prepare all inbound-link changes before disk mutation;
3. write the destination while keeping the source alive;
4. rewrite backlinks one at a time;
5. delete the source only after rewrites succeed;
6. on error, restore applied backlinks and remove the destination;
7. if rollback cannot finish, keep both ids so old and new links resolve and
   report every rollback error.

## Git history under concurrency

Wikis initialize a local Git repository. `auto_commit` defaults true. A
transient `.history.lock/` directory is acquired around the entire
stage/diff/commit transaction. An atomic marker filename records the owner PID
and unique token.
Contenders retry until the configurable wait timeout. A lock is eligible for
reclamation only after the stale threshold and when its recorded owner process
is no longer live; the token is rechecked before removal. Dropping an owner
removes only the lock carrying that owner's token.

Focused mutations supply wiki-relative paths. The history layer validates that
each path is non-empty, relative, and traversal-free, then runs path-scoped
`git add -A`, cached-diff detection, and `git commit --only`. Thus one command
does not consume unrelated changes staged by another command or a human. A few
bulk operations retain an empty-path whole-wiki commit contract.

Git identity is `wookie <wookie@localhost>`. With
`history.fail_on_commit_error = false`, history failures emit a warning after
the underlying mutation; true propagates the error. `history.commit_sessions`
controls durable session/activity/notification history, while receiver inbox
markers are always ignored.

## CLI and machine output

The CLI exposes wiki lifecycle, page CRUD and graph operations, bounded
retrieval, protocols, ingest, critique, audit/status, transactional publish,
rules proposals, locks, Obsidian, plugin, configuration, session, notification,
and MCP server commands. `--wiki` and `--json` are global.
Read-style and mutation results have JSON variants; errors explain the next
valid action where possible.

`session start --id-only` is intended for assigning `WOOKIE_SESSION`. Session
prune defaults dry-run. Section configuration and unlock operations keep their
explicit user-approval barriers.

## MCP protocol and results

`wookie serve` implements newline-delimited JSON-RPC 2.0 over stdio with
`initialize`, `ping`, `tools/list`, and `tools/call`. Tools mirror the CLI's
wiki, page, prime/search, protocol, publish, ingest, critique, section-lock,
doctor/status, configuration, session, and notification surfaces. `wiki` and
`cwd` fields select the resolution context.

The command layer is called with JSON output. On success an object result is
carried once in `structuredContent`; the text block is deliberately compact:

```json
{
  "content": [{"type": "text", "text": "Structured result available in structuredContent."}],
  "structuredContent": {"...": "..."},
  "isError": false
}
```

If a command returns a non-object JSON value it is wrapped as `{"value": ...}`;
non-JSON text remains unchanged in `content` and is wrapped as
`{"message": ...}`. Errors return readable text and `isError: true`. This avoids
doubling bounded prime, search, and publish payloads while preserving
human-visible text results and an object-valued structured result.

## Agent integration freshness

Both integrations embed `templates/guidance.md` and the package version:

- Claude: `~/.claude/skills/wookie/SKILL.md` with a version metadata field.
- Codex: an idempotently replaced `<!-- wookie:start/end -->` block in
  `~/.codex/AGENTS.md` with a version marker.

`plugin status [target]` compares the complete managed content to what this
binary would generate and reports `current`, `stale`, or `missing`, including
path and expected version in JSON. `--strict` fails when any selected target is
not current. Installation uses the same portable atomic replacement used by
wiki configuration.

## Health and CI

`doctor` reports broken links, orphans, stubs, missing summaries, malformed
frontmatter, source provenance, section requirements, recovery state, and code
movement since ingest; `--fix` performs safe mechanical repair and `--strict`
exits nonzero while error diagnostics remain. `status` renders the same audit
as a concise operator dashboard.

CI runs formatting and Clippy with warnings denied on stable Rust under Linux,
then runs locked all-target/all-feature tests on Linux, macOS, and Windows. The
workflow has read-only repository permission, cancels superseded branch runs,
and caches Rust build artifacts.

## Non-goals

- No HTML renderer or hosted UI.
- No embedded model or autonomous judgment.
- No remote wiki synchronization.
- No automatic cross-clone matching by repository remote.
- No process interruption or push transport; agents poll cooperatively.
