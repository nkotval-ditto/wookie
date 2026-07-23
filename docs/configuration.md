# Configuration

Wookie combines typed global defaults with sparse per-wiki overrides. A wiki
inherits every value it does not override, so changing one local setting does
not copy or freeze the rest of the global configuration.

## Configuration locations and precedence

The global registry and defaults are stored in:

```text
$WOOKIE_HOME/config.toml
```

`WOOKIE_HOME` defaults to `~/.wookie`. Each wiki has its own stored
configuration at:

```text
$WOOKIE_HOME/<wiki-slug>/wookie.toml
```

For configurable behavior, precedence is:

1. A value explicitly stored in the wiki's `wookie.toml`.
2. The corresponding value under `[defaults]` in the global `config.toml`.
3. Wookie's built-in default.

The `sessions`, `history`, `retrieval`, `audit`, and `publish` tables in
`wookie.toml` are sparse: each field is optional. Use `wookie config unset` to
remove one wiki override and resume inheritance.

## Inspect configuration

```sh
# Stored per-wiki configuration (overrides remain sparse)
wookie config show

# Resolved global defaults plus per-wiki overrides
wookie config show --effective

# The global registry and defaults
wookie config show --global

# Read one stored or resolved value
wookie config get sessions.poll_limit
wookie config get sessions.poll_limit --effective
wookie config get defaults.sessions.poll_limit --global

# Discover accepted dotted keys
wookie config keys
wookie config keys --global
```

The global `--json` option works with every configuration command.
`--effective` applies only to a resolved wiki, not to `--global` output.

## Set and unset values

`config set` accepts a dotted key and a TOML value:

```sh
# Per-wiki overrides
wookie config set sessions.poll_limit 50
wookie config set sessions.include_git_context false
wookie config set sessions.default_kind '"code-change"'
wookie config set retrieval.search_limit 15
wookie config set audit.source_provenance false
wookie config set audit.critique_tokens 6000
wookie config set publish.orphan_policy '"error"'

# Global defaults use the defaults.* prefix
wookie config set --global defaults.sessions.stale_after_minutes 240
wookie config set --global defaults.history.lock_timeout_ms 10000
wookie config set --global defaults.retrieval.prime_tokens 1800
wookie config set --global defaults.audit.critique_tokens 6000
wookie config set --global defaults.publish.output_tokens 5000

# Force literal string handling when shell/TOML quoting is inconvenient
wookie config set sessions.default_importance high --string

# Resume inheritance, or reset a global value to its built-in default
wookie config unset sessions.poll_limit
wookie config unset --global defaults.sessions.poll_limit
```

Values that parse as TOML booleans, integers, arrays, or tables retain that
type. A value that does not parse as TOML is stored as a string; `--string`
always stores it literally. The complete configuration is deserialized and
validated before it is written, and unknown fields are rejected.

Global edits are intentionally limited to `defaults.*`; wiki registration is
managed by `init`, `roots`, `remove-wiki`, and `rename-wiki`. Per-wiki `name`,
`project_roots`, and `last_ingest_commit` are likewise managed by dedicated
commands.

Changing `sections.*` can weaken a rules section or its lock. Wookie therefore
requires `--user-approved`, which must only be supplied after the user has
explicitly approved that change in the current conversation:

```sh
wookie config set sections.workflow.locked false --user-approved
```

## Core settings

| Per-wiki key | Built-in default | Meaning |
|---|---:|---|
| `description` | empty | One-line wiki description. |
| `auto_commit` | `true` | Automatically record wiki mutations in the wiki's own Git history. |

At global scope the key is `defaults.auto_commit`; `description` has no global
equivalent. `name`, `project_roots`, and `last_ingest_commit` also appear in a
wiki's stored configuration but are intentionally edited only by their
dedicated lifecycle commands. The global `wikis` registry is likewise not a
generic `config set` target.

Each `sections.<name>` table accepts `description` (empty string), `kind`
(`info`), `locked` (omitted; rules sections are locked by default), and
`required` (empty list). New wikis store the built-in `architecture`, `code`,
`decisions`, `guides`, `findings`, `style`, and `workflow` sections.
`architecture` requires an `overview` page; `style` and `workflow` are rules
sections and are locked by default. On legacy wikis, an empty section map
activates the same built-ins. Custom entries sparsely overlay those defaults,
so adding one section cannot silently remove the built-in rules sections.
Section changes require the approval guard described above.

