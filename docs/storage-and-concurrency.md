# Storage and concurrency

Wookie keeps project knowledge and coordination state in a project-specific
wiki under `WOOKIE_HOME` (normally `~/.wookie`). The project checkout contains
no generated wiki files, and linked Git worktrees resolve to the main
checkout's wiki.

## Wiki and session layout

```text
$WOOKIE_HOME/
  config.toml
  <wiki-slug>/
    .git/
    .gitignore
    wookie.toml
    pages/
      ...
    sessions/
      <session-id>/
        session.toml
        activity/
          <activity-id>.toml
        notifications/
          <notification-id>.md
        inbox/
          <notification-id>.read
          <notification-id>.dismissed
        inbox.toml            # optional legacy read state
```

`session.toml` is the immutable starting record. Later heartbeats and status
changes are append-only activity files; wookie derives `updated_at`,
`last_seen_at`, and status when loading the session. Notification Markdown has
TOML frontmatter followed by its full body.

Inbox acknowledgement files are receiver-local state. They and legacy
`inbox.toml` files are gitignored, so acknowledgement markers themselves do not
enter wiki history. With `sessions.heartbeat_on_activity` enabled, a read or
dismiss may also append a debounced durable activity event; that event is
committed when automatic session history is enabled. Notifications are durable
and follow the same history policy.

The managed `.gitignore` entries are:

```gitignore
.history.lock
.unlocks.toml
.unlocks/
.publish.lock
.publish-journal.json
.ingest-reconciliation-recovery.json
.cache/
proposals/rules/
pages/.obsidian/
sessions/*/inbox.toml
sessions/*/inbox/
sessions/**/.*.tmp-*
```

## Concurrent session operations

Publishing a notification, recording activity, and acknowledging a notice
each atomically publishes a complete new file without replacement. No two readers rewrite a
shared inbox map, so concurrent read/dismiss operations cannot lose one
another's acknowledgement. Notification and activity ids include time and
process entropy; idempotent notices use a stable, source-scoped id.

Legacy `inbox.toml` state remains readable. New acknowledgements always use
the append-only marker layout.

Polling is cooperative and local: wookie scans notification directories and
does not run a daemon or push into an agent process. Every scan reads bounded
frontmatter and validates compact metadata for each retained notice, so header
work remains O(retained notices). It does not load bodies unless a text query
survives the metadata filters and does not already match the summary. Direct
reads and idempotent-publish comparisons also load the selected body;
dismissal uses metadata only.
Use session retention to bound large histories; non-text filters reduce the
result set but do not avoid the initial metadata scan.

## Corruption isolation

Collection operations isolate malformed entries. Session list/prune skip
unusable session records; notification scans used by poll, session show, and
doctor skip unusable frontmatter/metadata and duplicate ids. JSON and MCP
results include structured warning objects with `path` and `message`; human
output reports the number skipped.

Notification bodies are intentionally lazy. A body-only decoding or size
problem can therefore remain visible as valid compact metadata in an ordinary
poll, and doctor does not diagnose it. A text filter records a warning and
skips a notice whose body cannot be loaded. A direct read or idempotent
comparison that needs that body fails. Direct session operations likewise fail
when the requested session's base record is corrupt. A missing, duplicated, or
improperly targeted notification also fails directly.

## Path containment

Wiki slugs must be simple lowercase names, and a wiki must be a real direct
child of `WOOKIE_HOME`. Page ids are validated relative paths under `pages/`.
Managed storage checks every existing path component and refuses symlinks,
absolute paths, parent traversal, hidden page-id segments, and non-directory
ancestors. Session and notification ids are validated before becoming paths.
Global configuration loading also refuses a `config.toml` symlink.

These checks apply in the storage layer, so CLI and MCP writes receive the
same protection. A symlink planted beneath `pages/` or `sessions/` cannot
redirect a managed write outside the wiki.

## Atomic mutable writes

Mutable TOML and Markdown files are replaced through a same-directory
temporary file: wookie creates the temporary exclusively, writes and syncs
it, preserves existing permissions, then atomically replaces the target.
Unix uses `rename`; Windows uses replace-existing `MoveFileExW` semantics.
Temporary files are removed on failure, and Unix directory syncing is
best-effort after the rename.

Page moves preflight all backlink rewrites, keep the old page present while
rewrites are applied, and roll completed rewrites back if a later step fails.
The old page is removed only after all inbound links have been updated. The
preflight also verifies that the source, destination, and every rewritten
backlink page are writable; a backlink in a locked rules section stops the
move before mutation. `doctor --fix` similarly refuses to rewrite a locked
rules page.

## Serialized, path-scoped Git history

When `auto_commit` is enabled, the `.history.lock/` directory serializes the
complete `git add` plus `git commit` transaction across wookie processes. Focused
mutations stage and commit only their wiki-relative paths, so one agent's
commit does not absorb another agent's files or unrelated entries already in
the index.

`history.lock_timeout_ms` controls how long a contender waits. A lock older
than `history.lock_stale_seconds` is reclaimed only when its recorded owner PID
is no longer live; stale age by itself is not permission to steal it. The lock
also carries an atomic owner marker with a unique token, which is checked
before reclamation and when an owner drops its guard so an old owner does not
remove a replacement lock. An interrupted empty lock directory is reclaimable
after the stale threshold. The lock itself is transient and gitignored.
`history.commit_sessions` can exclude session operations from history while
leaving page history enabled.

By default, a Git commit failure leaves the successful storage mutation in
place and emits a warning. Set `history.fail_on_commit_error=true` to make the
command return an error instead. See [Configuration](configuration.md) for all
history controls.
