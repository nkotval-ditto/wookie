# Getting started

Wookie keeps project knowledge and agent coordination outside the repository
being documented. Install one binary, register a project, and retrieve only the
knowledge relevant to the task in front of you.

## Install from this checkout

```sh
cargo install --locked --path .
```

Confirm that the installed agent guidance matches the binary:

```sh
wookie plugin install codex
wookie plugin install claude
wookie plugin status --strict
```

## Register a project

From the project you want Wookie to document:

```sh
cd /path/to/project
wookie init
```

The wiki is created under `WOOKIE_HOME`, which defaults to `~/.wookie`.
Wookie does not add documentation or coordination files to the project
checkout.

## Start a task

Prime with the actual task instead of loading the full catalog:

```sh
wookie prime --query "trace retry exhaustion and update its documentation"
```

Prime returns bounded standing instructions, section metadata, ranked page
suggestions, omissions, and exact continuation commands. Read authoritative
detail only when it is relevant:

```sh
wookie read architecture/overview --expand
wookie search "retry exhaustion" --limit 10
```

Use `wookie context` only when you deliberately need the exhaustive wiki
catalog.

## Coordinate with other agents

If sessions are enabled, retain one session id for the task:

```sh
export WOOKIE_SESSION="$(wookie session start --agent codex --id-only)"
wookie notifications
```

After a meaningful change, decision, blocker, or handoff:

```sh
printf '%s\n' "Retry callers now handle the terminal state." | \
  wookie notify \
    --summary "Changed retry exhaustion behavior" \
    --kind code-change \
    --importance high \
    --paths src/retry.rs,tests/retry.rs
```

Before stopping, run `wookie doctor` if you changed wiki content, publish a
handoff notice when unfinished context matters, and close the session.

## Next steps

- Learn the bounded retrieval contract in [Retrieving knowledge](retrieval.md).
- Configure retention, budgets, and behavior in [Configuration](configuration.md).
- Preview coordinated changes with [Transactional publishing](publishing.md).
- Understand the on-disk safety model in
  [Storage and concurrency](storage-and-concurrency.md).
