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
```

- Page id = relative path under `pages/` without `.md`
- Each wiki is a git repo (`wookie init` runs `git init`; mutations
  auto-commit, best-effort and silent on failure)
- Wikis live outside project checkouts, so one wiki per project regardless of
  how many checkouts or worktrees exist

## Wiki resolution

1. `--wiki <slug>` flag (CLI) or `wiki` param (MCP)
2. cwd prefix match against registered `project_roots` (longest wins)
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
  unfilled pages, writing real content clears it
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
tools/list, tools/call). 12 tools mirroring the CLI verbs, all accepting
optional `wiki` and `cwd` for resolution. Same command layer as the CLI.

## Plugins

Both generated from `templates/guidance.md` so guidance never drifts:

- `wookie plugin install claude` -> `~/.claude/skills/wookie/SKILL.md`
- `wookie plugin install codex` -> managed `<!-- wookie:start/end -->` block
  in `~/.codex/AGENTS.md`, idempotent on reinstall

The guidance teaches: `context` at task start, `read --expand` before
answering, capture durable knowledge, `expand` + fill stubs, `doctor` before
finishing.

## Non-goals for v0.1

- No rendering/HTML; terminals and agents only
- No remote sync; wikis are local git repos
- No cross-clone resolution by remote URL (possible v0.2)