## Session settings

Per-wiki keys begin with `sessions.`. Global versions begin with
`defaults.sessions.`.

| Key suffix | Built-in default | Meaning |
|---|---:|---|
| `enabled` | `true` | Enable session and notification commands for the wiki. |
| `initial_lookback_hours` | `0` | Default history window before a new session's creation time. Zero starts caught up. |
| `stale_after_minutes` | `120` | Inactivity threshold used by `session list --stale` and `--active`. Must be positive. |
| `activity_debounce_seconds` | `30` | Minimum interval between automatic append-only activity records. Must be positive. |
| `retention_days` | omitted | Default age for pruning and optional auto-pruning. A per-wiki override of `0` disables inherited retention. |
| `auto_prune_on_start` | `false` | When retention is configured, delete eligible old closed sessions before starting a session. |
| `poll_limit` | `100` | Default and maximum notification results per poll. Must be positive. |
| `max_summary_bytes` | `512` | Maximum one-line summary size. |
| `max_agent_bytes` | `128` | Maximum session agent/host identifier size. |
| `max_label_bytes` | `1024` | Maximum optional session-purpose label size. |
| `max_body_bytes` | `65536` | Maximum Markdown body size; must be at least the summary limit. |
| `max_paths` | `64` | Maximum affected paths on one notification. |
| `max_path_bytes` | `4096` | Maximum size of each affected or dirty Git path. |
| `max_targets` | `32` | Maximum target session ids on one notification. |
| `max_idempotency_key_bytes` | `256` | Maximum idempotency-key size. |
| `max_metadata_entries` | `32` | Maximum custom routing metadata pairs. |
| `max_metadata_key_bytes` | `64` | Maximum size of one metadata key. |
| `max_metadata_value_bytes` | `1024` | Maximum size of one metadata value. |
| `max_git_dirty_paths` | `256` | Maximum dirty paths captured in automatic Git context. |
| `max_git_branch_bytes` | `512` | Maximum captured Git branch size. |
| `max_git_commit_bytes` | `128` | Maximum captured Git commit identifier size. |
| `max_git_worktree_bytes` | `4096` | Maximum captured worktree path size. |
| `include_git_context` | `true` | Attach branch, commit, worktree, and dirty paths to notifications when available. |
| `heartbeat_on_activity` | `true` | Record debounced activity during publish, poll, read, and dismiss operations. |
| `default_kind` | `"note"` | Default notification kind: `code-change`, `decision`, `blocker`, `handoff`, `warning`, or `note`. |
| `default_importance` | `"normal"` | Default importance: `low`, `normal`, or `high`. |

All byte and count limits must be positive. `retention_days`, when configured
globally, must be positive; use a per-wiki value of `0` specifically to disable
inherited retention.

The configured maximums are enforced for new writes and while validating
stored records. Lowering one below an existing session or notification can
make that record unusable until the limit is restored; treat reductions as a
migration and prune or replace oversized records first.

Configuration cannot raise these limits without bound. Immutable ceilings are
10,000 poll results, affected paths, or targets; 100,000 captured dirty paths;
1,000 metadata entries; 64 KiB summaries, labels, or metadata values; 32 KiB
paths or worktree names; 4 KiB agent names, idempotency keys, metadata keys,
branches, or commit identifiers; and 16 MiB notification bodies. Notification
files, session scans, and Git-context capture have an additional absolute
32 MiB ceiling. Time ceilings are 100 years of initial lookback or retention,
10 years for stale detection, and 7 days for activity debounce. Values above a
ceiling are rejected during configuration validation.

## History settings

Per-wiki keys begin with `history.`. Global versions begin with
`defaults.history.`.

