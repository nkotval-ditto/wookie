# wookie: the project wiki

This project may have a wookie wiki: a local, markdown, LLM-first knowledge base
managed by the `wookie` CLI. Wikis live under `~/.wookie/`, one per project,
resolved automatically from your working directory (worktrees resolve to the
main checkout's wiki). Nothing lives inside the repo itself.

## When to use it

- Starting a task on a project: run `wookie context` once. If a wiki exists you
  get every page with a one-line description; skim it before exploring code.
- Answering questions about the project: `wookie read <id> --expand` first.
  `--expand` inlines the summary of every linked page, so one command usually
  gives full context.
- After learning something durable (architecture, a gotcha, a decision, how a
  subsystem works): capture it. Knowledge that dies with the conversation is
  the failure mode wookie exists to prevent.
- Before finishing a task where you touched the wiki: `wookie doctor`.

## Commands

```
wookie context                 # digest: all pages + descriptions (start here)
wookie read <id> [--expand]    # read a page; --expand inlines linked summaries
wookie search <query>          # case-insensitive regex over ids/titles/tags/bodies
wookie links <id>              # outlinks + backlinks
wookie new <id> <<'EOF' ...    # create a page (body via stdin/heredoc)
wookie write <id> <<'EOF' ...  # replace a page's body (also clears stub status)
wookie write <id> --append     # append instead of replace
wookie expand [<id>]           # create stubs for broken [[links]], print worklist
wookie mv <old> <new>          # rename; inbound links rewritten automatically
wookie ingest [--level L]      # sync wiki with codebase (see below)
wookie critique [--since ref]  # check current changes against the rules sections
wookie doctor [--fix]          # health check: broken links, orphans, stubs
wookie unlock <section>        # ONLY with explicit user permission (see below)
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

## Pinned pages (always-on instructions)

Pages with `pin: true` are inlined in full by `wookie context`: they are the
wiki's standing orders (commit format, PR rules, hard constraints), not
reference material. Follow them for the whole session. Pin with
`wookie new/write <id> --pin`, unpin with `--unpin`. Keep the pinned set
small — a handful of short pages — or priming drowns in it. Everything else
stays unpinned and is fetched on demand via read/search.

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
2. Run `wookie expand` — every broken link becomes a stub page, and you get a
   worklist of all stubs.
3. Fill each stub you have knowledge for: `wookie read <id>` to see what links
   to it, then pipe a body with `wookie write <id>`. Writing clears the stub.
4. Leave stubs you can't fill; they are honest TODOs for the next session.
