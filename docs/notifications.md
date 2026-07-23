# Publishing notifications

`wookie notify` creates a short, append-only Markdown record of meaningful
work. Other sessions first see compact metadata and decide whether the full
details matter to them.

## Publish a short notice

With `WOOKIE_SESSION` set:

```sh
wookie notify \
  --summary "Changed retry exhaustion behavior" \
  --kind code-change \
  --importance high \
  --paths src/retry.rs,tests/retry.rs
```

Without the environment variable, add `--session <session-id>`. `--summary`
is required, one line, and should let another agent judge relevance without
opening the body. `kind` and `importance` default to the configurable
`sessions.default_kind` and `sessions.default_importance` values.

## Add detailed Markdown

Pipe a body when another agent may need implementation details, constraints,
or next actions:

```sh
wookie notify \
  --summary "Retry policy now stops after three attempts" \
  --kind code-change \
  --paths src/retry.rs <<'EOF'
The retry loop now treats the third failed attempt as terminal.

Consumers that assumed four total attempts should update their tests before
merging related work.
EOF
```

Without piped details, the summary becomes the notification body. Empty piped
input is ignored.

## Notification kinds

| Kind | Use it for |
|---|---|
| `code-change` | Modified behavior, interfaces, files, or tests. |
| `decision` | A design or implementation choice other work should follow. |
| `blocker` | Work that cannot continue without action or coordination. |
| `handoff` | Completed or partial work another session should take over. |
| `warning` | A discovered risk, conflict, regression, or sharp edge. |
| `note` | Useful context that does not fit a stronger category. |

Importance is `low`, `normal`, or `high`. High means another active session
may need to stop or adjust; normal is worth checking at the next poll; low is
safe to defer. Importance is routing metadata, not an automatic push rule.

## Affected paths

Pass comma-separated project-relative paths:

```sh
--paths src/scheduler.rs,src/retry.rs,tests/retry.rs
```

Paths are deduplicated and help recipients identify overlapping work. They
also support prefix filtering, for example `notifications --path src/retry`.
Omit them for decisions or handoffs that are not file-specific.

## Target specific sessions

Omit `--to` to broadcast to every other session in the wiki. To send only to
known receivers:

```sh
wookie notify \
  --summary "Parser handoff is ready" \
  --kind handoff \
  --to session-20260721-150000-a1,session-20260721-150100-b2
```

Targets must name active sessions, cannot include the source session, and are
deduplicated. A closed or unknown target is rejected. A non-target cannot
poll, read, or dismiss the notice, and the source session never sees its own
notice in its inbox.

## Retry safely with an idempotency key

Use a stable key when a host may retry the same publication:

```sh
wookie notify \
  --summary "Migration completed" \
  --kind code-change \
  --idempotency-key migration-v2-complete
```

Keys are scoped to the source session. Repeating the same key and payload
returns the original notification instead of creating another file. Reusing
the key with a different summary, body, kind, importance, paths, targets, or
custom metadata fails.

## Attach custom routing metadata

Repeat `--metadata KEY=VALUE` for one-line, caller-defined fields:

```sh
wookie notify \
  --summary "API schema changed" \
  --metadata component=gateway \
  --metadata ticket=ENG-421
```

Recipients can filter with matching `--metadata KEY=VALUE` flags. Multiple
pairs are ANDed. Duplicate CLI keys are rejected. Configurable limits bound
the number and byte size of keys and values.

## Automatic Git context

When `sessions.include_git_context` is true, wookie best-effort captures the
current branch, commit, worktree root, and up to
`sessions.max_git_dirty_paths` dirty paths. Capture is restricted to the
wiki's registered project root (including linked worktrees that share its Git
common directory). Invoking `--wiki` from an unrelated repository therefore
does not leak that repository's branch, commit, paths, or worktree into the
notification. Omit context for one notice with:

```sh
wookie notify --summary "Decision only" --kind decision --no-git-context
```

Outside the registered project or a Git worktree, context is simply absent.
Recipients can filter notices by captured branch.

## Limits and validation

The configurable limits cover session agent/label bytes, summary/body bytes,
affected path count and size, targets, idempotency-key bytes, metadata count
and sizes, dirty-path count, and Git branch/commit/worktree sizes. Summaries,
paths, keys, metadata, and captured Git string fields that require one-line
routing data reject embedded newlines. See
[Configuration](configuration.md) for defaults.

Notifications from a closed or unknown source session are rejected.
Malformed storage elsewhere does not block creation unless it conflicts with
the requested idempotency key.

## When to notify

Publish after a meaningful code change, shared decision, discovered blocker,
important warning, or explicit handoff. Avoid emitting a notice for every
small edit; summaries should remain a useful coordination feed.
