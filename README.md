# wookie

Wookie is a local, Markdown, LLM-first project wiki and an agent-to-agent
coordination layer. One Rust binary owns the storage format, linking rules,
project resolution, session lifecycle, and Git history, so every agent working
on a project sees the same knowledge and can announce work that may affect the
others.

Wikis live outside the project checkout under `~/.wookie/`. Nothing is added to
the repository being documented.

## Quick start

```sh
cargo install --path .

cd /path/to/project
wookie init
wookie prime --query "understand the architecture"
wookie read index --expand
```

Install guidance for the agents you use:

```sh
wookie plugin install claude
wookie plugin install codex
wookie plugin status --strict
```

Start a coordination session and keep its id in the environment:

```sh
export WOOKIE_SESSION="$(wookie session start --agent codex --id-only)"
wookie notifications
```

After meaningful work, publish enough metadata for another agent to judge its
relevance without opening the full notice:

```sh
printf '%s\n' 'Retry callers must now handle the terminal state.' | \
  wookie notify \
    --summary "Changed retry exhaustion behavior" \
    --kind code-change \
    --importance high \
    --paths src/retry.rs,tests/retry.rs
```

`notify`, `notifications`, `notification read`, and `notification dismiss` use
`WOOKIE_SESSION` when `--session` is omitted.

## How it works

`wookie init` registers the current project. After that, commands resolve a
wiki from the current directory; linked Git worktrees resolve to the main
checkout's wiki, and `--wiki <slug>` selects one explicitly from anywhere.
Independent clones remain independent unless both roots are registered.

Each wiki is its own Git repository. Mutations auto-commit by default. Focused
operations stage only the paths they own, and a per-wiki lock serializes the
complete `git add` plus `git commit` transaction across concurrent processes.

```text
~/.wookie/
  config.toml                       global registry and defaults
  my-project/
    wookie.toml                     roots and sparse per-wiki overrides
    .gitignore
    .history.lock/                  transient Git transaction lock (ignored)
    .publish.lock/                  shared writer/publisher lock (ignored)
    .publish-journal.json           interrupted publish recovery (ignored)
    .unlocks/                       per-section rules state (ignored)
    .cache/retrieval-v1.json        disposable retrieval parse index (ignored)
    protocols/                      project-scoped Markdown page scaffolds
      findings/finding.md
    pages/
      architecture/overview.md
      code/src/scheduler.md
      workflow/checks.md
    sessions/
      session-20260721-143052-7f3a/
        session.toml                immutable base session record
        activity/                   append-only status/heartbeat events
          activity-....toml
        notifications/              append-only Markdown notices
          notify-....md
        inbox/                      receiver-local acknowledgements (ignored)
          notify-....read
          notify-....dismissed
        inbox.toml                  legacy read state, if present (ignored)
```

`WOOKIE_HOME` overrides `~/.wookie`, which is useful for isolated automation
and tests. Home-directory discovery supports the usual Unix and Windows
environment variables.

## Wiki methodology

Wookie separates mechanical work from judgment:

- The binary owns page format, links, sections, locking, indexing, storage,
  and deterministic worklists.
- The invoking LLM decides what matters, writes useful summaries, fills
  documentation, and executes critique or ingest instructions.
- Descriptive knowledge stays on demand. Normative rules are locked and
  checkable. A small pinned set is included in every context prime.

Pages are Markdown with tool-owned frontmatter, a standalone first-paragraph
summary, and `[[wikilinks]]`. Links inside code spans and fences are ignored.
Broken links are an intentional growth mechanism: `wookie expand` creates
every eligible stub, then prints a bounded worklist for filling them. The
default limit and token budget inherit `retrieval.search_limit` and
`retrieval.search_tokens`; `--limit` and `--tokens` override only the response,
never stub creation. Omission totals and a continuation command keep every
stub reachable. Use `wookie expand --all` only when an exhaustive current
worklist is intentional. Unknown frontmatter fields round-trip unchanged.

### Sections and rules

Every page can be filed in a top-level section declared in `wookie.toml`. With
no custom section table, these defaults apply:

| Section | Kind | Purpose |
|---|---|---|
| `architecture/` | info | Structure, boundaries, subsystem interactions |
| `code/` | info | Module reference seeded by ingest |
| `decisions/` | info | Why the system works this way |
| `guides/` | info | Build, test, release, and debug procedures |
| `findings/` | info | Audit findings, remediation, verification evidence |
| `style/` | rules | Code style and review conventions |
| `workflow/` | rules | Commit, branch, PR, review, and release process |

