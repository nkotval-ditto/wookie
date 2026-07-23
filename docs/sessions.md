# Session lifecycle

Sessions give each active agent a stable identity inside one project wiki.
Create one at the beginning of a task, keep its generated id in
`WOOKIE_SESSION`, and close it when the task ends.

All session and notification commands fail when the effective
`sessions.enabled` setting is false.

## Start a session

Run from a project directory registered to a wiki:

```sh
wookie session start --agent codex --label "retry-policy implementation"
```

The result resembles:

```text
Started session 'session-20260721-143052-7f3a...'. Set WOOKIE_SESSION to reuse this id.
```

`--agent` names the host or agent type and defaults to `unknown`. `--label` is
an optional one-line purpose. If the current directory cannot resolve a wiki,
add the global `--wiki <slug>` option.

A convenient shell setup prints only the id and exports it:

```sh
export WOOKIE_SESSION="$(wookie session start \
  --agent codex \
  --label 'retry-policy implementation' \
  --id-only)"
```

`notify`, `notifications`, `notification read`, `notification dismiss`,
`session heartbeat`, and `session close` use `WOOKIE_SESSION` when their
explicit session argument is omitted. An explicit `--session` or positional id
takes precedence. MCP calls do not inherit this shell variable; they take an
explicit `session` field.

For structured CLI output:

```sh
wookie session start --agent codex --json
```

The object includes `id`, `agent`, optional `label`, `created_at`,
`updated_at`, `last_seen_at`, the configured lookback and activity behavior,
and `status`.

## Choose initial history

By default, a new session starts caught up: notices created before it do not
appear as unread. Include a prior window when the new agent should see recent
coordination:

```sh
wookie session start --lookback-hours 6
```

The default comes from `sessions.initial_lookback_hours`. A poll can
temporarily override the stored window with `wookie notifications
--lookback-hours <hours>`. `wookie notifications --all` ignores the window and
shows all history visible to that session. Both startup and per-poll overrides
share the immutable 100-year lookback ceiling, so a command cannot bypass the
configuration safety bound.

## Configure activity behavior per session

Ordinary notification operations record debounced activity according to
`sessions.heartbeat_on_activity` and `sessions.activity_debounce_seconds`.
Disable those automatic records for one session with:

```sh
wookie session start --no-heartbeat
```

An explicit `wookie session heartbeat --force` still records activity. See
[Session maintenance](session-maintenance.md) for stale-session and heartbeat
workflows.

## List sessions

```sh
wookie session list
wookie session list --status active --agent codex --limit 20
wookie session list --status active --limit 20 --cursor 20
wookie session list --active
wookie session list --stale
```

Sessions appear newest first and the response defaults to 100 entries. Filters include comma-separated `--status` and
`--agent`, case-insensitive label substring via `--label`, RFC3339
`--created-after` and `--active-after`, activity-derived `--active` or
`--stale`, `--limit`, and `--oldest-first`. When more matches exist, both
human and JSON output return a numeric continuation for `--cursor`; preserve
the original filters and ordering when continuing. One response can request at
most 1,000 sessions; larger `--limit` values are rejected instead of silently
clamped. The active/stale boundary is the effective
`sessions.stale_after_minutes` value.

Malformed session entries are skipped. Human output reports a warning count;
JSON includes path-specific `warnings`, total/omission counts, and
`scan_complete`. Filesystem enumeration also has a generous absolute ceiling
(200,000 entries and 32 MiB of session records), with a lower per-session
activity ceiling. Crossing one is reported explicitly instead of silently
claiming a complete result. Pruning fails closed unless its bounded storage
scan is complete.

## Inspect a session

```sh
wookie session show "$WOOKIE_SESSION"
wookie session show "$WOOKIE_SESSION" --limit 20 --cursor 20
```

This prints session metadata plus at most 20 recent notification summaries by
default (maximum 1,000). Long summaries are compacted to 512 bytes, and large
path, target, Git, and routing-metadata collections are represented only by
counts. JSON output returns totals, omission counts, a numeric continuation,
storage-scan completeness, and bounded warning details. If the requested
session's own base record is corrupt, `session show` fails instead of returning
a partial session.

## Record a heartbeat

```sh
wookie session heartbeat
wookie session heartbeat --force
```

The id is optional when `WOOKIE_SESSION` is set. Normal heartbeats honor the
session's debounce and heartbeat preference; `--force` bypasses both.

## Close a session

```sh
wookie session close
```

Closing appends a status activity record. Closed sessions remain available as
operational history, but cannot publish new notifications or be named as an
explicit notification target. Start a new session instead of reopening an old
one.

## Retain or prune old sessions

`wookie session prune` previews closed sessions eligible for removal and
changes nothing until `--apply` is supplied. Retention can use inactivity age,
an exact cutoff, and a keep-newest count. See
[Session maintenance](session-maintenance.md) before applying deletion.

## Storage behavior

Each session has its own append-oriented directory:

```text
sessions/<session-id>/
  session.toml
  activity/<activity-id>.toml
  notifications/<notification-id>.md
  inbox/<notification-id>.read
  inbox/<notification-id>.dismissed
```

`session.toml`, activity, and published notifications are durable wiki state.
Inbox markers are local and gitignored, so concurrent reads and dismissals do
not rewrite a shared file or create history noise. Legacy `inbox.toml` state
is still honored. Details are in
[Storage and concurrency](storage-and-concurrency.md).
