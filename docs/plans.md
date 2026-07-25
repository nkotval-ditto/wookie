# Live plans

`wookie plan` turns a non-trivial implementation plan into a small, durable
session log and a read-only live Kanban board. It is deliberately not a
general project manager: the plan has four fixed states, agents record
meaningful events through Wookie, and the browser only projects those records.

## End-to-end workflow

Start a normal Wookie session, then ask Wookie for the plan-writing contract:

```sh
export WOOKIE_SESSION="$(wookie session start \
  --agent codex \
  --label 'retry-policy implementation' \
  --id-only)"

wookie plan guide --query "redesign retry exhaustion handling"
```

Use `plan guide` before writing a non-trivial implementation plan. Its
task-specific prompt prepares the artifact contract. Feed that contract into
the host's native planning mode when one is available—Codex Plan mode, Claude's
planning workflow, or the equivalent—so the host remains responsible for
exploration, questions, and plan review. Wookie then validates and tracks the
approved artifact instead of introducing a second planning engine. In Codex,
the operator can enter Plan mode with `/plan` or Shift+Tab.

Save the resulting definition as TOML:

```toml
schema = "wookie.plan/v1"
title = "Redesign retry exhaustion handling"

[[segments]]
id = "confirm-boundaries"
title = "Confirm retry ownership boundaries"
status = "todo"
guide = "architecture/retry-policy"
justification = "Implementation depends on stable ownership and failure boundaries."
decisions = [
  "Keep retry policy separate from mutable execution state.",
  "Preserve the existing caller-facing error contract."
]
verification = "Review the boundary with callers and run the architecture checks."
depends_on = []

[[segments]]
id = "implement-policy"
title = "Implement the policy change"
status = "todo"
guide = "code/retry"
justification = "The implementation can proceed once its ownership is explicit."
decisions = ["Keep exhaustion accounting in the retry module."]
verification = "Run the focused retry tests and the full test suite."
depends_on = ["confirm-boundaries"]
```

Validate before attaching it:

```sh
wookie plan check plan.toml
wookie plan attach plan.toml
```

Omit the file to read TOML from standard input. `check` is read-only. `attach`
stores the validated definition in the current session. Reattaching the
identical canonical plan is an idempotent retry; a different definition is
rejected because a plan is immutable once work starts. Correct an unattached
file and check it again; do not rewrite an attached plan behind Wookie's back.

Open the board:

```sh
wookie plan
```

Wookie starts a short-lived viewer bound to loopback on an
operating-system-selected port, prints the URL, and opens it in the default
browser. Version 1 cannot bind a LAN or public interface. Use a fixed local
port or suppress browser launch when needed:

```sh
wookie plan --port 4317
wookie plan --no-open
```

The server stays in the foreground until interrupted. It is a read-only view;
keep it open while agents update the same session through the CLI or MCP.

## Record progress

The four states are intentionally fixed:

- `todo` — ready or waiting on dependencies;
- `doing` — active work;
- `blocked` — progress requires a decision or external change;
- `done` — the segment's verification has passed.

Move a segment only when its real state changes:

```sh
wookie plan update confirm-boundaries doing \
  --note "Tracing ownership through scheduler callers"

wookie plan log \
  --segment confirm-boundaries \
  --kind decision \
  --summary "Retry policy remains separate from execution state"

wookie plan update confirm-boundaries done \
  --note "Architecture review complete"
```

Log kinds are `progress`, `decision`, `blocker`, and `note`. A log can describe
the whole plan when `--segment` is omitted. Use logs for information another
agent or the future operator would need to understand the work; avoid recording
every command or minor edit.

Inspect the same folded state without starting a server:

```sh
wookie plan show
wookie --json plan show
```

The browser polls the snapshot and animates a card between `todo`, `doing`,
`blocked`, and `done` when a new transition appears. Selecting a card exposes
its justification, decisions, verification, dependencies, and linked guide.
The activity rail shows the Wookie-recorded timeline.

