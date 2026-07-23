//! Stable, machine-readable diagnostics shared by Wookie's audit commands.
//!
//! Human wording may improve over time; consumers should branch on `schema`,
//! `code`, and `severity`. Diagnostic codes are strings rather than a closed
//! enum so a v1 reader can safely preserve codes added by a newer producer.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
#[cfg(test)]
use std::fmt::Write as _;

pub const REPORT_SCHEMA: &str = "wookie.report/v1";

/// Escape terminal control characters without changing the underlying value
/// used by JSON consumers. Human renderers should apply this at their final
/// interpolation boundary.
pub fn terminal_safe(value: &str) -> String {
    value
        .chars()
        .flat_map(|character| {
            if character.is_control() {
                character.escape_default().collect::<Vec<_>>()
            } else {
                vec![character]
            }
        })
        .collect()
}

/// Stable diagnostic codes emitted by Wookie itself.
pub mod code {
    pub const BROKEN_LINK: &str = "broken_link";
    pub const INVALID_SOURCE: &str = "invalid_source";
    pub const SOURCE_MISSING: &str = "source_missing";
    pub const RULE_LOCKED: &str = "rule_locked";
    pub const ORPHAN_PAGE: &str = "orphan_page";
    pub const STUB_PAGE: &str = "stub_page";
    pub const MISSING_SUMMARY: &str = "missing_summary";
    pub const MISSING_CHECKS: &str = "missing_checks";
    pub const STALE_PAGE: &str = "stale_page";
    pub const INVALID_PAGE: &str = "invalid_page";
    pub const PUBLISH_CONFLICT: &str = "publish_conflict";
    pub const PUBLISH_PLAN_INVALID: &str = "publish_plan_invalid";
    pub const JOURNAL_RECOVERY_REQUIRED: &str = "journal_recovery_required";
    pub const INGEST_RECOVERY_REQUIRED: &str = "ingest_recovery_required";
    pub const AUDIT_STATE_IN_TRANSITION: &str = "audit_state_in_transition";
    pub const PROTOCOL_CATALOG_INVALID: &str = "protocol_catalog_invalid";
    pub const PROTOCOL_SECTION_INVALID: &str = "protocol_section_invalid";
    pub const FINDING_STATUS_INVALID: &str = "finding_status_invalid";
    pub const FINDING_SEVERITY_INVALID: &str = "finding_severity_invalid";
    pub const FINDING_SOURCES_MISSING: &str = "finding_sources_missing";
    pub const FINDING_SECTIONS_MISSING: &str = "finding_sections_missing";
    pub const PINNED_STANDING_TEXT_INVALID: &str = "pinned_standing_text_invalid";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    Error,
}

impl Severity {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProjectSnapshotMode {
    WorkingTree,
    Staged,
    Revision,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WikiSnapshot {
    pub slug: String,
    /// Fully resolved revision identifier when the wiki is revisioned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectSnapshot {
    pub root: String,
    /// Fully resolved revision identifier, not a moving branch or shorthand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    pub mode: ProjectSnapshotMode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub wiki: WikiSnapshot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<ProjectSnapshot>,
}

impl Snapshot {
    pub fn new(slug: impl Into<String>) -> Self {
        Self {
            wiki: WikiSnapshot {
                slug: slug.into(),
                revision: None,
                content_hash: None,
            },
            project: None,
        }
    }

    pub fn wiki_revision(mut self, revision: impl Into<String>) -> Self {
        self.wiki.revision = Some(revision.into());
        self
    }

    pub fn wiki_content_hash(mut self, content_hash: impl Into<String>) -> Self {
        self.wiki.content_hash = Some(content_hash.into());
        self
    }

    pub fn with_project(mut self, project: ProjectSnapshot) -> Self {
        self.project = Some(project);
        self
    }
}

impl ProjectSnapshot {
    pub fn new(root: impl Into<String>, mode: ProjectSnapshotMode) -> Self {
        Self {
            root: root.into(),
            revision: None,
            mode,
        }
    }

