# Inbox polling and triage

Notifications use cooperative polling: wookie writes durable records, while
each agent checks for new metadata at useful points in its workflow. Wookie
does not interrupt an already-running agent process.

## Poll unread notifications

```sh
wookie notifications --session session-20260721-151005-a832
```

The default output includes only unseen notifications from other sessions:

```text
notify-20260721-151422-b91c
  From: session-20260721-143052-7f3a
  Summary: Changed retry exhaustion behavior
  Kind: code-change
  Importance: high
  Paths: src/retry.rs, tests/retry.rs
```

The notification body is deliberately omitted. Use the metadata to decide
whether the item affects the current task.

## Read a relevant notification

```sh
wookie notification read \
  notify-20260721-151422-b91c \
  --session session-20260721-151005-a832
```

This returns the full Markdown body and records the notification as read for
the receiving session. It will no longer appear in the default unread poll.

## Dismiss an irrelevant notification

```sh
wookie notification dismiss \
  notify-20260721-151422-b91c \
  --session session-20260721-151005-a832
```

Dismissal records the relevance decision without returning the full body. The
item stops appearing in the unread poll but remains in history.

## Inspect all history

```sh
wookie notifications \
  --session session-20260721-151005-a832 \
  --all
```

`--all` includes read, dismissed, and notifications that existed before the
receiving session started. It still excludes notifications published by the
receiving session itself; inspect those with `wookie session show <id>`.

## Recommended polling points

Poll:

1. Immediately after starting the session.
2. Before editing files that other agents may also touch.
3. After a substantial tool-heavy or implementation phase.
4. Before committing, pushing, or handing work to another agent.
5. Before closing the session.

High-importance blockers and overlapping affected paths should usually be read
immediately. Low-importance notices with unrelated paths can usually be
dismissed.

## Machine-readable triage

All commands support global `--json` output:

```sh
wookie notifications --session session-20260721-151005-a832 --json
```

The response contains the receiving session id, whether the result is
unread-only, and an array of structured notification metadata.
