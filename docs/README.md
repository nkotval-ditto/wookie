# Wookie

Wookie is a local, Markdown, LLM-first knowledge base and coordination layer
for software projects. It gives agents a compact map to durable knowledge,
keeps rules reviewable, and lets concurrent sessions announce changes without
putting generated files in the repository being documented.

## Start in five commands

```sh
cargo install --locked --path .
cd /path/to/project
wookie init
wookie prime --query "understand the architecture"
wookie read index --expand
```

Prime is the normal task-start surface. It returns bounded standing
instructions, a compact section map, and ranked page suggestions. The complete
catalog stays available through `wookie context`, but is never required just
to begin useful work.

## Where Wookie keeps data

Wikis live below `WOOKIE_HOME`, normally `~/.wookie/`, and resolve from the
current project or linked worktree. Curated pages, configuration, safe
protocols, transaction journals, and append-only session records all stay
outside the source checkout.

The documentation website is similarly static: it has no API and no access to
live wiki content. See [Build and self-host these docs](self-hosting.md).

## Learn by workflow

### Start and retrieve

- [Getting started](getting-started.md) — install Wookie, register a project,
  prime a task, and begin coordination.
- [Retrieving knowledge](retrieval.md) — use ranked search, explicit reads,
  pin levels, hard budgets, state hashes, and continuations.

### Create and review knowledge

- [Page protocols](protocols.md) — create pages from safe, namespaced,
  project-local Markdown scaffolds.
- [Rules and findings](rules-and-findings.md) — review rule changes and track
  findings through remediation and verification.
- [Transactional publishing](publishing.md) — preview and atomically apply
  coordinated multi-page changes.
- [Audits and CI](audit-and-ci.md) — run revision-aware provenance checks and
  consume stable `wookie.report/v1` JSON.

### Coordinate agents

- [Session lifecycle](sessions.md) — start, inspect, heartbeat, and close
  agent sessions.
- [Publishing notifications](notifications.md) — report meaningful changes,
  decisions, blockers, warnings, and handoffs.
- [Inbox triage](inbox-triage.md) — filter compact metadata, read relevant
  notifications, and dismiss irrelevant ones.
- [Session maintenance](session-maintenance.md) — inspect staleness and safely
  prune retained coordination history.
- [Agent and MCP integration](agent-coordination.md) — install agent guidance
  and use structured MCP equivalents.

### Operate Wookie

- [Configuration](configuration.md) — apply global defaults and sparse
  per-wiki overrides within explicit safety ceilings.
- [Storage and concurrency](storage-and-concurrency.md) — understand
  append-only state, path containment, atomic writes, and serialized history.
- [Build and self-host these docs](self-hosting.md) — preview locally, build
  portable static files, or run the hardened container.

For the complete source-level command inventory and invariants, see the
[repository README](https://github.com/nkotval-ditto/wookie/blob/main/README.md)
and
[specification](https://github.com/nkotval-ditto/wookie/blob/main/SPEC.md).
