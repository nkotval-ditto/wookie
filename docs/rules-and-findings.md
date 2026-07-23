# Rules and findings

Rules are normative wiki content with an explicit approval boundary. Findings
are ordinary, structured pages that describe issues and their remediation.
Keeping both on page and publish primitives avoids parallel storage systems.

## Rules lifecycle

Do not unlock a rules section merely to make a routine documentation change.
Prepare and validate a proposal first:

```sh
wookie rules propose rules-change.toml
wookie rules review <proposal-id>
```

The proposal records the base wiki revision and exact page changes. Its review
re-runs publish preflight, including affected checks and expected diagnostics,
without mutating the wiki. Missing `<section>/checks` pages or weakened
verification steps should be prominent. Proposal and review output uses the
configured `publish.output_tokens` budget by default; use `--tokens` for a
one-off bounded override or `--full-diff` for an explicitly exhaustive review.

Review creates a strict sidecar receipt under the Git-ignored
`proposals/rules/` store. The receipt binds SHA-256 identities for the raw
proposal, exact raw page catalog, configuration snapshot, effective policy,
complete deterministic plan, and observed/base revisions. A compact review can
omit display details without weakening that binding, but approval should use
`--full-diff` whenever the bounded response reports omissions. Proposal and
receipt files must be bounded regular non-symlink files.

After the user explicitly approves the change in the current conversation,
apply it with the approval guard:

```sh
wookie rules apply <proposal-id> --user-approved
```

Apply rechecks the proposal against the current revision and authorizes only
the exact locked pages in that reviewed transaction. It requires the receipt,
prepares the plan exactly once, compares every bound field, and passes that same
checked preflight into the transaction. Any proposal, catalog, configuration,
policy, revision, or plan change makes the receipt stale and requires another
review. It does not open a section-wide unlock window. `--user-approved`
records an assertion; it is not a substitute for actually obtaining
permission.

If an affected rules section was already temporarily unlocked, apply relocks
it while holding the same publication lock, before page mutation. Only
sections whose configured kind is `rules` receive this treatment; a custom
locked information section remains a normal locked section and is never
silently reclassified as rules.

If the installed release exposes only the lower-level commands, the safe
equivalent is to review `wookie publish rules-change.toml --check`, obtain
explicit approval, then apply that exact manifest with
`wookie publish rules-change.toml --apply --user-approved`. Publish uses the
same exact-page authorization while the section-wide lock remains closed.

## What every rules section needs

Each rules section needs a `<section>/checks` page that states:

- Scope: artifacts and paths covered by the rules;
- Procedure: commands and inspections a reviewer performs;
- Violations: concrete examples of noncompliance;
- Exceptions: how an exception is approved and recorded.

`critique` turns those rules and checks into a revision-specific review
briefing. The invoking agent still performs the procedure and reports evidence;
Wookie does not pretend prose rules are automatically executable. The default
briefing omits whole bodies and supplies page summaries plus exact read
commands; use `--tokens` to raise the compact ceiling or `--all` for explicit
exhaustive output.

## Findings workflow

Create findings from a project protocol:

```sh
wookie new findings/f-014-export-authz \
  --protocol findings/finding \
  --title "F-014: export authorization bypass" \
  --tags severity/high \
  --sources src/export.rs
```

Record a stable id, severity, affected files, owner, remediation, and
verification evidence. Every `finding` page must have exactly one non-empty
`status/*` tag and exactly one recognized severity tag:
`severity/critical`, `severity/high`, `severity/medium`, `severity/low`, or
`severity/info`. Its page id is the finding id, and its `sources` are the
affected files. The body must retain `## Owner`, `## Remediation`, and
`## Verification evidence`. Link the finding to the affected architecture and
decision pages.

`wookie doctor` reports missing or ambiguous status/severity as errors. Empty
affected-file sources and missing required body sections are warnings, so the
finding remains an ordinary page while still having an enforceable review
contract.

Update status with a reviewed publish metadata change, preserving the
`finding`, severity, and owner tags while replacing the single `status/*` tag.
A finding is not complete merely because remediation text exists; use
`status/verified` only when the page records reproducible evidence. `wookie
status` counts unresolved findings without needing a separate findings
database.

## Security and auditability

Rule approval and finding evidence cross trust boundaries. Avoid secrets,
credentials, customer data, or exploit payloads in page bodies and command
arguments. Use durable external evidence references when raw material is
sensitive. Publication history should make the author, approved diff, and final
revision recoverable without weakening rule locks.
