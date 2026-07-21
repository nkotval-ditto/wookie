# Publishing notifications

`wookie notify` creates a short, append-only Markdown record of meaningful
work. Other active sessions first see its compact metadata and decide whether
the full details matter to them.

## Publish a short notice

```sh
wookie notify \
  --session session-20260721-143052-7f3a \
  --summary "Changed retry exhaustion behavior" \
  --kind code-change \
  --importance high \
  --paths src/retry.rs,tests/retry.rs
```

`--session` and `--summary` are required. The summary must fit on one line and
should let another agent judge relevance without opening the body.

## Add detailed Markdown

Pipe a body when another agent may need implementation details, constraints,
or next actions:

```sh
wookie notify \
  --session session-20260721-143052-7f3a \
  --summary "Retry policy now stops after three attempts" \
  --kind code-change \
  --paths src/retry.rs <<'EOF'
The retry loop now treats the third failed attempt as terminal.

Consumers that assumed four total attempts should update their tests before
merging related work.
EOF
```

Without piped details, the summary is also used as the notification body.

## Notification kinds

Choose the kind that best describes why another agent should care:

| Kind | Use it for |
|---|---|
| `code-change` | Modified behavior, interfaces, files, or tests |
| `decision` | A design or implementation choice other work should follow |
| `blocker` | Work that cannot continue without action or coordination |
| `handoff` | Completed or partially completed work another session should take over |
| `warning` | A discovered risk, conflict, regression, or sharp edge |
| `note` | Useful context that does not fit a stronger category |

The default is `note`.

## Importance

Use `low`, `normal`, or `high`. The default is `normal`.

- `high` means another active session may need to stop or adjust its work.
- `normal` covers meaningful changes worth checking at the next polling point.
- `low` is informational and safe to defer.

Importance is a hint, not an automatic delivery rule. Receiving agents still
judge relevance using all metadata.

## Affected paths

Pass comma-separated project-relative paths:

```sh
--paths src/scheduler.rs,src/retry.rs,tests/retry.rs
```

Accurate paths are the cheapest way for another agent to detect overlapping
work. Omit the option for decisions or handoffs that are not file-specific.

## When to notify

Publish after a meaningful code change, shared decision, discovered blocker,
important warning, or explicit handoff. Avoid emitting a notice for every
small edit; notification summaries should remain a useful coordination feed.

Notifications from a closed or unknown session are rejected.
