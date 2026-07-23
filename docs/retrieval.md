# Retrieving knowledge

Wookie separates fast task startup from exhaustive inspection. Start with a
bounded map, fetch the few relevant pages, and only request the full catalog
when you genuinely need it.

## Start a task with `prime`

Pass the task you are about to perform:

```sh
wookie prime --query "diagnose why startup context is slow" --tokens 1500
```

`prime` returns standing instructions, discoverable pinned references, a
compact section/page map, ranked page metadata (without body excerpts),
selection reasons, state/context hashes, omissions, and retrieval telemetry.
The token budget is a hard output target, not permission to silently drop
required instructions. If optional content does not fit, Wookie reports what
was omitted and how to continue.

Freshness is `current` only after a successful zero-change Git comparison.
Changed paths make it `stale`, including paths not yet covered by a page;
missing revisions, failed comparisons, and ambiguous multi-root invocations
are `unknown`. Linked worktrees are compared in the active invocation
worktree, not silently against the registered main checkout.

Use the returned page ids with `read`:

```sh
wookie read architecture/retrieval --expand
```

`--expand` adds linked summaries to a maximum depth of 5 and returns at most
100 linked pages. Breadth omissions are counted with direct-read guidance;
summary text is compacted. It does not replace deliberate reads when a linked
page's full detail matters.

## Discover with bounded search

Search is ranked and bounded by default:

```sh
wookie search "context cache pinned" --limit 10 --tokens 2000
```

Each result contains a page id, title or description, a short excerpt, and the
reason it ranked. Narrow further with tags when the wiki uses them:

```sh
wookie search "rollback" --tag publishing --limit 5
```

Use `--excerpt-lines` to tune excerpt size. Deterministic relevance search is
the default; pass `--regex` for bounded regular-expression results. `--all`
restores the legacy exhaustive regex dump and ignores the normal result and
token limits, while retaining at most five matching body lines per page. Reserve
it for audits or migrations.

Bounded retrieval rejects queries above 4 KiB or 64 distinct terms, result
windows above 1,000 pages, and excerpts above 20 lines per result. These are
hard allocation ceilings for CLI and MCP requests. Prime, instruction, and
search token budgets also have an immutable maximum of 1,000,000; ordinary
defaults remain much smaller. A per-call prime token budget below the configured instruction
budget automatically lowers the implicit instruction allowance, while an
explicit `--instruction-tokens` value above `--tokens` remains an error.

## Choose the right command

| Need | Command |
| --- | --- |
| Start one task | `wookie prime --query "..."` |
| Find likely pages | `wookie search "..."` |
| Read authoritative detail | `wookie read <id> [--expand]` |
| Inspect every page description | `wookie context` |

`context` remains the explicit full-catalog operation. It is useful for wiki
maintenance, but should not be injected into every agent turn.

## Grow links with a bounded worklist

`expand` is a complete mutation with a compact response:

```sh
wookie expand --limit 10 --tokens 2000
```

It still creates every eligible missing page. The limits apply only to the
returned `created`, current `stubs`, and `skipped_locked` worklist categories.
Human and JSON output report complete totals, per-category omission counts,
and the command to continue. Omitted `created` and existing `stubs` entries can
be fetched directly with `wookie read <id> --expand`; `skipped_locked` targets
were not created and require the normal approved unlock workflow. `wookie
expand --all` lists the exhaustive current worklist. By default, expand reuses
`retrieval.search_limit` and `retrieval.search_tokens`, avoiding another
configuration surface.

## Pins and standing instructions

Pins have three intended roles:

- `instruction`: always include concise normative text. If a page has an
  `## Agent instructions` section, prime extracts that section; otherwise it
  uses the first-paragraph summary.
- `summary`: always surface the page's standalone first-paragraph summary.
- `discoverable`: always highlight compact title/description metadata and a
  `wookie read <id>` command, but never inline body/summary content or consume
  the standing-instruction budget.

Legacy `pin: true` pages behave as instruction pins. Keep instruction content
short and use `wookie read` for rationale and examples. `doctor` should flag a
pinned instruction set that exceeds its configured budget rather than silently
discarding rules. Instruction and summary pins must have real, non-stub,
non-placeholder standing text; `new`/`write` prevent invalid states, while
prime fails closed and doctor reports legacy invalid data. Discoverable stubs
are allowed and shown explicitly.

Set the role explicitly when creating or updating a page:

```sh
wookie new workflow/agent-rules --pin-level instruction < agent-rules.md
wookie write architecture/overview --pin-level summary < overview.md
wookie new guides/operator-reference --pin-level discoverable < reference.md
```

## Context hashes and continuation

A priming response includes two opaque identities:

- `state_hash` is query-independent and binds the canonical page catalog, pin
  state, effective configuration/sections, and freshness. Reuse it with
  `--since` to avoid retransmitting unchanged section structure.
- `context_hash` binds that state plus the exact query and effective window
  options. It authorizes only pagination through `--context-hash`.

The state hash may be reused for an entirely new task:

```sh
wookie prime --query "diagnose a new scheduler task" --since <state-hash>
```

If the complete retrieval state is unchanged, Wookie marks the response
`unchanged_since` and suppresses only the unchanged section catalog. It still
returns standing instructions, budgeted discoverable metadata, and suggestions
freshly ranked for the new query. A continuation must reuse the state hash,
query/options context hash, and numeric cursor returned together:

```sh
wookie prime --query "review the publish transaction" \
  --since <state-hash> --context-hash <context-hash> --cursor 8
wookie search "publish transaction" --context-hash <hash> --cursor 8
```

Changing the query, options, project freshness, or catalog invalidates the
cursor; restart at zero instead of risking skipped or duplicated results.
Treat hashes and cursors as opaque retrieval state, not Git revisions or
long-term page identifiers.

## Incremental parse cache

`prime` and `search` maintain one disposable `.cache/retrieval-v1.json` inside
the wiki. The directory is Git-ignored. Each entry binds a strong file
signature, exact raw SHA-256, and an integrity-checked parsed page projection.
Warm queries reuse unchanged parses; edits refresh only changed pages, while
creates and deletes update the catalog index atomically. Telemetry reports
`hit`, `updated`, or `bypassed` plus reused and refreshed page counts.

The cache is intentionally not a database, daemon, embedding store, or query
result cache. Ranking remains deterministic and scans cached page text, so CPU
cost is still proportional to wiki text. A warm prime/search performs strict
O(page-count) metadata enumeration, but strong-signature cache hits do not
reopen or reparse unchanged page bodies. Prime rereads only current
instruction/summary bodies canonically, checks them against catalog hashes,
then verifies that the complete file generation stayed unchanged. Discoverable
pins use integrity-checked cached metadata and are never body-reread merely for
highlighting. Query-proportional inverted or semantic indexes are intentionally
outside this minimal design.

Malformed, oversized, wrong-version, or symlinked cache storage is ignored and
rebuilt from canonical pages. If an active publish, recovery journal, missing
`.cache/` ignore rule on an older wiki, or write failure prevents persistence,
retrieval still returns freshly read in-memory results and reports `bypassed`.
Pinned instruction and summary bodies are reread from canonical storage before
output; telemetry distinguishes projection reuse, refreshed parses, and pinned
body rereads.
The local account that owns the wiki/cache is the trust boundary: self-hashes
detect corruption and partial writes, not a same-user attacker who can rewrite
both source pages and derived cache state.
