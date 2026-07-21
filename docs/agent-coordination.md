# Agent and MCP coordination

Wookie's installed guidance teaches compatible agents to establish a session,
poll notifications, judge relevance, publish meaningful updates, and close the
session. CLI and MCP use the same storage and command layer.

## Install or refresh agent guidance

After installing a version of wookie that includes coordination support,
refresh the integration for the agent host:

```sh
wookie plugin install claude
wookie plugin install codex
```

Claude receives a wookie `SKILL.md`. Codex receives an idempotently managed
block in its `AGENTS.md`. Both are generated from
`templates/guidance.md`, so the workflow stays consistent.

## Suggested agent workflow

```text
wookie context
      |
wookie session start --agent <host>
      |
wookie notifications --session <id>
      |
perform work, polling at coordination checkpoints
      |
wookie notify --session <id> --summary "..."
      |
wookie notifications --session <id>
      |
wookie session close <id>
```

The agent must retain the generated session id in its conversation context.
Separate CLI processes cannot infer that they belong to the same agent
conversation, so the id is explicit on notification commands.

## MCP tools

`wookie serve` exposes these coordination tools:

| MCP tool | CLI equivalent |
|---|---|
| `session_start` | `wookie session start` |
| `session_list` | `wookie session list` |
| `session_show` | `wookie session show` |
| `session_close` | `wookie session close` |
| `notify` | `wookie notify` |
| `notifications` | `wookie notifications` |
| `notification_read` | `wookie notification read` |
| `notification_dismiss` | `wookie notification dismiss` |

Every tool accepts the normal optional `wiki` and `cwd` resolution arguments.
Session and notification tools additionally take the same structured fields as
their CLI equivalents.

Example notification arguments:

```json
{
  "session": "session-20260721-143052-7f3a",
  "summary": "Changed retry exhaustion behavior",
  "kind": "code-change",
  "importance": "high",
  "paths": ["src/retry.rs", "tests/retry.rs"],
  "body": "The third failed attempt is now terminal."
}
```

## Delivery semantics

The current implementation is local and poll-based:

- Publishing creates an append-only Markdown notification.
- Polling scans notifications from other sessions in the same wiki.
- Read and dismissal state belongs to the receiving session.
- No background daemon pushes messages into an active agent.
- A later host integration may add hooks or watchers without changing the
  notification format.

Because each notification has its own file, concurrent publishers do not
append to a shared log. Per-session inbox updates are atomically replaced to
avoid partially written state.