| Key suffix | Built-in default | Meaning |
|---|---:|---|
| `lock_timeout_ms` | `30000` | How long a process waits for the serialized Git-history lock. Must be positive. |
| `lock_stale_seconds` | `60` | Minimum age before a history lock whose recorded owner process is no longer live may be reclaimed. Must be positive. |
| `commit_sessions` | `true` | Include durable session, activity, notification, and prune changes in wiki history. |
| `fail_on_commit_error` | `false` | Fail the command when its history commit fails; otherwise preserve the mutation and emit a warning. |

`auto_commit=false` disables all automatic history commits, including session
commits, regardless of `history.commit_sessions`.

Stale age alone never authorizes reclamation of a valid lock. The history lock
stores an owner PID and unique token in an atomic marker; wookie also requires
the owner to be gone and rechecks the token before removal. An interrupted
empty lock directory is reclaimable after the stale threshold.

## Retrieval settings

Per-wiki keys begin with `retrieval.`. Global versions begin with
`defaults.retrieval.`. Command-line limits override these defaults for that one
invocation; they do not change stored configuration.

| Key suffix | Built-in default | Meaning |
|---|---:|---|
| `prime_tokens` | `1500` | Maximum estimated tokens for the complete default `prime` response. Range: 1–1,000,000. |
| `instruction_tokens` | `700` | Separate ceiling for standing instructions in `prime`. Range: 1–1,000,000 and no greater than `prime_tokens`. Discoverable pins do not consume it. |
| `search_limit` | `10` | Default maximum results for `prime`, bounded `search`, and each `expand` worklist category. Range: 1–1,000. |
| `search_tokens` | `2000` | Maximum estimated tokens for default bounded `search` and `expand` responses. Range: 1–1,000,000; expand requires at least 256. |
| `excerpt_lines` | `2` | Default matching body lines retained per search hit. Range: 1–20. |
| `max_per_section` | `5` | Default diversity cap for suggested pages from one section. Range: 1–1,000. |

`context`, `search --all`, and `expand --all` provide exhaustive catalog,
matching-page, and worklist coverage without these normal output budgets.
`search --all` still retains at most five matching body lines per page. Raising
a token budget can materially increase agent context cost; prefer a more
specific query or a continuation before raising the global default.

## Audit settings

Per-wiki keys begin with `audit.`. Global versions begin with
`defaults.audit.`.

| Key suffix | Built-in default | Meaning |
|---|---:|---|
| `source_provenance` | `true` | Validate code-page `sources` metadata and `File:` references against the selected project working tree or revision. Missing or mismatched declarations are errors when enabled. |
| `critique_tokens` | `4000` | Maximum estimated tokens for the compact critique briefing. Range: 256–1,000,000. Use `critique --all` only when explicitly requesting exhaustive output. |

Disabling provenance suppresses those source checks but does not disable link,
stub, orphan, rules-check, staleness, publication-recovery, or finding health
checks. Prefer leaving it enabled in CI and selecting an explicit
`--project-root` and `--revision` when reproducibility matters.

## Publish settings

Per-wiki keys begin with `publish.`. Global versions begin with
`defaults.publish.`.

| Key suffix | Built-in default | Meaning |
|---|---:|---|
| `require_base_revision` | `true` | Require every publish/rule manifest to name the exact wiki revision it was reviewed against. |
| `orphan_policy` | `"warn"` | Treat pages newly left without inbound links as `"warn"` diagnostics or blocking `"error"` diagnostics. |
| `output_tokens` | `4000` | Maximum estimated tokens for normal bounded publish-check and rules proposal/review responses. Must be at least `256`. |

`require_base_revision=false` is useful for simple local workflows but weakens
stale-plan detection; keep the default for concurrent or automated use.
`output_tokens` limits operator output, not manifest size or transactional
validation. An explicit exhaustive/full-diff command option may exceed it, so
avoid that mode in routine agent startup or CI logs.

## MCP configuration tools

`wookie serve` exposes `config_show`, `config_get`, `config_set`,
`config_unset`, and `config_keys`. They use the same keys, precedence,
validation, and global/local restrictions as the CLI. Set `global: true` for
global defaults and `effective: true` only for resolved wiki reads. For
`sections.*`, `config_set` and `config_unset` require
`user_approved: true` after explicit user permission.

Successful MCP calls include both text content and `structuredContent`; see
[Agent and MCP coordination](agent-coordination.md).