    pub fn revision(mut self, revision: impl Into<String>) -> Self {
        self.revision = Some(revision.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Diagnostic {
    /// Stable snake_case identifier. Human wording in `message` is not an API.
    pub code: String,
    pub severity: Severity,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub data: BTreeMap<String, Value>,
}

impl Diagnostic {
    pub fn new(code: impl Into<String>, severity: Severity, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            severity,
            message: message.into(),
            page: None,
            source: None,
            suggestion: None,
            data: BTreeMap::new(),
        }
    }

    pub fn page(mut self, page: impl Into<String>) -> Self {
        self.page = Some(page.into());
        self
    }

    pub fn source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    pub fn suggestion(mut self, suggestion: impl Into<String>) -> Self {
        self.suggestion = Some(suggestion.into());
        self
    }

    pub fn data(mut self, key: impl Into<String>, value: impl Into<Value>) -> Self {
        self.data.insert(key.into(), value.into());
        self
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Summary {
    pub errors: usize,
    pub warnings: usize,
    pub info: usize,
    pub total: usize,
}

impl Summary {
    pub fn from_diagnostics(diagnostics: &[Diagnostic]) -> Self {
        let mut summary = Self::default();
        for diagnostic in diagnostics {
            match diagnostic.severity {
                Severity::Error => summary.errors += 1,
                Severity::Warning => summary.warnings += 1,
                Severity::Info => summary.info += 1,
            }
        }
        summary.total = diagnostics.len();
        summary
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub schema: String,
    /// Stable producer discriminator such as `doctor`, `critique`, or
    /// `publish-check`. Consumers must not infer it from human output.
    pub command: String,
    pub generated_at: String,
    pub snapshot: Snapshot,
    pub summary: Summary,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub diagnostics: Vec<Diagnostic>,
    /// Command-specific aggregates. Diagnostic details belong in
    /// `diagnostics`, while optional telemetry can live here.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub data: BTreeMap<String, Value>,
}

impl Report {
    pub fn new(command: impl Into<String>, snapshot: Snapshot) -> Self {
        Self {
            schema: REPORT_SCHEMA.to_string(),
            command: command.into(),
            generated_at: chrono::Utc::now().to_rfc3339(),
            snapshot,
            summary: Summary::default(),
            diagnostics: Vec::new(),
            data: BTreeMap::new(),
        }
    }

    pub fn with_diagnostics(
        command: impl Into<String>,
        snapshot: Snapshot,
        diagnostics: Vec<Diagnostic>,
    ) -> Self {
        let summary = Summary::from_diagnostics(&diagnostics);
        Self {
            schema: REPORT_SCHEMA.to_string(),
            command: command.into(),
            generated_at: chrono::Utc::now().to_rfc3339(),
            snapshot,
            summary,
            diagnostics,
            data: BTreeMap::new(),
        }
    }

    pub fn push(&mut self, diagnostic: Diagnostic) {
        self.diagnostics.push(diagnostic);
        self.recompute_summary();
    }

    pub fn extend(&mut self, diagnostics: impl IntoIterator<Item = Diagnostic>) {
        self.diagnostics.extend(diagnostics);
        self.recompute_summary();
    }

    pub fn insert_data(&mut self, key: impl Into<String>, value: impl Into<Value>) {
        self.data.insert(key.into(), value.into());
    }

    pub fn recompute_summary(&mut self) {
        self.summary = Summary::from_diagnostics(&self.diagnostics);
    }

    pub fn has_errors(&self) -> bool {
        self.diagnostics
            .iter()
            .any(|diagnostic| diagnostic.severity == Severity::Error)
    }

    /// Compact operator-facing projection of the same report. The JSON form
    /// remains the compatibility contract for automation. Every persisted or
    /// external string is escaped before interpolation so a corrupt legacy
    /// page cannot inject terminal controls or forge additional output lines.
    #[cfg(test)]
    pub fn render_human(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "Wiki '{}': {} error(s), {} warning(s), {} info",
            terminal_safe(&self.snapshot.wiki.slug),
            self.summary.errors,
            self.summary.warnings,
            self.summary.info
        );
        if let Some(project) = &self.snapshot.project {
            let revision = project.revision.as_deref().unwrap_or("unresolved");
            let _ = writeln!(
                out,
                "Project: {} ({:?}, {})",
                terminal_safe(&project.root),
                project.mode,
                terminal_safe(revision)
            );
        }
        for diagnostic in &self.diagnostics {
            let location = diagnostic
                .page
                .as_deref()
                .or(diagnostic.source.as_deref())
                .map(|value| format!(" [{}]", terminal_safe(value)))
                .unwrap_or_default();
            let _ = writeln!(
                out,
                "{} {}{}: {}",
                diagnostic.severity.as_str().to_uppercase(),
                terminal_safe(&diagnostic.code),
                location,
                terminal_safe(&diagnostic.message)
            );
            if let Some(suggestion) = &diagnostic.suggestion {
                let _ = writeln!(out, "  Suggestion: {}", terminal_safe(suggestion));
            }
        }
        out.trim_end().to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_counts_and_round_trips() {
        let snapshot = Snapshot::new("example").wiki_revision("abc").with_project(
            ProjectSnapshot::new("/project", ProjectSnapshotMode::Revision).revision("def"),
        );
        let report = Report::with_diagnostics(
            "doctor",
            snapshot,
            vec![
                Diagnostic::new(code::BROKEN_LINK, Severity::Error, "missing target")
                    .page("architecture/overview")
                    .suggestion("create the target"),
                Diagnostic::new(code::ORPHAN_PAGE, Severity::Warning, "not linked"),
                Diagnostic::new(code::STALE_PAGE, Severity::Info, "source changed"),
            ],
        );

        assert_eq!(report.summary.errors, 1);
        assert_eq!(report.summary.warnings, 1);
        assert_eq!(report.summary.info, 1);
        assert_eq!(report.summary.total, 3);
        assert!(report.has_errors());

        let json = serde_json::to_string_pretty(&report).unwrap();
        let decoded: Report = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, report);
        assert_eq!(decoded.schema, REPORT_SCHEMA);
    }

    #[test]
    fn mutations_keep_summary_consistent() {
        let mut report = Report::new("doctor", Snapshot::new("example"));
        report.push(Diagnostic::new(
            code::MISSING_SUMMARY,
            Severity::Warning,
            "summary missing",
        ));
        report.extend([Diagnostic::new(
            code::SOURCE_MISSING,
            Severity::Error,
            "source missing",
        )]);

        assert_eq!(report.summary.total, report.diagnostics.len());
        assert_eq!(report.summary.errors, 1);
        assert_eq!(report.summary.warnings, 1);
    }

    #[test]
    fn human_render_includes_stable_code_and_location() {
        let report = Report::with_diagnostics(
            "doctor",
            Snapshot::new("example"),
            vec![
                Diagnostic::new(code::SOURCE_MISSING, Severity::Error, "does not exist")
                    .source("src/missing.rs"),
            ],
        );
        let rendered = report.render_human();
        assert!(rendered.contains("ERROR source_missing [src/missing.rs]"));
        assert!(rendered.contains("1 error(s)"));
    }

    #[test]
    fn human_render_escapes_terminal_controls_but_json_preserves_data() {
        let message = "bad\nforged\u{1b}[2J\u{85}tail";
        let report = Report::with_diagnostics(
            "doctor",
            Snapshot::new("wiki\u{1b}").with_project(ProjectSnapshot::new(
                "/tmp/project\nforged",
                ProjectSnapshotMode::WorkingTree,
            )),
            vec![
                Diagnostic::new(code::INVALID_PAGE, Severity::Error, message)
                    .page("page\nforged")
                    .suggestion("fix\ttab"),
            ],
        );

        let rendered = report.render_human();
        assert!(!rendered.contains('\u{1b}'));
        assert!(rendered.contains(r"wiki\u{1b}"));
        assert!(rendered.contains(r"page\nforged"));
        assert!(rendered.contains(r"bad\nforged\u{1b}[2J\u{85}tail"));
        assert!(rendered.contains(r"fix\ttab"));

        let decoded: Report =
            serde_json::from_str(&serde_json::to_string(&report).unwrap()).unwrap();
        assert_eq!(decoded.diagnostics[0].message, message);
    }
}
