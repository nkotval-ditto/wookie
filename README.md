# wookie

LLM-first wiki manager. One binary that owns the conventions (layout, page
format, linking, growth workflow) so agents never have to guess how a
project's wiki works.

## Install

```sh
cargo install --path .
wookie plugin install claude   # Claude Code skill
wookie plugin install codex    # Codex AGENTS.md block
```

## How it works

Wikis live under `~/.wookie/`, one per project, outside any checkout:

```
~/.wookie/
  config.toml              # registry: project roots -> wikis, defaults
  my-project/
    wookie.toml            # per-wiki config (name, description, project_roots)
    pages/
      scheduler.md
      internals/retry-policy.md
```

Run `wookie init` from a project directory once. After that every command
resolves the right wiki from your cwd; git worktrees resolve to the main
checkout's wiki, and `--wiki <slug>` works from anywhere. Each wiki is its own
git repo and every mutation auto-commits.

Pages are markdown with tool-owned frontmatter, a standalone first-paragraph
summary, and `[[wikilinks]]` between pages. Links inside code spans are
ignored. Broken links are not errors: `wookie expand` turns them into stub
pages, which is how the wiki grows.

Pages are filed under sections: top-level namespaces declared in each
`wookie.toml` (defaults: `architecture/`, `code/`, `decisions/`, `guides/`,
`style/`, `workflow/`). Sections are `info` or `rules`: rules sections
(style, workflow) are checkable via `wookie critique`, need a
`<section>/checks` page saying how to verify them, and are locked, so agents
must get explicit user permission (then `wookie unlock`) before changing
rules; unlocks auto-expire. `toc` and `context` group by section, unfiled pages
are doctor findings, and sections can require pages (`architecture/overview`
by default). Pages carrying `pin: true` (via `--pin`) are always-on
instructions: `context` inlines their full bodies, so commit/PR rules and
hard constraints reach the agent every session while reference pages stay
on-demand.

`wookie ingest` keeps the wiki synced with the code. The first run inventories
the project, seeds `code/<module>` stubs and prints a documentation worklist
at the chosen intensity (`quick`: index + module overviews; `standard`: +
submodules and key flows; `deep`: + per-file pages and invariants). wookie
does the mechanical part; the LLM executes the worklist, then runs
`wookie ingest --mark` to record the sync commit. Later runs diff the code
since that commit and map changed files to stale pages via each page's
`sources` frontmatter.

## Commands

```
wookie init [slug]             register a wiki for this project
wookie list                    all wikis
wookie context                 digest for priming an agent (start here)
wookie toc                     every page + description
wookie read <id> [--expand[=N]]  page, optionally with linked summaries inlined
wookie new <id> [--title --tags]  create page, body from stdin
wookie write <id> [--append]   replace/append body from stdin, clears stub
wookie rm <id> / mv <old> <new>  delete / rename (inbound links rewritten)
wookie expand [<id>]           stub out broken links, print fill-in worklist
wookie ingest [--level L]      sync wiki with the codebase (quick|standard|deep)
wookie ingest --mark           record the current commit as the sync point
wookie search <query> [--tag]  regex search over ids/titles/tags/bodies
wookie links <id>              outlinks + backlinks
wookie critique [--since ref]  briefing to check changes against rules sections
wookie unlock/lock <section>   temporary write access to a locked section
wookie doctor [--fix|--strict] health check (--strict exits non-zero for CI)
wookie roots [--add|--remove]  show or edit the wiki's project roots
wookie rename-wiki / remove-wiki  wiki lifecycle
wookie obsidian [--print]      open the wiki as an Obsidian vault
wookie plugin install claude|codex
wookie serve                   MCP server over stdio
```

`--json` on any read-style command gives machine-readable output.
`WOOKIE_HOME` overrides `~/.wookie` (used by the test suite).

## MCP

`wookie serve` speaks MCP over stdio and mirrors the CLI as 12 tools
(`wiki_context`, `page_read`, `page_write`, `wiki_expand`, ...). Register it
in Claude Code with:

```sh
claude mcp add wookie -- wookie serve
```

## Development

```sh
cargo test    # unit tests + end-to-end CLI tests against a temp WOOKIE_HOME
```

See `SPEC.md` for the design.
