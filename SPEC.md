# wookie spec v0.1

An LLM-first wiki manager. One Rust binary that owns the conventions so agents
never have to guess how a project's wiki works. Agents interact via CLI or
MCP; humans get the same commands.

## Storage layout

```
~/.wookie/                 # override with WOOKIE_HOME
  config.toml              # global registry + defaults
  <slug>/                  # one wiki per project
    wookie.toml            # per-wiki config
    pages/
      scheduler.md
      internals/retry-policy.md   # subdirs allowed; id = path sans .md
    sessions/
      session-20260721-143052-7f3a/
        session.toml
        inbox.toml                # local/gitignored read state
        notifications/
          notify-20260721-144210-a91c.md
```

- Page id = relative path under `pages/` without `.md`, lowercase-only
  (case-insensitive filesystems would otherwise alias ids past section locks)
- Each wiki is a git repo (`wookie init` runs `git init`; mutations
  auto-commit, best-effort and silent on failure)
- Wikis live outside project checkouts, so one wiki per project regardless of
  how many checkouts or worktrees exist

## Wiki resolution

1. `--wiki <slug>` flag (CLI) or `wiki` param (MCP)
2. cwd prefix match against `project_roots` (longest wins). Each wiki's own
   wookie.toml is the source of truth; the global config.toml only carries
   defaults. Edit roots with `wookie roots --add/--remove`
3. Worktree fallback: `git rev-parse --git-common-dir` gives the main
   checkout's path; match that instead. Any linked worktree resolves to the
   main checkout's wiki. Skipped silently outside git
4. Error listing known wikis

`wookie init` from a worktree registers the main worktree path. Non-git
projects work throughout; git is never required.

Two independent clones of one repo do not auto-merge onto one wiki (no shared
git dir). Escape hatches: add both paths to `project_roots`, or `--wiki`.

## Page format (hardcoded, checked by doctor)

```markdown
---
title: Run Lifecycle
description: One-line summary used in tocs and expanded reads
tags: [scheduler, core]
created: 2026-07-17
updated: 2026-07-17
---

First paragraph is a standalone summary readable without the rest of the page.

Body. Links: [[scheduler]] or [[internals/retry-policy|display text]].
```

- Frontmatter is tool-owned: timestamps bump on write, `status: stub` marks
  unfilled pages, writing real content clears it. Unknown frontmatter lines
  (e.g. Obsidian properties) round-trip untouched, and values are sanitized
  so they cannot break the block
- Wikilinks inside code fences or inline code spans are ignored (that is how
  pages document link syntax)
- Parsing is lenient; malformed frontmatter is a doctor finding, not a crash

## Expand: two meanings, two surfaces

- `wookie expand [<id>]` grows the wiki: every broken `[[link]]` (on one page
  or wiki-wide) becomes a `status: stub` page recording what links to it, and
  the output is a worklist telling the agent how to fill stubs
- `wookie read <id> --expand[=N]` inlines linked context: the page plus
  title/description/summary of every linked page, BFS to depth N, deduped

## CLI

init, list, toc, context, read, new, write (stdin body, `--append`), rm, mv
(rewrites inbound links), expand, search (case-insensitive regex, `--tag`),
links (out + back), doctor (`--fix` repairs frontmatter; also flags when code
moved past the last ingest), obsidian (open the wiki's `pages/` as an Obsidian
vault: registers it in Obsidian's own obsidian.json, then launches
`obsidian://open`; `--print` emits the URI, side-effect free), plugin install
claude|codex, serve. Global flags: `--wiki`, `--json`. Errors always say what
to run instead, because agents read errors.

## Sections: where knowledge goes

Sections are top-level namespaces declared per wiki in `wookie.toml`
(`[sections.<name>]` with `description` and optional `required = ["page"]`).
Built-in defaults (used verbatim by wikis with no `[sections]`): architecture
(requires `overview`), code, decisions, guides, style, workflow. They are
conventions with warnings, not walls:

- `toc`/`context` group pages by section and print each description, so
  priming teaches the wiki's shape; empty sections show as filing options
- `new` notes when a page id is unfiled; doctor flags unfiled pages and
  missing required pages; `index` is exempt
- ingest worklists reference sections (missing required pages become step 2)

## Section kinds, critique, and locks

Sections have a `kind`: `info` (default; descriptive) or `rules` (normative;
default for style and workflow). Rules sections get three behaviors:

- Checks page: each rules section needs `<section>/checks` describing how to
  verify its rules (scope, procedure, violations, exceptions). Doctor flags
  its absence; critique falls back to judgment and says so.
- `wookie critique [--section s] [--since ref] [--staged] [--paths ...]`:
  assembles a briefing (target files from git, including untracked files,
  or explicit paths, each rules section's checks page + rule page bodies,
  and an output contract:
  severity | rule id | file:line | problem | fix, verdict per section). The
  agent executes the briefing; wookie only gathers. Read-only, never a gate.
