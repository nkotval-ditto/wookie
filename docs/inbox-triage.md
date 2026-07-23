# Inbox polling and triage

Notifications use cooperative polling: wookie writes durable records, while
each agent checks compact metadata at useful points in its workflow. Wookie
does not interrupt an already-running agent process.

## Poll unread notifications

With `WOOKIE_SESSION` set:

```sh
wookie notifications
```

Otherwise pass `--session <session-id>`. Default output includes only
unacknowledged notifications from other sessions, targeted to this receiver,
and inside the session's initial lookback window:

```text
notify-20260721-151422-b91c...
  From: session-20260721-143052-7f3a...
  Summary: Changed retry exhaustion behavior
  Kind: code-change
  Importance: high
  Paths: src/retry.rs, tests/retry.rs
```

The body is deliberately omitted. Use summary, source, kind, importance,
paths, targets, Git context, and custom metadata to judge relevance.

An ordinary poll reads bounded frontmatter for every retained notification and
validates its compact metadata, but does not load Markdown bodies. This keeps
large bodies off the normal triage path, although the header scan still grows
with retained notification count.

Results are newest-first by default. Add `--oldest-first` when chronological
processing is more useful. `--limit` defaults to `sessions.poll_limit` and
cannot exceed that configured cap. Every page reports total, returned, and
omitted counts; when more matches remain, repeat the same filters, order, and
limit with the returned `--offset <n>` continuation. `--newest-first` remains
accepted for explicitness and compatibility.

## Filter before opening bodies

Filters can be combined; a result must satisfy all configured filter groups:

```sh
wookie notifications \
  --from session-20260721-143052-7f3a \
  --kind code-change,warning \
  --min-importance normal \
  --path src/retry \
  --branch feature/retries,main \
  --metadata component=scheduler \
  --max-age-hours 12 \
  --text exhaustion \
  --limit 25 \
  --oldest-first
```

| Filter | Match behavior |
|---|---|
| `--from <ids>` | Source session is one of the comma-separated ids. |
| `--kind <kinds>` | Kind is one of the comma-separated values. |
| `--min-importance <level>` | Importance is at least `low`, `normal`, or `high`. |
| `--path <prefix>` | At least one affected path begins with a repeatable prefix. |
| `--branch <branches>` | Captured Git branch is one of the comma-separated values. |
| `--metadata KEY=VALUE` | Custom metadata contains every repeated exact pair. |
| `--created-after <time>` | Created at or after the RFC3339 timestamp. |
| `--created-before <time>` | Created at or before the RFC3339 timestamp. |
| `--max-age-hours <hours>` | Created within the rolling age window. |
| `--text <query>` | Case-insensitive substring in summary or full body. |

Text filtering checks the summary first. If it does not match, wookie loads the
body only after the other filters match. It does not return that body or mark
the notice read.

## Control the history window

A session stores its initial lookback when it starts. Override that window for
one unread poll with:

```sh
wookie notifications --lookback-hours 24
```

This does not rewrite the session. The window is ignored with `--all`, which
includes visible pre-session history and acknowledged notices.

## Read a relevant notification

```sh
wookie notification read notify-20260721-151422-b91c
```

This returns the full Markdown body and creates a receiver-local `.read`
marker. The notification no longer appears in unread polls. Repeating the
read is safe.

## Dismiss an irrelevant notification

```sh
wookie notification dismiss notify-20260721-151422-b91c
```

Dismissal records the relevance decision without returning the full body by
creating a `.dismissed` marker. Wookie validates the selected compact metadata
record without loading its body before acknowledging. It remains visible in
history.

Concurrent reads and dismissals create independent marker files rather than
rewriting a shared inbox, so acknowledgements are not lost. If both markers
exist, dismissal is reported as the state.

## Inspect all history

```sh
wookie notifications --all
```

`--all` includes visible read, dismissed, and pre-existing notifications and
reports each available state. It still excludes notifications published by
the receiving session and notices targeted only to other sessions. Use
`wookie session show <id>` to inspect what one source published.

## Corrupt storage warnings

Malformed notification frontmatter/metadata, duplicate ids, and unusable
legacy inbox files are skipped rather than blocking valid metadata results.
Human output reports a warning count. JSON output includes a `warnings` array
with `path` and `message`, alongside `session`, `unread_only`, and structured
`notifications`.

Because bodies are lazy, invalid body encoding or body-size violations may not
appear in an ordinary poll or doctor result. A text filter that needs an
invalid body records a warning and skips that notice while returning other
valid matches. A direct read fails for an invalid body and direct access also
fails for a missing, duplicated, or improperly targeted id.

## Recommended polling points

Poll:

1. Immediately after starting the session.
2. Before editing files that other agents may also touch.
3. After a substantial tool-heavy or implementation phase.
4. Before committing, pushing, or handing work to another agent.
5. Before closing the session.

High-importance blockers and overlapping paths should usually be read
immediately. Low-importance notices with unrelated paths can usually be
dismissed.

## Machine-readable triage

All commands support the global `--json` option:

```sh
wookie notifications --json
```

MCP `notifications` exposes the same filters with arrays named
`source_sessions`, `kinds`, `path_prefixes`, and `branches`, an object-valued
`metadata`, and the same time/text/limit fields. MCP read and dismiss calls
require explicit `session` and `id` fields.