> [!note]
> The board is an operational record, not a model-observation system. It shows
> plan transitions, plan logs, session activity, and notifications recorded
> through Wookie. It cannot show private model reasoning or arbitrary shell and
> editor actions that no agent logged.

## Link a plan to a Linear epic

Wookie can map one plan to a Linear Project (the epic) and one issue per
segment without storing Linear credentials or calling Linear itself. The
active agent bridges Wookie MCP and Linear MCP:

```sh
wookie --json plan linear export
```

The export contains a Project name/summary/description and ordered issue
manifests with stable segment ids, semantic statuses, descriptions, and
dependency keys. The agent creates the Project and issues through Linear MCP,
adds blocking relationships, then gives Wookie the complete result:

```toml
schema = "wookie.plan-linear-link/v1"

[project]
id = "project-id-or-slug"
url = "https://linear.app/acme/project/retry-redesign"

[[issues]]
segment_id = "confirm-boundaries"
id = "ENG-101"
url = "https://linear.app/acme/issue/ENG-101/confirm-boundaries"
status = "doing"

[[issues]]
segment_id = "implement-policy"
id = "ENG-102"
url = "https://linear.app/acme/issue/ENG-102/implement-policy"
status = "todo"
```

```sh
wookie plan linear link linear-link.toml
```

Linking succeeds only when every segment has one unique issue and the supplied
semantic status already agrees with Wookie. The Project and issue mapping is
immutable. Identical retries are idempotent; remapping fails closed. The board
then shows the epic and per-card issue links.

Progress synchronization is explicit and two-phase. First, the agent reads
each linked issue through Linear MCP, maps the team's concrete workflow state
to `todo`, `doing`, `blocked`, or `done`, and previews reconciliation:

```toml
schema = "wookie.plan-linear-observation/v1"

[[issues]]
segment_id = "confirm-boundaries"
status = "done"

[[issues]]
segment_id = "implement-policy"
status = "todo"
```

```sh
wookie --json plan linear reconcile linear-observation.toml
```

The result proposes Linear updates when only Wookie changed, Wookie updates
when only Linear changed, and conflicts when both changed differently from the
last anchor. The agent executes proposed provider writes through Linear MCP
and Wookie writes through `plan update`. After both sides agree, it records the
new immutable anchor:

```sh
wookie --json plan linear reconcile linear-observation.toml --confirm
```

Confirmation never performs a hidden provider mutation. This preserves normal
Wookie dependency checks, keeps Linear access in the user's MCP connection,
and makes partial failures recoverable by rerunning preview after observing
both sides again.

## Finish and retain the record

Archive after every segment is done:

```sh
wookie plan archive --summary "Retry redesign implemented and verified"
```

Archive folds the immutable definition and append-only events, appends the
authoritative receipt/close event, and writes a deterministic immutable
`archive.md` view of the final plan, timeline, and bounded outgoing-notification
summaries with an explicit omission count. It refuses an incomplete plan by
default. For an intentionally stopped or superseded effort, make that explicit:

```sh
wookie plan archive \
  --allow-incomplete \
  --summary "Stopped after upstream API was withdrawn"
```

Archived plans remain beneath the session directory and follow the normal
session-retention policy. Closing or archiving does not delete them, but an
applied `wookie session prune` removes `plan.toml`, `archive.md`, and activity
with the rest of that session. Durable Git history exists only when wiki
auto-commit and `history.commit_sessions` make those records commits. Preview
any cleanup before using `wookie session prune --apply`.

Archive retries verify the original receipt and exact derived Markdown. If a
process stopped after the authoritative close event but before `archive.md`
was published, rerunning archive safely finishes that projection; a conflicting
file fails closed.

## Plan contract

A definition uses the exact schema name `wookie.plan/v1` and contains a
non-empty title plus one or more segments. Each segment has:

