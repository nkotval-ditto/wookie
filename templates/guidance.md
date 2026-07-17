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
wookie doctor [--fix]          # health check: broken links, orphans, stubs
wookie list / wookie init      # all wikis / register a new one for this project
```

Add `--wiki <slug>` if you are outside the project directory, and `--json` for
machine-readable output.

## Conventions (enforced by the tool; do not fight them)

- Page ids are kebab-case paths: `scheduler`, `internals/retry-policy`.
- Link pages with `[[page-id]]` or `[[page-id|display text]]`. Link liberally;
  a link to a page that doesn't exist yet is fine and becomes a stub via
  `wookie expand`.
- The first paragraph of every page must be a standalone summary, readable
  without the rest of the page (it is what `--expand` shows other readers).
- Never edit frontmatter timestamps or files under `~/.wookie` directly; go
  through wookie commands so history and metadata stay correct.

## Growing the wiki (the expand workflow)

1. While writing a page, link concepts you mention: `[[run-lifecycle]]`.
2. Run `wookie expand` — every broken link becomes a stub page, and you get a
   worklist of all stubs.
3. Fill each stub you have knowledge for: `wookie read <id>` to see what links
   to it, then pipe a body with `wookie write <id>`. Writing clears the stub.
4. Leave stubs you can't fill; they are honest TODOs for the next session.