Rules sections are locked by default. Writes, deletes, moves, and stub creation
cannot change them until `wookie unlock <section>` opens a short window. Agents
must obtain explicit user permission first; MCP additionally requires
`user_approved: true`. Each rules section should have a `checks` page.
`wookie critique` assembles the changed files, checks pages, rules, and an
output contract for the invoking agent to execute. Its default is a bounded
map with exact `wookie read <id>` continuations; `--all` explicitly includes
complete normative bodies. Configure the default ceiling with
`audit.critique_tokens` or override one run with `--tokens`.

The lock also covers indirect mutations: a move that would rewrite a backlink
inside a locked rules page fails before the move starts, and `doctor --fix`
does not repair a locked rules page.

### Retrieval, ingest, and pinned pages

`wookie ingest --level quick|standard|deep` inventories a project, seeds code
stubs, and emits a documentation worklist. Once that work is actually done,
rerun ingest and use its exact receipt-bound `data.mark_command`;
`--mark-reconciled` (alias: `--mark`) never performs a blind HEAD write. It
rejects a changed worklist, wiki, policy, or target commit and requires the
audit error gate to pass before recording the project commit. Worklist display
is bounded by default (`--limit`, `--tokens`); `--all` is the explicit
exhaustive opt-in, while every receipt always covers the complete worklist.
An ambiguous metadata commit blocks further mutations until the operator runs
`wookie ingest --recover accept|rollback`. Rollback restores the recorded
pre-mark config; when the exact mark commit already landed, it preserves
history by appending a verified compensating config-only commit. Interrupted
recovery can be retried safely.
Later runs map code changes to pages through each page's `sources` frontmatter
and return confidence-ranked reconciliation worklists in JSON.

`wookie prime --query "the actual task"` is the normal task-start command. Its
complete output has a configured token ceiling and contains standing
instructions, section summaries, ranked page suggestions, selection reasons,
telemetry, and a continuation cursor. `wookie search` is ranked and bounded by
default. `context` remains the exhaustive catalog; `search --all` visits every
matching page without a response budget while retaining at most five matching
body lines per page.

Prime returns a query-independent `state_hash` for `--since` deltas and a
query/options/state `context_hash` for cursor binding. Reusing a state hash
with a new task omits unchanged section structure but still returns standing
instructions and freshly ranked suggestions.

Pins are `instruction` (concise normative content), `summary` (the standalone
first paragraph), or `discoverable` (metadata plus an explicit `wookie read`
command, never inline content). Legacy `pin: true` behaves as an instruction
pin. If an instruction page contains `## Agent instructions`, prime extracts
that section instead of its rationale. Standing pins must contain real,
non-stub text. An oversized instruction set fails visibly; standing rules are
never silently truncated.

### Protocols, findings, and transactional publishing

Protocols are inert Markdown templates stored under the wiki's `protocols/`
directory. They support fixed `id`, `title`, and `date` substitutions—no hooks,
dependencies, or executable code. Discover them with `wookie protocol list`
and create a page with `wookie new <id> --protocol <name>`.

Findings use ordinary pages, source metadata, links, and controlled tags such
as `finding`, `severity/high`, and `status/open`. The built-in
`findings/finding` protocol keeps this workflow extensible without a second
database.

`wookie publish --check` validates a strict multi-page manifest and returns a
compact plan, changed-line excerpts, provenance errors, link/orphan effects,
and applicable rules within `publish.output_tokens` (4,000 by default). Use
`--tokens <n>` for a one-off bound or explicitly opt into exhaustive page
images with `--full-diff`. The preview's `review_token` can be supplied to
`--apply --expect-plan <token>` to reject any manifest, catalog,
configuration, policy, revision, or plan drift.
`--apply` revalidates after taking the shared mutation lock, journals exact
before/after images plus the full catalog, configuration, effective policy,
and lock-control state, writes the complete plan, and creates one path-scoped
history unit from the recorded pre-finalizer HEAD. Dirty target paths, no-op
plans, and rendered pages over the canonical 16 MiB limit are rejected.
Ordinary failures roll back content, metadata, permissions, and Git index
state; unrelated hook mutations or ambiguous history retain the journal and
require explicit `--recover rollback` or `--recover accept`. Rules use
`rules propose`, `review`, and explicitly approved `apply`; review creates a
cryptographic receipt, and only that exact revalidated plan is authorized
inside the transaction without opening a section-wide unlock window.