| Field | Meaning |
|---|---|
| `id` | Unique lowercase, path-safe segment identifier |
| `title` | Concise outcome shown on the card |
| `status` | `todo`, `doing`, `blocked`, or `done` |
| `guide` | Existing, non-stub Wookie page that guides the work |
| `justification` | Why this segment is necessary |
| `decisions` | One or more key architectural decisions |
| `verification` | Observable evidence required before `done` |
| `depends_on` | Optional list of segment ids that must precede it; defaults to `[]` |

Unknown fields, duplicate ids, missing guides, stub guides, self-dependencies,
unknown dependencies, dependency cycles, unsafe text, and oversized definitions
fail validation. Wookie applies the same checks in `check` and `attach`; attach
revalidates while holding the wiki mutation lock.

Guide validation reads each distinct linked page once, accepts at most 512 KiB
per serialized guide and 4 MiB across the plan, and rejects larger guide sets.
Those are resource-safety ceilings, not a reason to split a coherent plan.

A segment cannot start as `doing` or `done` while one of its dependencies is
incomplete, and later updates enforce the same rule when moving into either
state. A missing or newly stubbed guide after attachment does not corrupt the
historical plan: snapshots remain readable so the board can surface that guide
error on the affected card.

The definition is immutable after attachment. Updates, logs, and the
authoritative archive receipt are separate append-only activity records. A
snapshot deterministically folds those records over the definition, which
makes concurrent readers safe and browser ETags stable without creating a
second mutable board database. `archive.md` is a derived immutable human view,
not another source of mutable state.

## Browser security

The plan server:

- listens only on `127.0.0.1`, using an ephemeral port by default;
- has no LAN/public bind option and exists only for the foreground command;
- accepts only read requests and exposes no browser mutation endpoint;
- validates local host/origin information;
- serves embedded assets without a CDN or external dependency;
- sends a restrictive content security policy and defensive browser headers;
- returns guide content as data for safe text rendering, not trusted HTML.

The loopback server is an observation surface, not an authorization boundary.
It has no login or capability token: any local process that learns the
ephemeral port and sends the expected `Host` value can read the plan and its
linked guides while the foreground server is running. Do not put secrets,
private reasoning, credentials, or sensitive logs in plan text.

The viewer is also not a hardened multi-user network service. A hostile local
process can consume its foreground connection/header resources, so stop it
when you are finished and do not run it on a shared host with untrusted local
users.

## JSON and MCP

Add global `--json` to `guide`, `check`, `attach`, `show`, `update`, `log`,
`linear`, or `archive` for stable structured CLI output:

```sh
wookie --json plan check plan.toml
wookie --json plan update implement-policy blocked \
  --note "Awaiting caller contract decision"
```

Machine consumers can identify the definition as `wookie.plan/v1`, snapshots
as `wookie.plan-snapshot/v1`, typed activity payloads as
`wookie.plan-event/v1`, and archive receipts as `wookie.plan-archive/v1`.

The stdio server exposes the same non-UI workflow:

| MCP tool | CLI equivalent |
|---|---|
| `plan_guide` | `wookie plan guide --query ...` |
| `plan_check` | `wookie plan check` |
| `plan_attach` | `wookie plan attach` |
| `plan_snapshot` | `wookie plan show` |
| `plan_update` | `wookie plan update` |
| `plan_log` | `wookie plan log` |
| `plan_linear_export` | `wookie plan linear export` |
| `plan_linear_link` | `wookie plan linear link` |
| `plan_linear_reconcile` | `wookie plan linear reconcile` |
| `plan_archive` | `wookie plan archive` |

MCP calls take an explicit `session` because they do not inherit a client's
`WOOKIE_SESSION` environment variable. The browser launcher remains a CLI-only
operator action; MCP agents mutate the same validated append-only records and
the open board observes their changes on its next poll.

## Why the design stays small

Plans reuse Wookie pages for durable implementation guidance and sessions for
operational history. There is no configurable workflow engine, drag-and-drop
mutation API, separate database, background daemon, embedded Linear client, or
arbitrary plugin code.
Fixed states make every plan immediately legible; immutable definitions and
append-only events make the record explainable; guide links keep plan cards
compact while the full knowledge remains one read away.
