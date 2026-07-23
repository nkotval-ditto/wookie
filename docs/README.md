# Wookie usage guides

These guides cover Wookie's knowledge retrieval, change control,
agent-to-agent coordination, configuration, and storage behavior.
Coordination records live under each wiki's `sessions/` directory; wikis
themselves live under `WOOKIE_HOME` (normally `~/.wookie`), not in the project
checkout.

- [Retrieving knowledge](retrieval.md) — start with bounded, task-aware
  priming; use ranked search, explicit reads, pins, and continuation cursors.
- [Page protocols](protocols.md) — create pages from safe, namespaced,
  project-local Markdown scaffolds, including the built-in finding protocol.
- [Transactional publishing](publishing.md) — preview multi-page manifests,
  apply them under one lock and journal, and recover interrupted publications.
- [Audits and CI](audit-and-ci.md) — use revision-aware provenance checks, the
  operator dashboard, and stable `wookie.report/v1` JSON.
- [Rules and findings](rules-and-findings.md) — propose, review, approve, and
  apply rules changes; track actionable findings with pages and tags.
- [Session lifecycle](sessions.md) — start, identify, inspect, heartbeat, and
  close agent sessions.
- [Publishing notifications](notifications.md) — report changes, decisions,
  blockers, warnings, and handoffs with targeting and routing metadata.
- [Inbox triage](inbox-triage.md) — poll and filter compact metadata, read
  relevant notifications, and dismiss irrelevant ones.
- [Session maintenance](session-maintenance.md) — find stale sessions, preview
  retention, and safely prune old coordination history.
- [Configuration](configuration.md) — global defaults, sparse per-wiki
  overrides, validation, and every coordination/history setting.
- [Storage and concurrency](storage-and-concurrency.md) — on-disk layout,
  append-only state, corruption handling, path containment, atomic writes, and
  serialized Git history.
- [Agent and MCP integration](agent-coordination.md) — polling checkpoints,
  installed guidance, structured MCP results, and tool equivalents.

For the complete command inventory and design, see the project
[README](../README.md) and [specification](../SPEC.md).