`wookie status` is the compact operator dashboard. `doctor`, `critique`,
`expand`, and `ingest` expose stable `wookie.report/v1` JSON for CI; provenance
checks can validate page sources against an explicit project revision.

## Cross-session coordination

Sessions are project-scoped identities named
`session-<UTC-date>-<UTC-time>-<unique-id>`. The base `session.toml` stays
immutable; heartbeats, closes, and debounced command activity are separate
append-only events. This avoids shared-file lost updates when agents work in
parallel.

Notifications are immutable Markdown records with TOML metadata:

- source session, one-line summary, kind, importance, timestamp, and affected
  paths;
- optional receiving session ids (`--to`), routing metadata, and a retry-safe
  idempotency key;
- optional Git branch, commit, worktree, and dirty paths, attached by default;
- an optional Markdown body that ordinary polling does not load.

Polling scans and validates bounded TOML frontmatter for every retained notice,
then returns compact metadata. Bodies are loaded only for a direct read, an
idempotent-publish comparison, or a text filter whose query did not
already match the summary.

An empty target list is a broadcast. Normal polling routes a targeted notice
only to the named active sessions; targeting is not confidentiality or an
authorization boundary for users who can access the local wiki files. A source
cannot target itself. Reusing an idempotency
key in one source session returns the original notification when the payload
matches and fails if it differs.

Each receiver acknowledges a notice by creating one `.read` or `.dismissed`
marker. These files are local and Git-ignored, so concurrent acknowledgements
cannot overwrite one another or pollute durable history. The legacy shared
`inbox.toml` format remains readable.

New sessions default to a zero-hour lookback, so old notifications do not flood
their unread queue. Configure or override the lookback when history matters;
`notifications --all` explicitly includes acknowledged and pre-session
history. Polling can filter by source, kind, minimum importance, path prefix,
branch, metadata, timestamps, age, and text, then bound and order the results.

Malformed notification metadata and malformed session entries are reported as
warnings during collection scans while valid entries continue to work. A
direct operation on a corrupt session or notice fails; body-only corruption is
discovered only when an operation loads that body. `session list` can find
stale sessions, and `session prune` previews its exact deletion set unless
`--apply` is passed. It prunes closed sessions by default and supports
age/cutoff and keep-latest guards.

Delivery is cooperative polling, not push. Installed guidance checks after
session start, before overlapping edits, after substantial work, and before
commit or handoff.

## Configuration

Configuration has two layers:

1. `~/.wookie/config.toml` holds the wiki registry and complete global
   defaults.
2. `<wiki>/wookie.toml` holds project roots plus sparse per-wiki overrides.

Per-wiki `sessions.*`, `history.*`, `retrieval.*`, `audit.*`, and `publish.*`
fields are optional individually. Setting one does not freeze the rest:
omitted fields continue to inherit future global default changes.
Configuration is typed, rejects unknown fields, and is validated before it is
written.

```sh
wookie config keys
wookie config show --effective
wookie config get sessions.poll_limit --effective
wookie config set sessions.poll_limit 50
wookie config set audit.critique_tokens 6000
wookie config unset sessions.poll_limit

wookie config set --global defaults.sessions.poll_limit 50
wookie config show --global
```

`--string` prevents TOML parsing for a literal string. Registry keys and ingest
state have dedicated lifecycle commands and cannot be changed through generic
configuration. Because `sections.*` can change rules and their locks, those
edits require `--user-approved` after explicit approval.

Retrieval configuration controls prime/search budgets, limits, excerpt size,
and per-section diversity. Retrieval token budgets have an immutable
1,000,000-token ceiling, and result/per-section limits cannot exceed 1,000.
Audit configuration controls source provenance and
the compact critique token ceiling;
publish configuration controls base-revision enforcement, orphan policy, and
the normal check/review output budget.
Session configuration controls feature enablement, lookback, stale and
retention windows, pruning, polling limits, payload limits, Git context,
heartbeats, and default kind/importance. History configuration controls lock
timeouts, stale-lock recovery, whether session operations are committed, and
whether a Git history error warns or fails the command. `auto_commit` can be
set globally or per wiki.

## Storage safety and concurrency

All managed paths are checked below the real wiki directory. Existing path
components may not be symlinks, so a symlink under `pages/`, `protocols/`, or `sessions/`
cannot redirect a read, write, delete, or prune outside the wiki. Page ids and
wiki slugs reject traversal and absolute paths.

