# Page protocols

A protocol is a small, project-local Markdown scaffold for creating a
consistent kind of page. Protocols keep page creation extensible without adding
a plugin runtime, dependency graph, hooks, or generated commands.

## Where protocols live

Protocols belong to the resolved wiki under `~/.wookie/`, not in the project
checkout. That keeps repositories clean while allowing every agent working on
the same project to share them.

Nested paths are the namespace:

```text
protocols/
├── decisions/record.md
├── findings/finding.md
└── operations/runbook.md
```

There are no protocol manifests, packages, versions, inheritance rules, or
runtime hooks. One file describes one creation-time scaffold.

## Inspect protocols

```sh
wookie protocol list
wookie protocol show findings/finding
```

Discovery is exhaustive or it fails: Wookie never presents a partial protocol
catalog as complete. Immutable, generous ceilings bound the number of tree
entries, files, protocols, aggregate template bytes, and path bytes.

Create a page from a protocol and then edit it like any other page:

```sh
wookie new findings/authz-bypass --protocol findings/finding \
  --title "Authorization bypass in export"
wookie write findings/authz-bypass < completed-finding.md
```

The optional TOML header accepts only `description`, `section`, and `tags`.
`description` describes the protocol in discovery output; it is not a page
description template. Explicit page title and tags are merged with protocol
defaults by the command layer. Rendering rejects invalid page ids, unsafe
paths, unknown variables, and output that violates normal page validation.
Applying a protocol never grants permission to write a locked rules section.
Visible `description` and `tags` also reject terminal controls and Unicode
direction-formatting marks; the Markdown template body remains unrestricted
apart from its size and fixed-placeholder checks.

## Authoring a protocol

Use the protocol management command rather than editing files under
`~/.wookie` directly:

```sh
wookie protocol write findings/security < finding-protocol.md
```

Remove an obsolete protocol only after confirming it is no longer part of an
operator workflow:

```sh
wookie protocol remove findings/security
```

Removing a protocol does not change pages that were created from it.

A protocol should be short. Its accepted form is:

```markdown
+++
description = "Record a security finding"
section = "findings"
tags = ["finding", "security"]
+++
**{{title}}** records finding `{{id}}` discovered on {{date}}.

## Impact

## Evidence

## Remediation

## Verification
```

Only `{{id}}`, `{{title}}`, and `{{date}}` are expanded, once. Keep project
facts in wiki pages; a protocol is a form, not a second knowledge base.

Before relying on a placeholder or frontmatter field, inspect the accepted
schema with `wookie protocol show` or the command help for the installed Wookie
version. Unsupported fields fail closed rather than being copied into a page.

## Finding protocol

Findings should initially use ordinary pages, tags, source paths, and a shared
protocol rather than a separate database. New wikis include
`findings/finding`, whose body asks for:

```markdown
**Finding F-014** records the issue and the evidence needed to resolve it.

## Severity

Set one `severity/*` tag.

## Affected files

- `src/export.rs`

## Owner

Unassigned, or add an `owner/*` tag.

## Remediation

## Verification evidence
```

Use a stable finding id in the page id or tags, set `--sources` for affected
files, and link related architecture or decision pages. The protocol adds
`finding` and `status/open`; add one `severity/*` tag and optionally an
`owner/*` tag. Replace `status/open` with `status/verified` only after recording
evidence. The dashboard can then summarize unresolved findings without a
separate entity model. An older wiki may not contain the built-in protocol;
create it with
`wookie protocol write findings/finding < finding-protocol.md` if needed.

`wookie doctor` loads and validates the complete stored protocol catalog. An
unreadable or malformed template is an error, as is an explicit protocol
`section` that is absent from the wiki's effective section configuration.
Finding-page audits additionally require exactly one non-empty `status/*` and
one of `severity/critical`, `severity/high`, `severity/medium`, `severity/low`,
or `severity/info`. They warn when affected-file `sources` or the `## Owner`,
`## Remediation`, and `## Verification evidence` sections are missing.

## Design boundary

Protocols run only when a page is created. Later protocol edits do not migrate
existing pages. If a project eventually needs migrations, validation hooks, or
third-party executables, that should be designed as a distinct plugin system
with an explicit security boundary—not added implicitly to protocols.
