# Agent and MCP coordination

Wookie's installed guidance teaches compatible agents to establish a session,
poll notifications, judge relevance, publish meaningful updates, and close the
session. CLI and MCP share the same validated storage and command layer.

## Install or refresh agent guidance

```sh
wookie plugin install claude
wookie plugin install codex
```

Claude receives a wookie `SKILL.md`. Codex receives an idempotently managed
block in its `AGENTS.md`. Both are generated from `templates/guidance.md`.

Installed guidance contains a version marker. Check one or both integrations:

```sh
wookie plugin status
wookie plugin status codex
wookie plugin status --strict
```

`--strict` exits non-zero if an integration is missing or stale, which is
useful in setup checks. Re-run `plugin install` to refresh it.

## Suggested CLI workflow

```sh
wookie prime --query "describe the task you are starting"
export WOOKIE_SESSION="$(wookie session start --agent codex --id-only)"
wookie notifications

# Perform work, polling at coordination checkpoints.
wookie notify --summary "Changed retry exhaustion behavior" \
  --kind code-change --paths src/retry.rs,tests/retry.rs

wookie notifications
wookie session close
```

`WOOKIE_SESSION` removes repetitive flags for CLI session operations. It is
not global agent discovery: each conversation must retain or export its own
generated id. Explicit ids remain available for scripts and take precedence.

Start a session only after `wookie prime --query "$TASK"` confirms that a
project wiki exists and returns its bounded, task-relevant map. Poll before
overlapping edits and before commit, push, handoff, and close. Publish
meaningful changes rather than every small edit. Use exhaustive `wookie
context` only when you deliberately need the full catalog.

## MCP coordination tools

Run the newline-delimited JSON-RPC server with `wookie serve`. Coordination
tools mirror the CLI:

| MCP tool | CLI equivalent |
|---|---|
| `session_start` | `wookie session start` |
| `session_list` | `wookie session list` |
| `session_show` | `wookie session show` |
| `session_heartbeat` | `wookie session heartbeat` |
| `session_close` | `wookie session close` |
| `session_prune` | `wookie session prune` |
| `notify` | `wookie notify` |
| `notifications` | `wookie notifications` |
| `notification_read` | `wookie notification read` |
| `notification_dismiss` | `wookie notification dismiss` |

Every coordination tool accepts optional `wiki` and `cwd` resolution fields.
Session-scoped MCP calls take an explicit `session`; they do not read
`WOOKIE_SESSION` from a client shell.

Example publication:

```json
{
  "session": "session-20260721-143052-7f3a",
  "summary": "Changed retry exhaustion behavior",
  "kind": "code-change",
  "importance": "high",
  "paths": ["src/retry.rs", "tests/retry.rs"],
  "targets": ["session-20260721-151005-a832"],
  "idempotency_key": "retry-exhaustion-v1",
  "metadata": {
    "component": "scheduler",
    "ticket": "ENG-421"
  },
  "include_git_context": true,
  "body": "The third failed attempt is now terminal."
}
```

`session_start` accepts `agent`, `label`, `lookback_hours`, and
`heartbeat_on_activity`. `session_list` supports status, agent, label,
creation/activity, ordering, and limit fields. `session_prune` defaults to
`dry_run: true` and `closed_only: true`; explicitly set `dry_run: false` only
after reviewing the preview.

The `notifications` tool mirrors CLI filtering with `source_sessions`,
`kinds`, `min_importance`, `path_prefixes`, `branches`, exact object-valued
`metadata`, creation/age/text fields, lookback, limit, and ordering.

## MCP configuration tools

| MCP tool | Purpose |
|---|---|
| `config_show` | Show global, stored per-wiki, or effective configuration. |
| `config_get` | Read one dotted key. |
| `config_set` | Set and validate a dotted TOML value. |
| `config_unset` | Remove a local override or reset a global default. |
| `config_keys` | List supported dotted keys. |

Use `global: true` for global settings (whose keys begin with `defaults.`) and
`effective: true` only on per-wiki reads. For `sections.*` writes,
`user_approved: true` is required and may only be asserted after explicit user
approval. `config_set` accepts a typed JSON `value`; set `string: true` to force
literal string interpretation.

## Structured MCP results

Every successful MCP tool call includes `content` and an object-valued
`structuredContent`. JSON object output is exposed directly and `content`
contains only a short pointer instead of duplicating the full object into the
model context. Other JSON values are wrapped as `{"value": ...}`. Human-only
text remains unchanged in `content` and is wrapped as `{"message": "..."}` in
`structuredContent`. Coordination and configuration tools emit JSON-backed
results, including arrays of path-specific storage warnings where applicable.

Errors set `isError: true` and return readable error text. Clients should use
`structuredContent` for routing and automation while retaining `content` for
older MCP hosts.

## Delivery semantics

- Publishing creates one append-only Markdown file under the source session.
- Target omission broadcasts to all other sessions; a target list narrows
  normal polling visibility but is not an authorization boundary.
- Polling scans local wiki storage; no daemon pushes messages into an agent.
- Read and dismissal state belongs to the receiver and is gitignored.
- Activity, notice, and acknowledgement files are fully written before atomic
  no-replace publication for concurrent safety.
- Malformed session records and notification metadata are skipped by
  collection scans with structured warnings. Body-only errors surface when a
  direct read, text filter, or idempotent comparison loads that body; dismiss
  uses metadata only.

See [Storage and concurrency](storage-and-concurrency.md) for durability and
locking details, and [Configuration](configuration.md) for limits and policy.