Mutable files use same-directory atomic replacement. Unix uses rename;
Windows uses replace-existing `MoveFileExW`, preserving the cross-platform
atomic-write contract. Immutable session records are fully written and synced
before atomic no-replace publication (a hard link on Unix and rename on
Windows), so readers never observe partial records. Page
moves preflight every backlink, keep both ids resolvable while rewriting, and
roll back completed rewrites on failure; if rollback itself is incomplete,
both page ids are retained and the error explains the state.

Git history uses a transient lock directory with an atomic owner marker plus
configurable wait and stale thresholds. Age alone never steals a valid lock:
reclamation also requires the
recorded owner process to be gone, and ownership is rechecked before removal.
Path-scoped commits keep concurrent commands from accidentally staging or
labelling each other's changes. History failures warn by default and can be
made fatal with `history.fail_on_commit_error`.

## Command overview

```text
wookie init [slug]                    create and register a wiki
wookie list                           list wikis
wookie prime --query "..."            bounded task-aware startup map
wookie context                        exhaustive page catalog
wookie toc                            list every page by section
wookie read <id> [--expand[=N]]       read a page and linked summaries
wookie new / write / rm / mv          page lifecycle
wookie expand [<id>] [--limit N]       create all stubs; bound only the worklist
wookie expand [<id>] --all             explicitly list every current stub
wookie search / links                 bounded retrieval and graph relationships
wookie ingest [--level L]             emit a bounded receipt-bound worklist
wookie ingest --mark --expect-worklist SHA256  record a validated sync point
wookie critique [--revision REV]      bounded rules-review map (use --all for full bodies)
wookie doctor [--fix|--strict]        check or repair wiki health
wookie status [--strict]              compact wiki health dashboard
wookie protocol list|show|write       project-scoped page scaffolds
wookie publish [--check|--apply]      transactional multi-page publication
wookie rules propose|review|apply     explicitly approved rules workflow
wookie unlock / lock                  approved rules-section write window

wookie session start|list|show        session lifecycle and discovery
wookie session heartbeat|close        append activity/status events
wookie session prune                  preview or apply retention cleanup
wookie notify                         publish a notification
wookie notifications                  poll and filter compact metadata
wookie notification read|dismiss      acknowledge one notification

wookie config show|get|set|unset|keys inspect or edit typed configuration
wookie roots                          edit registered project roots
wookie rename-wiki / remove-wiki      wiki lifecycle
wookie obsidian [--print]              open pages as an Obsidian vault
wookie plugin install|status           manage agent guidance
wookie serve                           MCP server over stdio
```

Use `--json` for machine-readable CLI output. Use `wookie <command> --help`
for every flag and filter.

## MCP

`wookie serve` implements newline-delimited JSON-RPC 2.0 over stdio. Its tools
mirror page, wiki, session, notification, critique, doctor, ingest, lock, and
configuration operations. Tools that resolve a wiki accept optional `wiki`
and `cwd` fields.

```sh
claude mcp add wookie -- wookie serve
```

Successful tool calls expose JSON objects once through object-valued
`structuredContent`; `content` carries only a short pointer so large bounded
prime, search, and publish results do not consume the model context twice.
Human-only results remain unchanged in `content`. Failed calls set
`isError: true` and retain a readable diagnostic.

## Agent integrations

Both integrations are generated from `templates/guidance.md`:

- Claude Code: `~/.claude/skills/wookie/SKILL.md`
- Codex: a managed `<!-- wookie:start/end -->` block in
  `~/.codex/AGENTS.md`

The generated content carries the Wookie package version. `wookie plugin
status [claude|codex]` reports each integration as `current`, `stale`, or
`missing`; `--strict` exits nonzero unless every selected integration is
current. Re-run `plugin install` after upgrading Wookie.

## Guides

- [Guide index](docs/README.md)
- [Session lifecycle](docs/sessions.md)
- [Publishing notifications](docs/notifications.md)
- [Inbox polling and triage](docs/inbox-triage.md)
- [Agent and MCP coordination](docs/agent-coordination.md)
- [Session maintenance and pruning](docs/session-maintenance.md)
- [Configuration reference](docs/configuration.md)
- [Storage, safety, and concurrency](docs/storage-and-concurrency.md)
- [Bounded retrieval](docs/retrieval.md)
- [Page protocols](docs/protocols.md)
- [Transactional publishing](docs/publishing.md)
- [Audit and CI](docs/audit-and-ci.md)
- [Rules and findings](docs/rules-and-findings.md)

## Development and CI

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo test --locked --all-targets --all-features
```

GitHub Actions runs format and Clippy checks on Linux and the full test suite
on Linux, macOS, and Windows. See [SPEC.md](SPEC.md) for the precise model and
invariants.
