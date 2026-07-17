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
  config.toml              # global defaults
  my-project/
    wookie.toml            # per-wiki config: roots, sections, sync point
    .unlocks.toml          # transient lock state (gitignored)
    pages/
      architecture/overview.md
      code/src/scheduler.md
      workflow/checks.md
```

Run `wookie init` from a project directory once. After that every command
resolves the right wiki from your cwd; git worktrees resolve to the main
checkout's wiki, and `--wiki <slug>` works from anywhere. Each wiki is its
own git repo and every mutation auto-commits.

Pages are markdown with tool-owned frontmatter, a standalone first-paragraph
summary, and `[[wikilinks]]` between pages. Links inside code spans are
ignored. Broken links are not errors: `wookie expand` turns them into stub
pages, which is how the wiki grows. Frontmatter lines wookie doesn't own
(e.g. Obsidian properties) round-trip untouched.

## Methodology

wookie is built around three ideas:

**1. The tool hardcodes conventions; agents follow them.** Page format,
link style, filing rules, growth workflow: all decided once, enforced by the
binary, taught by the installed plugins. An agent never has to infer how
this project's wiki works, and two agents never invent divergent structures.
Every error message says what to run instead, because agents read errors.

**2. wookie does the mechanical work; the LLM does the judging.** wookie has
no LLM inside. Commands like `ingest` and `critique` inventory, diff, assemble
and structure, then emit a playbook (a worklist or briefing) that the invoking
agent executes. That split keeps behavior deterministic where it can be and
intelligent where it must be.

**3. Knowledge has tiers, and the tool encodes them.** Descriptive knowledge
(`info` sections) is fetched on demand. Normative knowledge (`rules`
sections) is checkable and change-protected. Standing orders (pinned pages)
are injected at every session prime. Where something lives determines how it
reaches an agent and how hard it is to change.

## Sections

Every page is filed under a section: a top-level namespace declared in the
wiki's `wookie.toml`. Defaults (used verbatim when no `[sections]` block
exists):

| Section         | Kind  | Holds                                              |
|-----------------|-------|----------------------------------------------------|
| `architecture/` | info  | System structure, boundaries (requires `overview`) |
| `code/`         | info  | Module-by-module reference; `ingest` seeds these   |
| `decisions/`    | info  | Why things are the way they are, one per decision  |
| `guides/`       | info  | How to build, test, release, debug                 |
| `style/`        | rules | Code style, naming, idioms, review conventions     |
| `workflow/`     | rules | How to commit, branch, PR, review, release         |

Sections are configurable per wiki:

```toml
[sections.style]
description = "Code style, naming, idioms, review conventions"
kind = "rules"          # "info" (default) | "rules"
locked = true           # optional; defaults to true for rules, false for info
required = ["checks"]   # pages doctor insists on

[sections.runbooks]     # add your own
description = "Incident response, one page per scenario"
```

What sections do:

- `toc` and `context` group pages by section with descriptions, so priming
  an agent teaches the wiki's shape; empty sections show as filing options
- `new` notes when a page id doesn't fit a section; doctor flags unfiled
  pages and missing required pages (warnings, not walls, for info sections)
- `kind = "rules"` makes a section normative: it needs a `<section>/checks`
  page, it participates in `wookie critique`, and it is locked by default

## Locks

Rules sections are locked. The problem locks solve: an agent documenting new
code has no business rewriting the team's rules as a side effect, and
without a hard stop that drift happens silently.

- Any `new`/`write`/`rm`/`mv` into a locked section fails, with an error that
  tells the agent to ask the user first. `expand` skips stubs that would land
  in a locked section
- Enforcement lives in the storage layer (`save_page`/`delete_page`), so
  every mutation path is covered, not just polite commands
- `wookie unlock <section> [--minutes N]` opens a temporary window (default
  15 min, capped at 24h), recorded in the gitignored `.unlocks.toml` and
  auto-expiring. `wookie lock <section>` relocks early
- Page ids are lowercase-only so case-insensitive filesystems (macOS) can't
  alias `STYLE/checks` past the lock

wookie cannot verify a human actually said yes, so enforcement is layered:
hard failure in the tool, a distinct `unlock` command that permission-prompting
harnesses (like Claude Code) surface to the user as its own approval, plugin
guidance that forbids unprompted unlocking, auto-expiry against dangling
unlocks, and over MCP an additional required `user_approved: true` argument.
Silently changing rules requires deliberately defeating every layer.

```sh
$ echo "New rule." | wookie write style/naming
error: section 'style' is locked (it holds this project's rules). Do NOT
unlock it on your own: ask the user for explicit permission first, then run
`wookie unlock style` (auto-relocks in 15 min) and retry.
```

## Critique

`wookie critique` turns the rules sections into a project-specific reviewer:

1. Each rules section keeps a `<section>/checks` page: what artifacts the
   rules apply to, the exact procedure to verify them, what violations look
   like, legitimate exceptions
2. `wookie critique` assembles a briefing: the changed files (uncommitted by
   default, `--staged`, `--since main` for branch review, or `--paths ...`;
   untracked files included), every rules section's checks page and rule
   bodies, and an output contract
3. The invoking agent executes the briefing and reports each violation as
   severity | rule page id | file:line | problem | suggested fix, with a
   pass/fail verdict per section

Critique is read-only and never a gate; pair it with `wookie doctor --strict`
in CI if you want an exit code.

## Ingest

`wookie ingest` keeps the wiki synced with the code. The first run
inventories the project, seeds `code/<module>` stubs and prints a
documentation worklist at the chosen intensity (`quick`: index + module
overviews; `standard`: + submodules and key flows; `deep`: + per-file pages
and invariants). The LLM executes the worklist, then runs
`wookie ingest --mark` to record the sync commit. Later runs diff the code
since that commit and map changed files to stale pages via each page's
`sources` frontmatter, matching the most specific page per file.

## Pinned pages

Pages with `pin: true` (set via `--pin`) are always-on instructions:
`wookie context` inlines their full bodies ahead of the reference listing,
so commit/PR rules and hard constraints reach the agent at every session
prime while everything else stays on demand. Keep the pinned set small; it
is paid on every prime.

## Commands

```
wookie init [slug]             register a wiki for this project
wookie list                    all wikis
wookie context                 digest for priming an agent (start here)
wookie toc                     every page + description, grouped by section
wookie read <id> [--expand[=N]]  page, optionally with linked summaries inlined
wookie new <id> [--title --tags --description --sources --pin]
wookie write <id> [--append --description --sources --pin|--unpin]
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

`wookie serve` speaks MCP over stdio and mirrors the CLI as 16 tools
(`wiki_context`, `page_read`, `page_write`, `ingest`, `critique`,
`unlock_section`, ...). Register it in Claude Code with:

```sh
claude mcp add wookie -- wookie serve
```

## Development

```sh
cargo test    # unit tests + end-to-end CLI tests against a temp WOOKIE_HOME
```

See `SPEC.md` for the design.
