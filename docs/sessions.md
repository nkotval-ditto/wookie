# Session lifecycle

Sessions give each active agent a stable identity inside one project wiki.
Create one at the beginning of a task, retain its generated id, and close it
when the task ends.

## Start a session

Run from a project directory registered to a wiki:

```sh
wookie session start --agent codex --label "retry-policy implementation"
```

The result resembles:

```text
Started session 'session-20260721-143052-7f3a'. Keep this id for notify and inbox commands.
```

Pass that id to every notification command for the rest of the task. `--agent`
describes the host or agent type; `--label` is an optional human-readable task
name. If the current directory cannot resolve a wiki, add `--wiki <slug>`.

For structured output:

```sh
wookie session start --agent codex --label "retry-policy implementation" --json
```

The JSON object contains `id`, `agent`, `label`, `created_at`, `updated_at`, and
`status`.

## List sessions

```sh
wookie session list
```

Sessions appear newest first with their id, active or closed status, agent,
creation time, and optional label. Use JSON when another tool will consume the
result:

```sh
wookie session list --json
```

## Inspect a session

```sh
wookie session show session-20260721-143052-7f3a
```

This prints the session metadata and compact details for every notification it
published. It is useful when reconstructing what another agent did without
reading every notification body.

## Close a session

```sh
wookie session close session-20260721-143052-7f3a
```

Closed sessions remain available as operational history, but they cannot
publish new notifications. Start a new session instead of reopening an old
one.

## Storage behavior

Each session has its own directory:

```text
sessions/<session-id>/
  session.toml
  inbox.toml
  notifications/
```

`session.toml` and published notifications are durable wiki history.
`inbox.toml` is local read state and is gitignored, so reading or dismissing a
notification does not create a commit.

A newly created session starts caught up to existing notifications. This keeps
old history from flooding its unread inbox; use `wookie notifications --all`
when historical context is wanted.
