# Session maintenance

Session activity is append-only, so a session's `last_seen_at` can advance
without rewriting its original metadata. Use activity filters to find stale
work and retention commands to control long-term storage.

## Heartbeats and activity

Publishing, polling, reading, and dismissing notifications record activity
when `sessions.heartbeat_on_activity` is enabled. Activity is debounced by
`sessions.activity_debounce_seconds`, preventing routine polls from producing
one history event each.

For a long-running agent that has no notification traffic, record an explicit
heartbeat:

```sh
wookie session heartbeat "$WOOKIE_SESSION"
```

`--force` bypasses both the debounce interval and a session started with
`--no-heartbeat`:

```sh
wookie session heartbeat --force
```

The positional id is optional for `heartbeat` and `close` when
`WOOKIE_SESSION` is set.

## Find active and stale sessions

```sh
# Activity within sessions.stale_after_minutes
wookie session list --active

# No activity within that threshold
wookie session list --stale

# Other filters can be combined
wookie session list \
  --status active \
  --agent codex \
  --label retry \
  --created-after 2026-07-20T00:00:00Z \
  --active-after 2026-07-21T12:00:00Z \
  --limit 25
```

`--status` and `--agent` accept comma-separated values. `--label` is a
case-insensitive substring. Timestamps are RFC3339. Lists are newest-first by
default; add `--oldest-first` to reverse them. `--active` and `--stale` are
mutually exclusive.

## Preview retention

Pruning is a dry run unless `--apply` is explicitly supplied:

```sh
# Preview closed sessions inactive for at least 30 days
wookie session prune

# Preview a custom age while preserving the five newest sessions
wookie session prune --older-than-days 14 --keep-latest 5

# Preview against an exact RFC3339 activity cutoff
wookie session prune --inactive-before 2026-06-01T00:00:00Z
```

When neither age flag is given, wookie uses `sessions.retention_days`, or 30
days if no retention is configured. By default only closed sessions qualify.
`--include-active` permits inactive active sessions to qualify too, but an age
or cutoff still bounds the operation.

Review the exact ids, then repeat with `--apply`:

```sh
wookie session prune --older-than-days 30 --keep-latest 5 --apply
```

Applying a prune removes each selected session directory, including its
session metadata, activity events, published notifications, and local inbox
markers. With automatic history enabled, the removal is recorded as a
path-scoped wiki commit. This is intentionally irreversible at the working
tree level; Git history may retain committed durable records.

## Automatic pruning

Set both a retention period and auto-pruning if every new session should first
clean up eligible old closed sessions:

```sh
wookie config set sessions.retention_days 30
wookie config set sessions.auto_prune_on_start true
```

Automatic pruning applies changes rather than previewing them, and runs only
when a retention period is configured. A per-wiki
`sessions.retention_days = 0` disables an inherited global retention period,
which also prevents auto-pruning.

## Corrupt entries

List and prune operations skip malformed or non-directory session entries and
return warnings instead of hiding all valid sessions. JSON output includes a
`warnings` array with the affected path and error. Inspect or repair those
paths before relying on retention to remove them.

The MCP equivalents are `session_heartbeat`, `session_list`, and
`session_prune`. MCP pruning uses `dry_run: true` by default and
`closed_only: true` by default.