- Locks: rules sections are locked by default (`locked` overrides per
  section). Enforcement lives in the storage layer (`save_page`/
  `delete_page`), so every mutation path is covered; `save_page_raw` exists
  only for tool-internal mechanical operations (doctor frontmatter repair,
  mv link rewrites). `wookie unlock <section> [--minutes N]` (default 15,
  capped at 24h) opens a temporary window recorded in the gitignored
  `.unlocks.toml` with an RFC3339 expiry; `wookie lock` relocks early. Over
  MCP, unlock_section additionally requires `user_approved: true`. Enforcement is layered: hard failure in the tool, a distinct unlock
  command the harness's permission prompt surfaces to the human, guidance
  that forbids unprompted unlocking, and auto-expiry against dangling
  unlocks. wookie cannot verify consent itself; the layers make silent rule
  drift require deliberately ignoring all of them.

## Pinned pages: always-on vs on-demand

`pin: true` frontmatter (set via `--pin`/`--unpin` on new/write) marks a page
as standing orders: `wookie context` inlines pinned bodies in full under
"Pinned instructions", ahead of the section listing. Use for commit/PR rules,
style laws, hard constraints. Everything unpinned is reference material
fetched via read/search. Keep the pinned set small; it is paid on every
session prime.

## Ingest: sync the wiki with the codebase

wookie has no LLM inside, so ingest splits the work: wookie does the
mechanical part and emits a hardcoded playbook the agent executes.

- Fresh run (`wookie ingest --level quick|standard|deep`): inventories the
  project (`git ls-files`, else a junk-filtered walk), seeds `code/<module>`
  stubs (top-level dirs; standard/deep also submodules with >=3 files, capped)
  each carrying `sources: [<dir>/]`, and prints the level's worklist —
  quick: index + module overviews; standard: + submodules and 3-5 key flows;
  deep: + per-file/type pages, invariants, edge cases.
- `wookie ingest --mark`: records the project's HEAD in `wookie.toml`
  (`last_ingest_commit`) once the agent finishes the worklist. Never
  auto-marked, so an interrupted worklist resurfaces next run.
- Update run (sync point exists): `git diff --name-only <last>..HEAD`, mapped
  against every page's `sources` prefixes -> stale-page worklist, plus
  uncovered changes and stubs for new top-level modules. `--full` re-ingests,
  `--since <commit>` overrides the base.
- Pages carry `sources: [paths]` frontmatter (set via `--sources` on
  new/write) so any page, not just seeded ones, participates in staleness.

## MCP (`wookie serve`)

Newline-delimited JSON-RPC 2.0 over stdio, hand-rolled (initialize, ping,
tools/list, tools/call). Tools mirror the CLI verbs and accept
optional `wiki` and `cwd` for resolution. Same command layer as the CLI.

## Plugins

Both generated from `templates/guidance.md` so guidance never drifts:

- `wookie plugin install claude` -> `~/.claude/skills/wookie/SKILL.md`
- `wookie plugin install codex` -> managed `<!-- wookie:start/end -->` block
  in `~/.codex/AGENTS.md`, idempotent on reinstall

## Sessions and notifications

`wookie session start` creates a project-scoped agent identity named
`session-<UTC date>-<UTC time>-<unique id>`. Session metadata is durable and
records the agent, optional label, timestamps, and active/closed status.

`wookie notify` writes an append-only Markdown notification beneath the source
session. Its TOML frontmatter records the source, one-line summary, kind
(`code-change`, `decision`, `blocker`, `handoff`, `warning`, or `note`),
importance, affected paths, and timestamp. The body contains optional details.

`wookie notifications --session <id>` scans other sessions and returns compact
metadata for unread items. `notification read` returns the body and marks it
read; `notification dismiss` suppresses an irrelevant item without exposing
the body. Inbox state is stored per receiving session and gitignored. A new
session initializes its inbox against existing notifications, preventing an
old-history flood; `--all` remains an explicit history view.

Delivery is cooperative polling rather than push. Installed agent guidance
polls after session start, before overlapping edits, after substantial work,
and before commit or handoff. CLI and MCP expose the same lifecycle.

The guidance teaches: `context` at task start, `read --expand` before
answering, capture durable knowledge, `expand` + fill stubs, `doctor` before
finishing.

## Lifecycle and CI

- `wookie roots [--add/--remove <path>]` edits a wiki's project roots
- `wookie rename-wiki <old> <new>` / `wookie remove-wiki <slug> --force`
- `wookie doctor --strict` exits non-zero when issues remain (CI gate)
- `--description` on new/write sets or refreshes the toc line explicitly

## Non-goals for v0.1

- No rendering/HTML; terminals and agents only
- No remote sync; wikis are local git repos
- No cross-clone resolution by remote URL (possible v0.2)
