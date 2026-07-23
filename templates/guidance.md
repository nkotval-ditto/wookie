# wookie: the project wiki

This project may have a wookie wiki: a local, markdown, LLM-first knowledge base
managed by the `wookie` CLI. Wikis live under `~/.wookie/`, one per project,
resolved automatically from your working directory (worktrees resolve to the
main checkout's wiki). Nothing lives inside the repo itself.

## When to use it

- Starting a task on a project: run `wookie prime --query "$TASK"` once, using
  a concise description of the actual task. It returns bounded standing
  instructions, the section map, and ranked page suggestions. Use `wookie
  context` only when you intentionally need the exhaustive catalog.
- If `wookie prime` resolves a wiki and coordination is enabled, start one
  session and retain its id in `WOOKIE_SESSION`. Poll at task start, before
  overlapping edits, and before committing or handing off. If no wiki exists
  or `sessions.enabled` is false, continue without a session.
- Answering questions about the project: `wookie read <id> --expand` first.
  `--expand` inlines a bounded set of linked summaries (depth at most 5, up to
  100 pages), so one command usually gives enough context and reports omissions.
- After learning something durable (architecture, a gotcha, a decision, how a
  subsystem works): capture it. Knowledge that dies with the conversation is
  the failure mode wookie exists to prevent.
- Before finishing a task where you touched the wiki: `wookie doctor`.

## Commands

```
wookie prime --query "$TASK"  # bounded standing rules + relevant page map
wookie context                 # explicit exhaustive catalog
wookie session start --agent A --id-only # create an id for WOOKIE_SESSION
wookie notifications           # compact unread notices for WOOKIE_SESSION
wookie notification read N     # read relevant notice + mark read
wookie notification dismiss N  # dismiss irrelevant notice
wookie session heartbeat       # keep a long-running session visibly active
wookie notify --summary "..."  # tell other sessions what changed
wookie read <id> [--expand]    # read a page; --expand inlines linked summaries
wookie search <query>          # ranked, bounded results; --all is exhaustive
wookie links <id>              # outlinks + backlinks
wookie new <id> <<'EOF' ...    # create a page (body via stdin/heredoc)
wookie write <id> <<'EOF' ...  # replace a page's body (also clears stub status)
wookie write <id> --append     # append instead of replace
wookie expand [<id>]           # create all broken-link stubs; bounded worklist
wookie mv <old> <new>          # rename; inbound links rewritten automatically
wookie ingest [--level L]      # sync wiki with codebase (see below)
wookie critique [--since ref]  # check current changes against the rules sections
wookie doctor [--fix]          # health check: broken links, orphans, stubs
wookie status                  # concise operator health dashboard
wookie protocol list           # discover project-scoped page scaffolds
wookie publish --check < plan  # validate a multi-page change without mutation
wookie rules propose < plan    # begin the reviewed rules-change lifecycle
wookie unlock <section>        # ONLY with explicit user permission (see below)
wookie plugin status --strict  # detect stale installed agent guidance
wookie list / wookie init      # all wikis / register a new one for this project
```

Add `--wiki <slug>` if you are outside the project directory, and `--json` for
machine-readable output.

## Where knowledge goes (sections)

File every page under the best-fitting section (defaults below; `wookie
context` shows the wiki's actual set, configurable in its wookie.toml):

- `architecture/` — system structure, boundaries, subsystem interactions
  (`architecture/overview` is required)
- `code/` — module-by-module reference; ingest seeds these
- `decisions/` — why things are the way they are, one page per decision
- `guides/` — how to do common tasks: build, test, release, debug
- `findings/` — structured audit findings, remediation, verification evidence
- `style/` — code style, naming, idioms, review conventions
- `workflow/` — how to commit, branch, PR, review, release; process rules

Unfiled pages are allowed but flagged by doctor.

Sections come in two kinds. `info` sections hold descriptive knowledge.
`rules` sections (by default `style/` and `workflow/`) hold normative,
checkable content and behave differently:

- They are LOCKED (see below), so rules don't drift when code gets documented.
- Each needs a `<section>/checks` page telling a reviewer how to verify the
  rules: Scope (what artifacts they apply to), Procedure (commands to run,
  what to inspect), Violations (what bad looks like), Exceptions.
- `wookie critique` checks work against them. Run it before committing or
  opening a PR: it returns a briefing (rules + checks pages + the changed
  files) that YOU then execute, reporting violations per its output contract.

## Locked sections: ask before you touch

Rules sections are locked. Any new/write/rm/mv into one fails until unlocked.
The rule is absolute: NEVER run `wookie unlock <section>` unless the user has
explicitly approved changing that section's content in the current
conversation. Documenting code, filling stubs, or fixing doctor findings is
NOT permission to edit rules. When you believe a rule page needs changing,
propose the edit to the user and wait. After approval: `wookie unlock
<section>`, make the edit, then `wookie lock <section>` (it also auto-relocks
after 15 minutes).

Indirect edits obey the same lock: a move that would rewrite a backlink in a
rules page fails, and `wookie doctor --fix` will not repair a locked rules
page. Do not treat either command as permission to unlock the section.

## Pinned pages (always-on instructions)

Use `--pin-level instruction` for concise standing orders, `--pin-level
summary` when only a page's first paragraph must stay visible, and `--pin-level
discoverable` to always highlight metadata plus a `wookie read` command without
inlining content. Legacy `--pin` means `instruction`. An instruction page may
isolate its normative text under `## Agent instructions`; `prime` extracts that
section. Follow returned instructions for the whole session. Instruction and
summary pins must contain real non-stub text. Keep them short: Wookie fails
clearly if they exceed the configured instruction budget instead of silently
dropping rules. Discoverable pins do not consume that budget; everything else
is fetched on demand through read/search.

## Protocols and checked publication

Protocols are inert, project-scoped Markdown scaffolds stored with the wiki.
Inspect them with `wookie protocol list/show`; create a page with `wookie new
<id> --protocol <name>`. They have no hooks or executable code and never bypass
section locks.

For a coordinated multi-page change, preview a strict manifest with `wookie
publish --check`; mutation requires `--apply`, revalidates the reviewed base,
and uses a rollback journal. Rules changes go through `wookie rules
propose/review/apply`. Never pass `--user-approved` unless the user explicitly
approved that exact rules change in the current conversation.

## Conventions (enforced by the tool; do not fight them)

- Page ids are lowercase kebab-case paths: `scheduler`, `internals/retry-policy`.
- Link pages with `[[page-id]]` or `[[page-id|display text]]`. Link liberally;
  a link to a page that doesn't exist yet is fine and becomes a stub via
  `wookie expand`.
- The first paragraph of every page must be a standalone summary, readable
  without the rest of the page (it is what `--expand` and Obsidian hover
  previews show). House style: **bold the key noun or claim** that opens it.
- Pages about code include a `File:` line right after the summary paragraph,
  e.g. ``File: `src/scheduler/retry.rs` `` — and set `--sources` to match.
- Use Obsidian callouts for asides: `> [!note]` for gotchas and design
  rationale, `> [!bug]` for known problems, `> [!tip]` for usage hints.
- Structure longer pages with `## Role`, `## Key files`, `## Related` style
  headings instead of one wall of text.
- Never edit frontmatter timestamps or files under `~/.wookie` directly; go
  through wookie commands so history and metadata stay correct (wikis render
  as Obsidian vaults via `wookie obsidian`, so format discipline matters).

## Syncing with the codebase (the ingest workflow)

`wookie ingest` keeps the wiki tracking the code. When the user asks to
"ingest", "index", or "document the codebase", or when doctor reports the code
has changed since the last ingest, run it and then EXECUTE the worklist it
prints — the command only scaffolds; you do the reading and writing.

- First run: `wookie ingest --level quick|standard|deep`. It inventories the
  project, seeds `code/<module>` stubs (with `sources` pointing at their
  directories), and prints a worklist. quick = index + module overviews;
  standard (default) = + submodules and key flows; deep = + per-file/type
  pages, invariants, edge cases.
- Later runs: `wookie ingest` diffs the code since the recorded sync point and
  lists stale pages (whose `sources` changed), uncovered changes, and new
  modules. Update each stale page after reviewing the diff.
- When you finish a worklist: `wookie ingest --mark` records the current
  commit as the sync point. Do not mark before the work is done.
- When writing pages about code, set `--sources src/path,other/path` so
  future ingests can flag the page when that code changes.

## Growing the wiki (the expand workflow)

1. While writing a page, link concepts you mention: `[[run-lifecycle]]`.
2. Run `wookie expand` — every eligible broken link becomes a stub page, and
   you get a bounded worklist with totals and omission guidance. Use
   `--limit`/`--tokens` to tune the response; use `--all` only when you need the
   exhaustive current stub list. Output limits never limit stub creation.
3. Fill each stub you have knowledge for: `wookie read <id>` to see what links
   to it, then pipe a body with `wookie write <id>`. Writing clears the stub.
4. Leave stubs you can't fill; they are honest TODOs for the next session.

## Coordinating concurrent agent sessions

Sessions are operational history under the wiki's `sessions/` directory, not
curated pages. Start one only after `wookie prime` finds a wiki and
`wookie config get sessions.enabled --effective` is true. Keep its id in the
environment so later commands can omit `--session`:

```sh
export WOOKIE_SESSION="$(wookie session start --agent codex --id-only)"
```

Replace `codex` with the current agent name. In PowerShell, use
`$env:WOOKIE_SESSION = (wookie session start --agent codex --id-only)`. Poll
immediately, before editing files another session may touch, after a substantial
tool-heavy phase, and before committing or handing off. Use `wookie session
heartbeat` during long work that otherwise has no wookie activity.

Notification listings intentionally contain only compact metadata. Judge
relevance from the summary, kind, importance, and affected paths. Read relevant
items with `wookie notification read`; dismiss irrelevant ones so they do not
repeat. Publish after meaningful changes, decisions, blockers, or handoffs.
Use `--to <session>` when only specific sessions need it and a stable
`--idempotency-key` when retrying publication. Put a short summary in
`--summary`, affected files in `--paths`, and pipe fuller Markdown details only
when they help another agent act.

Before stopping, send a `--kind handoff` notice when unfinished context matters,
then close with `wookie session close`. After upgrading wookie, run `wookie
plugin status --strict` and reinstall any target it reports stale or missing.
Notifications cannot interrupt a running agent by themselves; the skill's
checkpoint polling is the delivery mechanism.
