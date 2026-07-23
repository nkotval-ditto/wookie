//! Shared, revision-aware wiki health diagnostics.
//!
//! The CLI, publish preflight, and MCP surface should all consume this report
//! instead of implementing subtly different scanners. Audits are read-only:
//! repair remains an explicit command concern.

use crate::page::{Page, PinLevel};
use crate::protocol;
use crate::publish;
use crate::report::{
    code, Diagnostic, ProjectSnapshot, ProjectSnapshotMode, Report, Severity, Snapshot,
};
use crate::retrieval::estimate_standing_tokens;
use crate::sessions;
use crate::snapshot;
use crate::wiki::{SectionKind, Wiki};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt::Write as _;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

const MAX_GIT_REVISION_BYTES: usize = 512;
const MAX_AUDIT_GIT_TEXT_BYTES: usize = 64 * 1024;
const MAX_AUDIT_GIT_PATHS: usize = 200_000;
const MAX_AUDIT_GIT_PATH_BYTES: usize = 32 * 1024 * 1024;

/// Selects the code snapshot against which provenance and staleness are
/// checked. With no explicit revision, the working tree is inspected.
#[derive(Debug, Clone, Default)]
pub struct AuditOptions {
    pub project_root: Option<PathBuf>,
    pub project_revision: Option<String>,
}

#[derive(Debug)]
struct ProjectView {
    root: PathBuf,
    revision: Option<String>,
    mode: ProjectSnapshotMode,
    root_available: bool,
    revision_valid: bool,
}

impl ProjectView {
    fn snapshot(&self) -> ProjectSnapshot {
        ProjectSnapshot {
            root: self.root.to_string_lossy().to_string(),
            revision: self.revision.clone(),
            mode: self.mode.clone(),
        }
    }
}

fn command_output(root: &Path, args: &[&str]) -> Result<String> {
    let output = command_output_bytes(root, args)?;
    let output = String::from_utf8(output).with_context(|| {
        format!(
            "git -C {} {} returned non-UTF-8 text",
            root.display(),
            args.join(" ")
        )
    })?;
    Ok(output.trim().to_string())
}

fn command_output_bytes(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    crate::git_paths::bounded_git_stdout(
        root,
        args,
        &format!("git {}", args.join(" ")),
        MAX_AUDIT_GIT_TEXT_BYTES,
    )
}

fn command_path_output_bytes(root: &Path, args: &[&str], label: &str) -> Result<Vec<u8>> {
    let mut literal_args = Vec::with_capacity(args.len() + 1);
    literal_args.push("--literal-pathspecs");
    literal_args.extend_from_slice(args);
    crate::git_paths::bounded_git_stdout(root, &literal_args, label, MAX_AUDIT_GIT_PATH_BYTES)
}

fn validate_audit_paths(paths: Vec<String>, label: &str) -> Result<Vec<String>> {
    crate::git_paths::validate_path_inventory(
        paths,
        label,
        MAX_AUDIT_GIT_PATHS,
        MAX_AUDIT_GIT_PATH_BYTES,
    )
}

fn git_revision(root: &Path) -> Option<String> {
    command_output(root, &["rev-parse", "HEAD"])
        .ok()
        .filter(|revision| !revision.is_empty())
}

fn project_root(w: &Wiki, options: &AuditOptions) -> Option<PathBuf> {
    options
        .project_root
        .clone()
        .or_else(|| w.config.project_roots.first().map(PathBuf::from))
        .map(|root| root.canonicalize().unwrap_or(root))
}

fn project_view(
    w: &Wiki,
    options: &AuditOptions,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<ProjectView> {
    let root = project_root(w, options)?;
    let root_available = root.is_dir();
    if !root_available {
        diagnostics.push(
            Diagnostic::new(
                "project_root_unavailable",
                Severity::Error,
                format!(
                    "project root '{}' is not a readable directory",
                    root.display()
                ),
            )
            .source(root.to_string_lossy())
            .suggestion("Pass --project-root with a readable checkout."),
        );
    }

    if let Some(requested) = options.project_revision.as_deref() {
        let requested = requested.trim();
        if requested.is_empty()
            || requested.len() > MAX_GIT_REVISION_BYTES
            || requested.starts_with('-')
            || requested.chars().any(char::is_control)
        {
            diagnostics.push(
                Diagnostic::new(
                    "project_revision_invalid",
                    Severity::Error,
                    format!(
                        "the explicit project revision is empty, option-like, control-bearing, or longer than {MAX_GIT_REVISION_BYTES} bytes"
                    ),
                )
                .suggestion("Pass a commit, tag, or other Git revision."),
            );
            return Some(ProjectView {
                root,
                revision: None,
                mode: ProjectSnapshotMode::Revision,
                root_available,
                revision_valid: false,
            });
        }
        if !root_available {
            return Some(ProjectView {
                root,
                revision: Some(requested.to_string()),
                mode: ProjectSnapshotMode::Revision,
                root_available: false,
                revision_valid: false,
            });
        }
        let commitish = format!("{requested}^{{commit}}");
        match command_output(&root, &["rev-parse", "--verify", &commitish]) {
            Ok(resolved) => Some(ProjectView {
                root,
                revision: Some(resolved),
                mode: ProjectSnapshotMode::Revision,
                root_available,
                revision_valid: true,
            }),
            Err(error) => {
                diagnostics.push(
                    Diagnostic::new(
                        "project_revision_invalid",
                        Severity::Error,
                        format!("cannot resolve project revision '{requested}': {error:#}"),
                    )
                    .source(requested)
                    .suggestion("Use a revision that exists in the selected project checkout."),
                );
                Some(ProjectView {
                    root,
                    revision: Some(requested.to_string()),
                    mode: ProjectSnapshotMode::Revision,
                    root_available,
                    revision_valid: false,
                })
            }
        }
    } else {
        let revision = git_revision(&root);
        Some(ProjectView {
            root,
            revision,
            mode: ProjectSnapshotMode::WorkingTree,
            root_available,
            revision_valid: root_available,
        })
    }
}

/// Normalize a project-relative source without touching the filesystem.
/// Rejecting traversal here keeps both working-tree and `revision:path`
/// checks inside the selected project boundary.
pub(crate) fn normalize_source(source: &str) -> Result<String> {
    if source.trim() != source || source.contains('\\') || source.chars().any(char::is_control) {
        anyhow::bail!("source contains whitespace, a backslash, or a control character");
    }
    let source = source.trim_end_matches('/');
    if source.is_empty() {
        anyhow::bail!("source is empty");
    }
    let path = Path::new(source);
    if path.is_absolute() {
        anyhow::bail!("source must be project-relative");
    }
    let mut normalized = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(segment) => normalized.push(segment.to_string_lossy().to_string()),
            _ => anyhow::bail!("source contains '.' or '..' path traversal"),
        }
    }
    if normalized.is_empty() {
        anyhow::bail!("source is empty");
    }
    Ok(normalized.join("/"))
}

fn source_exists_in_worktree(root: &Path, source: &str) -> Result<bool> {
    let path = root.join(source);
    if !path.exists() {
        return Ok(false);
    }
    let canonical_root = root.canonicalize().with_context(|| {
        format!(
            "resolving project root {} for source validation",
            root.display()
        )
    })?;
    let canonical_path = path
        .canonicalize()
        .with_context(|| format!("resolving source path {}", path.display()))?;
    Ok(canonical_path.starts_with(canonical_root))
}

fn source_exists_at_revision(root: &Path, revision: &str, source: &str) -> Result<bool> {
    let object = format!("{revision}:{source}");
    if Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("--literal-pathspecs")
        .args(["cat-file", "-e", &object])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .with_context(|| format!("checking source '{source}' at revision {revision}"))?
        .success()
    {
        return Ok(true);
    }

    // A trailing directory prefix may not name an object exactly in unusual
    // repositories. Treat it as valid when the revision tracks a descendant.
    let output = command_path_output_bytes(
        root,
        &["ls-tree", "-r", "--name-only", "-z", revision, "--", source],
        "git source-path inventory",
    )?;
    let output = validate_audit_paths(
        crate::git_paths::parse_path_list(&output, "git ls-tree output")?,
        "git source-path inventory",
    )?;
    let prefix = format!("{source}/");
    Ok(output
        .iter()
        .any(|path| path == source || path.starts_with(&prefix)))
}

fn resolve_commit(root: &Path, revision: &str, label: &str) -> Result<String> {
    let revision = revision.trim();
    if revision.is_empty()
        || revision.len() > MAX_GIT_REVISION_BYTES
        || revision.starts_with('-')
        || revision.chars().any(char::is_control)
    {
        anyhow::bail!("{label} is not a safe Git revision");
    }
    command_output(
        root,
        &["rev-parse", "--verify", &format!("{revision}^{{commit}}")],
    )
    .with_context(|| format!("resolving {label} '{revision}'"))
}

fn changed_paths(view: &ProjectView, base: &str) -> Result<Vec<String>> {
    let mut paths = BTreeSet::new();
    let base = resolve_commit(&view.root, base, "last ingest revision")?;
    let diff = if view.mode == ProjectSnapshotMode::Revision {
        let revision = view.revision.as_deref().unwrap_or("HEAD");
        command_path_output_bytes(
            &view.root,
            &[
                "diff",
                "--name-status",
                "-z",
                "--find-renames",
                "--find-copies",
                &format!("{base}..{revision}"),
                "--",
            ],
            "git audit changed-path inventory",
        )?
    } else {
        command_path_output_bytes(
            &view.root,
            &[
                "diff",
                "--name-status",
                "-z",
                "--find-renames",
                "--find-copies",
                &base,
                "--",
            ],
            "git audit changed-path inventory",
        )?
    };
    paths.extend(crate::git_paths::parse_name_status(
        &diff,
        "git diff name-status output",
    )?);

    if view.mode == ProjectSnapshotMode::WorkingTree {
        let untracked = command_path_output_bytes(
            &view.root,
            &["ls-files", "-z", "--others", "--exclude-standard"],
            "git audit untracked-path inventory",
        )?;
        paths.extend(crate::git_paths::parse_path_list(
            &untracked,
            "git ls-files output",
        )?);
    }
    validate_audit_paths(
        paths.into_iter().collect(),
        "combined audit changed-path inventory",
    )
}

fn source_covers_change(source: &str, changed: &str) -> bool {
    source == changed
        || changed.starts_with(&format!("{source}/"))
        || source.starts_with(&format!("{changed}/"))
}

fn page_section(id: &str) -> Option<&str> {
    id.split_once('/').map(|(section, _)| section)
}

pub(crate) fn file_line_sources(page: &Page) -> Vec<String> {
    let Some(line) = page
        .body
        .lines()
        .find(|line| line.trim_start().starts_with("File:"))
    else {
        return Vec::new();
    };
    let mut paths = Vec::new();
    let mut remainder = line;
    while let Some(open) = remainder.find('`') {
        let after_open = &remainder[open + 1..];
        let Some(close) = after_open.find('`') else {
            break;
        };
        let value = after_open[..close].trim();
        if !value.is_empty() {
            paths.push(value.to_string());
        }
        remainder = &after_open[close + 1..];
    }
    paths
}

/// Valid project-relative provenance declared by either frontmatter or the
/// human-readable `File:` line. Validation still diagnoses the two forms
/// independently (and reports mismatches); their normalized union is what
/// staleness and reconciliation use so a declaration cannot disappear at one
/// command boundary.
pub(crate) fn effective_page_sources(page: &Page) -> Vec<String> {
    page.fm
        .sources
        .iter()
        .cloned()
        .chain(file_line_sources(page))
        .filter_map(|source| normalize_source(&source).ok())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn project_state(view: Option<&ProjectView>) -> Result<(Vec<String>, Vec<String>)> {
    let Some(view) =
        view.filter(|view| view.root_available && view.mode == ProjectSnapshotMode::WorkingTree)
    else {
        return Ok((Vec::new(), Vec::new()));
    };
    let output = command_path_output_bytes(
        &view.root,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        "git audit working-tree path inventory",
    )?;
    let parsed = crate::git_paths::parse_porcelain_v1(&output, "git status output")?;
    Ok((
        validate_audit_paths(parsed.dirty, "audit dirty-path inventory")?,
        validate_audit_paths(parsed.staged, "audit staged-path inventory")?,
    ))
}

fn count_codes(diagnostics: &[Diagnostic]) -> BTreeMap<String, usize> {
    let mut counts = BTreeMap::new();
    for diagnostic in diagnostics {
        *counts.entry(diagnostic.code.clone()).or_default() += 1;
    }
    counts
}

fn diagnose_source(page: &Page, source: &str, view: Option<&ProjectView>) -> Option<Diagnostic> {
    let normalized = match normalize_source(source) {
        Ok(normalized) => normalized,
        Err(error) => {
            return Some(
                Diagnostic::new(
                    code::INVALID_SOURCE,
                    Severity::Error,
                    format!("page '{}' has invalid source '{source}': {error}", page.id),
                )
                .page(&page.id)
                .source(source)
                .suggestion("Use a normalized path relative to the project root."),
            );
        }
    };
    let Some(view) = view else {
        return Some(
            Diagnostic::new(
                "source_unverifiable",
                Severity::Warning,
                format!(
                    "cannot verify source '{normalized}' for page '{}' without a project root",
                    page.id
                ),
            )
            .page(&page.id)
            .source(&normalized)
            .suggestion("Register a project root or pass --project-root."),
        );
    };
    if !view.root_available {
        return Some(
            Diagnostic::new(
                "source_unverifiable",
                Severity::Warning,
                format!(
                    "cannot verify source '{normalized}' for page '{}' because the project root is unavailable",
                    page.id
                ),
            )
            .page(&page.id)
            .source(&normalized)
            .suggestion("Fix the project root or pass --project-root with a readable checkout."),
        );
    }
    if view.mode == ProjectSnapshotMode::Revision && !view.revision_valid {
        return Some(
            Diagnostic::new(
                "source_unverifiable",
                Severity::Warning,
                format!(
                    "cannot verify source '{normalized}' for page '{}' at an invalid revision",
                    page.id
                ),
            )
            .page(&page.id)
            .source(&normalized)
            .suggestion("Fix the project revision and rerun the audit."),
        );
    }

    let exists = match &view.mode {
        ProjectSnapshotMode::Revision => view
            .revision
            .as_deref()
            .map(|revision| source_exists_at_revision(&view.root, revision, &normalized))
            .transpose(),
        ProjectSnapshotMode::WorkingTree | ProjectSnapshotMode::Staged => {
            Some(source_exists_in_worktree(&view.root, &normalized)).transpose()
        }
    };
    match exists {
        Ok(Some(true)) => None,
        Ok(Some(false)) => Some(
            Diagnostic::new(
                code::SOURCE_MISSING,
                Severity::Error,
                format!(
                    "source '{normalized}' for page '{}' does not exist in the audited project snapshot",
                    page.id
                ),
            )
            .page(&page.id)
            .source(&normalized)
            .suggestion("Update the page source metadata or restore the documented file."),
        ),
        Ok(None) => Some(
            Diagnostic::new(
                "source_unverifiable",
                Severity::Warning,
                format!("cannot verify source '{normalized}' for page '{}'", page.id),
            )
            .page(&page.id)
            .source(&normalized),
        ),
        Err(error) => Some(
            Diagnostic::new(
                "source_check_failed",
                Severity::Error,
                format!(
                    "failed to verify source '{normalized}' for page '{}': {error:#}",
                    page.id
                ),
            )
            .page(&page.id)
            .source(&normalized)
            .suggestion("Verify that Git and the selected project snapshot are readable."),
        ),
    }
}

fn capture_live_catalog(w: &Wiki) -> Result<(Option<String>, Vec<Page>, Vec<Diagnostic>)> {
    let captured = snapshot::capture_catalog(w)?;
    let mut pages = Vec::with_capacity(captured.pages.len());
    let mut diagnostics = Vec::new();
    for page in captured.pages {
        match std::str::from_utf8(&page.raw) {
            Ok(text) => pages.push(Page::parse(&page.id, text)),
            Err(error) => diagnostics.push(
                Diagnostic::new(
                    code::INVALID_PAGE,
                    Severity::Error,
                    format!("page '{}' is not valid UTF-8: {error}", page.id),
                )
                .page(&page.id),
            ),
        }
    }
    Ok((Some(captured.content_hash), pages, diagnostics))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum WriterLockObservation {
    Absent,
    Present,
    Unreadable(String),
}

fn observe_writer_lock(w: &Wiki) -> WriterLockObservation {
    let path = w.dir.join(publish::PUBLISH_LOCK_PATH);
    match std::fs::symlink_metadata(&path) {
        Ok(_) => WriterLockObservation::Present,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => WriterLockObservation::Absent,
        Err(error) => WriterLockObservation::Unreadable(error.to_string()),
    }
}

fn note_writer_lock_transition(
    before: &WriterLockObservation,
    after: &WriterLockObservation,
    phase: &str,
    reasons: &mut Vec<String>,
) {
    if before != after {
        reasons.push(format!("the writer lock changed during {phase}"));
    }
    match (before, after) {
        (WriterLockObservation::Present, _) | (_, WriterLockObservation::Present) => {
            reasons.push(format!("a writer lock existed during {phase}"));
        }
        (WriterLockObservation::Unreadable(error), _)
        | (_, WriterLockObservation::Unreadable(error)) => {
            reasons.push(format!(
                "the writer lock could not be inspected during {phase}: {error}"
            ));
        }
        _ => {}
    }
}

fn finding_diagnostics(page: &Page) -> Vec<Diagnostic> {
    const SEVERITIES: [&str; 5] = [
        "severity/critical",
        "severity/high",
        "severity/medium",
        "severity/low",
        "severity/info",
    ];
    const REQUIRED_HEADINGS: [&str; 3] = ["## Owner", "## Remediation", "## Verification evidence"];

    let mut diagnostics = Vec::new();
    let status_tags = page
        .fm
        .tags
        .iter()
        .filter(|tag| tag.starts_with("status/"))
        .cloned()
        .collect::<Vec<_>>();
    let valid_statuses = status_tags
        .iter()
        .filter(|tag| tag.len() > "status/".len())
        .count();
    if status_tags.len() != 1 || valid_statuses != 1 {
        diagnostics.push(
            Diagnostic::new(
                code::FINDING_STATUS_INVALID,
                Severity::Error,
                format!(
                    "finding '{}' must have exactly one non-empty status/* tag (found {})",
                    page.id,
                    status_tags.len()
                ),
            )
            .page(&page.id)
            .suggestion("Replace the finding's status tags with exactly one status/<state> tag.")
            .data("status_tags", json!(status_tags)),
        );
    }

    let severity_tags = page
        .fm
        .tags
        .iter()
        .filter(|tag| tag.starts_with("severity/"))
        .cloned()
        .collect::<Vec<_>>();
    let recognized = severity_tags
        .iter()
        .filter(|tag| SEVERITIES.contains(&tag.as_str()))
        .count();
    if severity_tags.len() != 1 || recognized != 1 {
        diagnostics.push(
            Diagnostic::new(
                code::FINDING_SEVERITY_INVALID,
                Severity::Error,
                format!(
                    "finding '{}' must have exactly one recognized severity/* tag (found {} total, {} recognized)",
                    page.id,
                    severity_tags.len(),
                    recognized
                ),
            )
            .page(&page.id)
            .suggestion(
                "Set exactly one of severity/critical, severity/high, severity/medium, severity/low, or severity/info.",
            )
            .data("severity_tags", json!(severity_tags)),
        );
    }

    if page.fm.sources.is_empty() {
        diagnostics.push(
            Diagnostic::new(
                code::FINDING_SOURCES_MISSING,
                Severity::Warning,
                format!("finding '{}' does not name any affected files", page.id),
            )
            .page(&page.id)
            .suggestion("Add affected project-relative files to the page's sources metadata."),
        );
    }

    let headings = page.body.lines().map(str::trim).collect::<HashSet<_>>();
    let missing_headings = REQUIRED_HEADINGS
        .into_iter()
        .filter(|heading| !headings.contains(heading))
        .collect::<Vec<_>>();
    if !missing_headings.is_empty() {
        diagnostics.push(
            Diagnostic::new(
                code::FINDING_SECTIONS_MISSING,
                Severity::Warning,
                format!(
                    "finding '{}' is missing {} required body section(s)",
                    page.id,
                    missing_headings.len()
                ),
            )
            .page(&page.id)
            .suggestion("Add Owner, Remediation, and Verification evidence level-two headings.")
            .data("missing_headings", json!(missing_headings)),
        );
    }
    diagnostics
}

/// Produce the stable report consumed by doctor, status, publish preflight,
/// and CI. This function never mutates wiki or project state.
pub fn audit(w: &Wiki, options: &AuditOptions) -> Result<Report> {
    audit_for(w, options, "doctor")
}

/// Run the shared audit for a named producer. `status` and publish preflight
/// use this entry point so the JSON command discriminator remains explicit.
pub fn audit_for(w: &Wiki, options: &AuditOptions, command: &str) -> Result<Report> {
    audit_catalog(w, options, command, None)
}

/// Audit a complete in-memory page catalog. Transactional publish uses this
/// to report the health Wookie expects after the proposed overlay is applied,
/// without writing temporary pages or weakening the ordinary audit checks.
pub fn audit_pages(
    w: &Wiki,
    options: &AuditOptions,
    command: &str,
    pages: &[Page],
) -> Result<Report> {
    audit_catalog(w, options, command, Some(pages.to_vec()))
}

fn rendered_catalog_hash(pages: &[Page]) -> Result<String> {
    let rendered = pages
        .iter()
        .map(|page| (page.id.as_str(), page.render().into_bytes()))
        .collect::<Vec<_>>();
    snapshot::catalog_content_hash_from_raw(rendered.iter().map(|(id, raw)| (*id, raw.as_slice())))
}

fn audit_catalog(
    w: &Wiki,
    options: &AuditOptions,
    command: &str,
    supplied_pages: Option<Vec<Page>>,
) -> Result<Report> {
    let mut diagnostics = Vec::new();
    let live_catalog = supplied_pages.is_none();
    let (content_hash, pages, wiki_revision, mut transition_reasons) = match supplied_pages {
        Some(pages) => (
            Some(rendered_catalog_hash(&pages)?),
            pages,
            git_revision(&w.dir),
            Vec::new(),
        ),
        None => {
            // Audits must remain available while an interrupted publish or
            // ingest marker exists so they can report the recovery condition.
            // `capture_live_catalog` uses the strict, retrying catalog
            // snapshot, while the revision comparison below rejects a Git
            // transition during capture; an exclusive writer lock is neither
            // necessary nor appropriate for this read-only operation.
            let lock_before = observe_writer_lock(w);
            let revision_before = git_revision(&w.dir);
            let (hash, pages, catalog_diagnostics) = capture_live_catalog(w)?;
            let revision_after = git_revision(&w.dir);
            let lock_after = observe_writer_lock(w);
            let mut transition_reasons = Vec::new();
            note_writer_lock_transition(
                &lock_before,
                &lock_after,
                "catalog capture",
                &mut transition_reasons,
            );
            if revision_before != revision_after {
                transition_reasons.push("the wiki revision changed during catalog capture".into());
            }
            diagnostics.extend(catalog_diagnostics);
            (hash, pages, revision_after, transition_reasons)
        }
    };
    let view = project_view(w, options, &mut diagnostics);
    let mut snapshot = Snapshot::new(&w.slug);
    snapshot.wiki.revision = wiki_revision;
    snapshot.wiki.content_hash = content_hash.clone();
    if let Some(view) = &view {
        snapshot = snapshot.with_project(view.snapshot());
    }

    let (_, notification_warnings) = sessions::inspect_notifications(w);
    diagnostics.extend(notification_warnings.into_iter().map(|warning| {
        Diagnostic::new(
            "invalid_notification_storage",
            Severity::Error,
            format!(
                "invalid notification storage '{}': {}",
                warning.path, warning.message
            ),
        )
        .source(warning.path)
        .suggestion("Repair or remove the malformed notification entry.")
    }));
    match sessions::list_with_options(w, &sessions::SessionListRequest::default()) {
        Ok(listing) => diagnostics.extend(listing.warnings.into_iter().map(|warning| {
            Diagnostic::new(
                "invalid_session_storage",
                Severity::Error,
                format!(
                    "invalid session storage '{}': {}",
                    warning.path, warning.message
                ),
            )
            .source(warning.path)
            .suggestion("Repair or remove the malformed session entry.")
        })),
        Err(error) => diagnostics.push(
            Diagnostic::new(
                "session_storage_check_failed",
                Severity::Error,
                format!("could not inspect session storage: {error:#}"),
            )
            .suggestion("Check the wiki's session storage permissions and structure."),
        ),
    }

    let ids: HashSet<&str> = pages.iter().map(|page| page.id.as_str()).collect();
    let linked: HashSet<String> = pages.iter().flat_map(Page::links).collect();
    let sections = w.sections();
    match protocol::list(&w.dir) {
        Ok(protocols) => {
            for item in protocols {
                let Some(section) = item.section.as_deref() else {
                    continue;
                };
                if !sections.contains_key(section) {
                    diagnostics.push(
                        Diagnostic::new(
                            code::PROTOCOL_SECTION_INVALID,
                            Severity::Error,
                            format!(
                                "protocol '{}' declares unknown section '{}'",
                                item.name, section
                            ),
                        )
                        .source(format!("protocols/{}.md", item.name))
                        .suggestion(
                            "Use an effective wiki section or remove the protocol's section declaration.",
                        )
                        .data("protocol", json!(item.name))
                        .data("section", json!(section)),
                    );
                }
            }
        }
        Err(error) => diagnostics.push(
            Diagnostic::new(
                code::PROTOCOL_CATALOG_INVALID,
                Severity::Error,
                format!("could not load the protocol catalog: {error:#}"),
            )
            .source("protocols")
            .suggestion("Repair or remove the invalid protocol template or storage entry."),
        ),
    }
    let mut finding_count = 0usize;
    let mut unresolved_finding_count = 0usize;
    let mut instruction_tokens = 0usize;

    for page in &pages {
        if let Err(error) = page.validate_frontmatter() {
            diagnostics.push(
                Diagnostic::new(
                    code::INVALID_PAGE,
                    Severity::Error,
                    format!("page '{}' has invalid metadata: {error:#}", page.id),
                )
                .page(&page.id)
                .suggestion(
                    "Remove control characters and use normalized project-relative sources.",
                )
                .data("kind", json!("invalid_metadata")),
            );
        }
        for target in page.links() {
            if !ids.contains(target.as_str()) {
                diagnostics.push(
                    Diagnostic::new(
                        code::BROKEN_LINK,
                        Severity::Error,
                        format!("broken link [[{target}]] in '{}'", page.id),
                    )
                    .page(&page.id)
                    .source(&target)
                    .suggestion("Create the target page, remove the link, or correct its id."),
                );
            }
        }
        if page.fm.created.is_empty() {
            diagnostics.push(
                Diagnostic::new(
                    "page_frontmatter_missing",
                    Severity::Warning,
                    format!("page '{}' has missing or invalid frontmatter", page.id),
                )
                .page(&page.id)
                .suggestion("Run `wookie doctor --fix` to normalize recoverable frontmatter."),
            );
        }
        if page.fm.description.is_empty() || page.fm.description.starts_with("TODO") {
            diagnostics.push(
                Diagnostic::new(
                    "page_description_missing",
                    Severity::Warning,
                    format!("page '{}' has no useful description", page.id),
                )
                .page(&page.id)
                .suggestion("Add a one-line frontmatter description."),
            );
        }
        if page.summary().is_empty() || page.summary().starts_with("TODO") {
            diagnostics.push(
                Diagnostic::new(
                    code::MISSING_SUMMARY,
                    Severity::Warning,
                    format!("page '{}' has no standalone summary paragraph", page.id),
                )
                .page(&page.id)
                .suggestion("Open the body with a concise standalone summary."),
            );
        }
        if page.is_stub() {
            diagnostics.push(
                Diagnostic::new(
                    code::STUB_PAGE,
                    Severity::Warning,
                    format!("stub '{}' is awaiting content", page.id),
                )
                .page(&page.id)
                .suggestion("Fill the page with `wookie write`, or remove it if unnecessary."),
            );
        }
        if page.id != "index" && !linked.contains(&page.id) {
            diagnostics.push(
                Diagnostic::new(
                    code::ORPHAN_PAGE,
                    Severity::Warning,
                    format!("page '{}' has no inbound wiki link", page.id),
                )
                .page(&page.id)
                .suggestion("Link it from a related page or the index."),
            );
        }
        match page_section(&page.id) {
            Some(section) if sections.contains_key(section) => {}
            _ if page.id == "index" => {}
            _ => diagnostics.push(
                Diagnostic::new(
                    "page_unfiled",
                    Severity::Warning,
                    format!("page '{}' is not filed under a configured section", page.id),
                )
                .page(&page.id)
                .suggestion("Move it under the best-fitting configured section."),
            ),
        }
        if w.audit.source_provenance {
            if page.id.starts_with("code/") && page.fm.sources.is_empty() {
                diagnostics.push(
                    Diagnostic::new(
                        "source_metadata_missing",
                        Severity::Error,
                        format!("code page '{}' does not declare any sources", page.id),
                    )
                    .page(&page.id)
                    .suggestion("Set project-relative `sources` metadata for this code page."),
                );
            }
            let mut checked_sources = BTreeSet::new();
            for source in &page.fm.sources {
                if let Some(diagnostic) = diagnose_source(page, source, view.as_ref()) {
                    diagnostics.push(diagnostic);
                }
                if let Ok(normalized) = normalize_source(source) {
                    checked_sources.insert(normalized);
                }
            }
            let file_line_sources = file_line_sources(page);
            if page.id.starts_with("code/") && file_line_sources.is_empty() {
                diagnostics.push(
                    Diagnostic::new(
                        "file_source_missing",
                        Severity::Error,
                        format!("code page '{}' has no File: provenance line", page.id),
                    )
                    .page(&page.id)
                    .suggestion("Add a File: line that matches the page's `sources` metadata."),
                );
            }
            for source in &file_line_sources {
                match normalize_source(source) {
                    Ok(normalized) if checked_sources.contains(&normalized) => {}
                    _ => {
                        if let Some(diagnostic) = diagnose_source(page, source, view.as_ref()) {
                            diagnostics.push(diagnostic);
                        }
                    }
                }
            }
            let file_sources: BTreeSet<String> = file_line_sources
                .into_iter()
                .filter_map(|source| normalize_source(&source).ok())
                .collect();
            let metadata_sources: BTreeSet<String> = page
                .fm
                .sources
                .iter()
                .filter_map(|source| normalize_source(source).ok())
                .collect();
            if !file_sources.is_empty() && file_sources != metadata_sources {
                diagnostics.push(
                    Diagnostic::new(
                        "source_metadata_mismatch",
                        Severity::Error,
                        format!(
                            "page '{}' has different File: and frontmatter source paths",
                            page.id
                        ),
                    )
                    .page(&page.id)
                    .suggestion(
                        "Make the File: line and `sources` metadata describe the same paths.",
                    )
                    .data("file_line_sources", json!(file_sources))
                    .data("frontmatter_sources", json!(metadata_sources)),
                );
            }
        }

        if page.fm.tags.iter().any(|tag| tag == "finding") {
            finding_count += 1;
            diagnostics.extend(finding_diagnostics(page));
            let resolved = page.fm.tags.iter().any(|tag| {
                matches!(
                    tag.as_str(),
                    "status/resolved" | "status/closed" | "status/verified" | "status/remediated"
                )
            });
            if !resolved {
                unresolved_finding_count += 1;
                diagnostics.push(
                    Diagnostic::new(
                        "finding_unresolved",
                        Severity::Info,
                        format!("finding '{}' is unresolved", page.id),
                    )
                    .page(&page.id)
                    .suggestion("Record remediation and verification evidence, then resolve it."),
                );
            }
        }

        if matches!(
            page.pin_level(),
            Some(PinLevel::Instruction | PinLevel::Summary)
        ) {
            if let Some(issue) = page.standing_text_issue() {
                diagnostics.push(
                    Diagnostic::new(
                        crate::report::code::PINNED_STANDING_TEXT_INVALID,
                        Severity::Error,
                        format!("pinned standing page '{}' {issue}", page.id),
                    )
                    .page(&page.id)
                    .suggestion("Fill the standing text or unpin the page before priming."),
                );
            }
            instruction_tokens = instruction_tokens
                .saturating_add(estimate_standing_tokens(&page.id, &page.pinned_text()));
        }
    }

    if instruction_tokens > w.retrieval.instruction_tokens {
        diagnostics.push(
            Diagnostic::new(
                "instruction_budget_exceeded",
                Severity::Error,
                format!(
                    "pinned instructions require about {instruction_tokens} tokens, exceeding the configured {}-token budget",
                    w.retrieval.instruction_tokens
                ),
            )
            .suggestion("Shorten pinned instructions, demote reference pages, or raise retrieval.instruction_tokens deliberately."),
        );
    }

    for (section, config) in &sections {
        for required in &config.required {
            let id = format!("{section}/{required}");
            if !ids.contains(id.as_str()) {
                diagnostics.push(
                    Diagnostic::new(
                        "missing_required_page",
                        Severity::Error,
                        format!("configured required page '{id}' does not exist"),
                    )
                    .page(&id)
                    .suggestion("Create and connect the required page."),
                );
            }
        }
        if config.kind == SectionKind::Rules {
            let id = format!("{section}/checks");
            if !ids.contains(id.as_str()) {
                diagnostics.push(
                    Diagnostic::new(
                        code::MISSING_CHECKS,
                        Severity::Error,
                        format!("rules section '{section}' has no checks page"),
                    )
                    .page(&id)
                    .suggestion("Create the checks page through the approved rules workflow."),
                );
            }
        }
    }

    let mut changed = Vec::new();
    if let (Some(base), Some(view)) = (w.config.last_ingest_commit.as_deref(), view.as_ref()) {
        if view.revision_valid {
            match changed_paths(view, base) {
                Ok(paths) => {
                    changed = paths;
                    let mut covered = HashSet::new();
                    for page in &pages {
                        let sources = effective_page_sources(page);
                        let affected: Vec<String> = changed
                            .iter()
                            .filter(|path| {
                                sources
                                    .iter()
                                    .any(|source| source_covers_change(source, path))
                            })
                            .cloned()
                            .collect();
                        if !affected.is_empty() {
                            covered.extend(affected.iter().cloned());
                            diagnostics.push(
                                Diagnostic::new(
                                    code::STALE_PAGE,
                                    Severity::Warning,
                                    format!(
                                        "page '{}' may be stale because {} source path(s) changed",
                                        page.id,
                                        affected.len()
                                    ),
                                )
                                .page(&page.id)
                                .suggestion("Review the changed sources and reconcile this page.")
                                .data("changed_paths", json!(affected)),
                            );
                        }
                    }
                    let uncovered: Vec<String> = changed
                        .iter()
                        .filter(|path| !covered.contains(*path))
                        .cloned()
                        .collect();
                    if !uncovered.is_empty() {
                        diagnostics.push(
                            Diagnostic::new(
                                "uncovered_changes",
                                Severity::Warning,
                                format!(
                                    "{} changed project path(s) are not covered by page sources",
                                    uncovered.len()
                                ),
                            )
                            .suggestion("Run `wookie ingest` to reconcile uncovered changes.")
                            .data("changed_paths", json!(uncovered)),
                        );
                    }
                }
                Err(error) => diagnostics.push(
                    Diagnostic::new(
                        "ingest_compare_failed",
                        Severity::Error,
                        format!("cannot compare the project to the last ingest: {error:#}"),
                    )
                    .source(base)
                    .suggestion("Verify the stored ingest commit and project repository."),
                ),
            }
        }
    }

    let locked_sections: Vec<String> = sections
        .iter()
        .filter(|(section, config)| config.is_locked() && !w.is_unlocked(section))
        .map(|(section, _)| section.clone())
        .collect();
    let unlocked_rule_sections: Vec<String> = sections
        .iter()
        .filter(|(section, config)| config.kind == SectionKind::Rules && w.is_unlocked(section))
        .map(|(section, _)| section.clone())
        .collect();
    for section in &unlocked_rule_sections {
        diagnostics.push(
            Diagnostic::new(
                "rule_section_unlocked",
                Severity::Warning,
                format!("rules section '{section}' is currently unlocked"),
            )
            .source(section)
            .suggestion("Relock it after the approved rule edit is complete."),
        );
    }

    let recovery = match publish::recovery_status(w) {
        Ok(Some(status)) => {
            diagnostics.push(
                Diagnostic::new(
                    code::JOURNAL_RECOVERY_REQUIRED,
                    Severity::Error,
                    format!(
                        "publish transaction '{}' requires recovery ({:?}, {}/{} paths applied)",
                        status.transaction_id, status.state, status.applied, status.total
                    ),
                )
                .suggestion(
                    "Inspect the transaction and run the explicit publish recovery workflow.",
                ),
            );
            Some(json!(status))
        }
        Ok(None) => None,
        Err(error) => {
            diagnostics.push(
                Diagnostic::new(
                    code::JOURNAL_RECOVERY_REQUIRED,
                    Severity::Error,
                    format!("cannot read the publish recovery journal: {error:#}"),
                )
                .suggestion("Do not publish again until the journal is inspected and recovered."),
            );
            Some(json!({"unreadable": true}))
        }
    };
    let ingest_recovery_path =
        w.contained_path(Path::new(".ingest-reconciliation-recovery.json"))?;
    let ingest_recovery = match std::fs::symlink_metadata(&ingest_recovery_path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            diagnostics.push(
                Diagnostic::new(
                    code::INGEST_RECOVERY_REQUIRED,
                    Severity::Error,
                    format!("cannot inspect the ingest recovery marker: {error}"),
                )
                .suggestion(
                    "Do not mutate the wiki until `wookie ingest --recover accept|rollback` succeeds.",
                ),
            );
            true
        }
        Ok(metadata) => {
            let detail = if metadata.file_type().is_symlink() || !metadata.is_file() {
                "the ingest recovery marker is not a regular file"
            } else {
                "an interrupted ingest reconciliation requires recovery"
            };
            diagnostics.push(
                Diagnostic::new(code::INGEST_RECOVERY_REQUIRED, Severity::Error, detail)
                    .suggestion(
                        "Inspect wiki history, then run `wookie ingest --recover accept|rollback`.",
                    ),
            );
            true
        }
    };

    let (dirty_paths, staged_paths) = match project_state(view.as_ref()) {
        Ok(paths) => paths,
        Err(error) => {
            diagnostics.push(
                Diagnostic::new(
                    "project_state_invalid",
                    Severity::Error,
                    format!("cannot safely represent project Git paths: {error:#}"),
                )
                .suggestion("Rename unsafe paths before generating CI audit reports."),
            );
            (Vec::new(), Vec::new())
        }
    };
    if live_catalog {
        let lock_before = observe_writer_lock(w);
        let final_catalog = snapshot::capture_catalog(w);
        let revision_after_audit = git_revision(&w.dir);
        let lock_after = observe_writer_lock(w);
        note_writer_lock_transition(
            &lock_before,
            &lock_after,
            "final audit verification",
            &mut transition_reasons,
        );

        let mut drift_reasons = Vec::new();
        match final_catalog {
            Ok(catalog) => {
                if content_hash.as_deref() != Some(catalog.content_hash.as_str()) {
                    drift_reasons.push("the wiki catalog changed before audit completion".into());
                }
            }
            Err(error) => drift_reasons.push(format!(
                "the final wiki catalog could not be verified: {error:#}"
            )),
        }
        if snapshot.wiki.revision != revision_after_audit {
            drift_reasons.push("the wiki revision changed before audit completion".into());
        }
        if !drift_reasons.is_empty() && transition_reasons.is_empty() {
            anyhow::bail!(
                "wiki state changed during audit; retry after concurrent or external writes finish ({})",
                drift_reasons.join("; ")
            );
        }
        transition_reasons.extend(drift_reasons);
        transition_reasons.sort();
        transition_reasons.dedup();
        if !transition_reasons.is_empty() {
            // A writer can place new page bytes before its history commit. Do
            // not associate that catalog with an older Git revision merely
            // because both individual reads succeeded.
            snapshot.wiki.revision = None;
            diagnostics.push(
                Diagnostic::new(
                    code::AUDIT_STATE_IN_TRANSITION,
                    Severity::Error,
                    format!(
                        "wiki state was in transition during audit: {}",
                        transition_reasons.join("; ")
                    ),
                )
                .suggestion("Wait for the writer to finish, then rerun the audit."),
            );
        }
    }
    let code_counts = count_codes(&diagnostics);
    let mut report = Report::with_diagnostics(command, snapshot, diagnostics);
    report.data.insert("page_count".into(), json!(pages.len()));
    report
        .data
        .insert("changed_project_paths".into(), json!(changed));
    report
        .data
        .insert("project_dirty_paths".into(), json!(dirty_paths));
    report
        .data
        .insert("project_staged_paths".into(), json!(staged_paths));
    report
        .data
        .insert("diagnostic_counts".into(), json!(code_counts));
    report
        .data
        .insert("locked_sections".into(), json!(locked_sections));
    report.data.insert(
        "unlocked_rule_sections".into(),
        json!(unlocked_rule_sections),
    );
    report
        .data
        .insert("finding_count".into(), json!(finding_count));
    report.data.insert(
        "unresolved_finding_count".into(),
        json!(unresolved_finding_count),
    );
    report.data.insert(
        "pinned_instruction_tokens".into(),
        json!(instruction_tokens),
    );
    if let Some(recovery) = recovery {
        report.data.insert("publish_recovery".into(), recovery);
    }
    report
        .data
        .insert("ingest_recovery_required".into(), json!(ingest_recovery));
    report.data.insert(
        "audit_state_in_transition".into(),
        json!(report
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == code::AUDIT_STATE_IN_TRANSITION)),
    );
    Ok(report)
}

fn data_usize(data: &BTreeMap<String, Value>, key: &str) -> usize {
    data.get(key)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or_default()
}

fn diagnostic_count(report: &Report, code: &str) -> usize {
    report
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.code == code)
        .count()
}

/// Compact operator-facing view of the same report used by CI. Detailed
/// diagnostics remain available through JSON or the full doctor rendering.
pub fn render_status(report: &Report) -> String {
    let pages = data_usize(&report.data, "page_count");
    let findings = data_usize(&report.data, "unresolved_finding_count");
    let health = if report.summary.errors == 0 && report.summary.warnings == 0 {
        "healthy"
    } else {
        "needs attention"
    };
    let mut out = format!(
        "Wiki '{}' {health}: {pages} pages; {} errors, {} warnings, {} info.",
        crate::report::terminal_safe(&report.snapshot.wiki.slug),
        report.summary.errors,
        report.summary.warnings,
        report.summary.info
    );
    let _ = write!(
        out,
        "\nBroken links: {} | Stubs: {} | Orphans: {} | Stale pages: {} | Missing checks: {} | Unresolved findings: {}",
        diagnostic_count(report, code::BROKEN_LINK),
        diagnostic_count(report, code::STUB_PAGE),
        diagnostic_count(report, code::ORPHAN_PAGE),
        diagnostic_count(report, code::STALE_PAGE),
        diagnostic_count(report, code::MISSING_CHECKS),
        findings,
    );
    let locked = report
        .data
        .get("locked_sections")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let _ = write!(
        out,
        "\nLocked sections: {}",
        if locked.is_empty() {
            "none".to_string()
        } else {
            crate::report::terminal_safe(&locked)
        }
    );
    if report.summary.total > 0 {
        out.push_str("\nRun `wookie doctor` for the full worklist.");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AuditSettings, HistorySettings, PublishSettings, RetrievalSettings, SessionSettings,
    };
    use crate::page::{today, Frontmatter};
    use crate::wiki::{default_sections, WikiConfig};
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(label: &str) -> PathBuf {
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wookie-audit-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn git(root: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Wookie Test")
            .env("GIT_AUTHOR_EMAIL", "wookie@example.invalid")
            .env("GIT_COMMITTER_NAME", "Wookie Test")
            .env("GIT_COMMITTER_EMAIL", "wookie@example.invalid")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn wiki(dir: PathBuf, project: &Path) -> Wiki {
        fs::create_dir_all(dir.join("pages")).unwrap();
        Wiki {
            slug: "test".into(),
            dir,
            config: WikiConfig {
                name: "test".into(),
                description: String::new(),
                project_roots: vec![project.to_string_lossy().to_string()],
                auto_commit: Some(false),
                sessions: Default::default(),
                history: Default::default(),
                retrieval: Default::default(),
                audit: Default::default(),
                publish: Default::default(),
                last_ingest_commit: None,
                sections: default_sections(),
            },
            auto_commit: false,
            sessions: SessionSettings::default(),
            history: HistorySettings::default(),
            retrieval: RetrievalSettings::default(),
            audit: AuditSettings::default(),
            publish: PublishSettings::default(),
        }
    }

    fn save_page(w: &Wiki, id: &str, body: &str, sources: &[&str]) {
        let mut page = Page {
            id: id.into(),
            fm: Frontmatter {
                title: id.into(),
                description: format!("Description for {id}"),
                created: today(),
                updated: today(),
                sources: sources.iter().map(|source| source.to_string()).collect(),
                ..Default::default()
            },
            body: body.into(),
        };
        w.save_page_raw(&mut page, false).unwrap();
    }

    #[test]
    fn doctor_counts_the_same_framed_standing_entry_as_prime() {
        let root = temp_dir("standing-budget-framing");
        let project = root.join("project");
        fs::create_dir_all(&project).unwrap();
        let mut w = wiki(root.join("wiki"), &project);
        let id = "standing-budget-boundary";
        let body = "**Rule.** Always verify the result.";
        let text_only = crate::retrieval::estimate_tokens(body);
        let framed = estimate_standing_tokens(id, body);
        assert!(framed > text_only);
        w.retrieval.instruction_tokens = text_only;

        let mut page = Page {
            id: id.into(),
            fm: Frontmatter {
                title: "Standing budget boundary".into(),
                description: "A standing instruction at the framing boundary".into(),
                created: today(),
                updated: today(),
                pin: true,
                pin_level: Some(PinLevel::Instruction),
                ..Default::default()
            },
            body: body.into(),
        };
        w.save_page_raw(&mut page, false).unwrap();

        let report = audit(&w, &AuditOptions::default()).unwrap();
        assert!(has_code(&report, "instruction_budget_exceeded", None));
        assert_eq!(
            report.data["pinned_instruction_tokens"].as_u64(),
            Some(u64::try_from(framed).unwrap())
        );
        fs::remove_dir_all(root).unwrap();
    }

    fn has_code(report: &Report, code: &str, page: Option<&str>) -> bool {
        report.diagnostics.iter().any(|diagnostic| {
            diagnostic.code == code
                && page.is_none_or(|page| diagnostic.page.as_deref() == Some(page))
        })
    }

    #[test]
    fn explicit_revision_verifies_files_and_directory_prefixes_at_that_commit() {
        let root = temp_dir("revision");
        let project = root.join("project");
        let wiki_dir = root.join("wiki");
        fs::create_dir_all(project.join("src")).unwrap();
        git(&project, &["init", "-q"]);
        fs::write(project.join("src/live.rs"), "fn live() {}\n").unwrap();
        git(&project, &["add", "src/live.rs"]);
        git(&project, &["commit", "-qm", "initial"]);
        let revision = git(&project, &["rev-parse", "HEAD"]);

        let w = wiki(wiki_dir, &project);
        save_page(
            &w,
            "code/live",
            "**Live module.** Documents [[index]].",
            &["src/live.rs", "src/"],
        );
        save_page(&w, "index", "**Index.** See [[code/live]].", &[]);
        fs::remove_file(project.join("src/live.rs")).unwrap();

        let revision_report = audit(
            &w,
            &AuditOptions {
                project_root: Some(project.clone()),
                project_revision: Some(revision),
            },
        )
        .unwrap();
        assert!(!has_code(
            &revision_report,
            "source_missing",
            Some("code/live")
        ));

        let working_report = audit(
            &w,
            &AuditOptions {
                project_root: Some(project),
                project_revision: None,
            },
        )
        .unwrap();
        assert!(has_code(
            &working_report,
            "source_missing",
            Some("code/live")
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn revision_sources_use_literal_git_paths() {
        let root = temp_dir("literal-revision-source");
        let project = root.join("project");
        fs::create_dir_all(project.join("src")).unwrap();
        git(&project, &["init", "-q"]);
        fs::write(project.join("src/live.rs"), "fn live() {}\n").unwrap();
        fs::write(project.join(":tracked.rs"), "fn colon() {}\n").unwrap();
        git(&project, &["add", "src/live.rs"]);
        git(
            &project,
            &["--literal-pathspecs", "add", "--", ":tracked.rs"],
        );
        git(&project, &["commit", "-qm", "initial"]);
        let revision = git(&project, &["rev-parse", "HEAD"]);

        assert!(source_exists_at_revision(&project, &revision, ":tracked.rs").unwrap());
        assert!(
            !source_exists_at_revision(&project, &revision, ":(literal)src/live.rs").unwrap(),
            "Git pathspec magic must not make an absent source validate against src/live.rs"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_pages_are_mapped_from_changed_sources() {
        let root = temp_dir("stale");
        let project = root.join("project");
        let wiki_dir = root.join("wiki");
        fs::create_dir_all(project.join("src")).unwrap();
        git(&project, &["init", "-q"]);
        fs::write(project.join("src/live.rs"), "const VERSION: u8 = 1;\n").unwrap();
        git(&project, &["add", "src/live.rs"]);
        git(&project, &["commit", "-qm", "initial"]);
        let base = git(&project, &["rev-parse", "HEAD"]);
        fs::write(project.join("src/live.rs"), "const VERSION: u8 = 2;\n").unwrap();
        git(&project, &["commit", "-qam", "change"]);
        let revision = git(&project, &["rev-parse", "HEAD"]);

        let mut w = wiki(wiki_dir, &project);
        w.config.last_ingest_commit = Some(base);
        save_page(
            &w,
            "code/live",
            "**Live module.** Documents [[index]].",
            &["src/"],
        );
        save_page(&w, "index", "**Index.** See [[code/live]].", &[]);

        let report = audit(
            &w,
            &AuditOptions {
                project_root: Some(project),
                project_revision: Some(revision),
            },
        )
        .unwrap();
        assert!(has_code(&report, "stale_page", Some("code/live")));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn git_path_discovery_preserves_controls_unicode_and_rename_endpoints() {
        let root = temp_dir("nul-git-paths");
        let project = root.join("project");
        fs::create_dir_all(&project).unwrap();
        git(&project, &["init", "-q"]);
        let old = "old name.rs";
        let renamed = "renamed-é.rs";
        let with_newline = "line\nbreak.rs";
        let untracked = "untracked space.rs";
        fs::write(project.join(old), "const OLD: bool = true;\n").unwrap();
        fs::write(project.join(with_newline), "const VERSION: u8 = 1;\n").unwrap();
        git(&project, &["add", "--", old, with_newline]);
        git(&project, &["commit", "-qm", "initial"]);
        let base = git(&project, &["rev-parse", "HEAD"]);

        git(&project, &["mv", "--", old, renamed]);
        fs::write(project.join(with_newline), "const VERSION: u8 = 2;\n").unwrap();
        fs::write(project.join(untracked), "const NEW: bool = true;\n").unwrap();
        let view = ProjectView {
            root: project.clone(),
            revision: Some(base.clone()),
            mode: ProjectSnapshotMode::WorkingTree,
            root_available: true,
            revision_valid: true,
        };

        let changed = changed_paths(&view, &base).unwrap();
        for expected in [old, renamed, with_newline, untracked] {
            assert!(changed.iter().any(|path| path == expected), "{changed:?}");
        }
        assert!(changed.windows(2).all(|pair| pair[0] < pair[1]));

        let (dirty, staged) = project_state(Some(&view)).unwrap();
        for expected in [old, renamed, with_newline, untracked] {
            assert!(dirty.iter().any(|path| path == expected), "{dirty:?}");
        }
        assert_eq!(staged, vec![old.to_string(), renamed.to_string()]);
        fs::remove_dir_all(root).unwrap();
    }

    // APFS rejects invalid UTF-8 names before Git can observe them. The
    // parser-level rejection test remains cross-platform in git_paths.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn invalid_utf8_git_paths_are_explicit_audit_diagnostics() {
        use std::os::unix::ffi::OsStringExt;

        let root = temp_dir("invalid-utf8-git-path");
        let project = root.join("project");
        let wiki_dir = root.join("wiki");
        fs::create_dir_all(&project).unwrap();
        git(&project, &["init", "-q"]);
        fs::write(project.join("tracked.rs"), "const OK: bool = true;\n").unwrap();
        git(&project, &["add", "tracked.rs"]);
        git(&project, &["commit", "-qm", "initial"]);
        let base = git(&project, &["rev-parse", "HEAD"]);
        let invalid = std::ffi::OsString::from_vec(b"invalid-\xff.rs".to_vec());
        fs::write(project.join(invalid), "const BAD: bool = true;\n").unwrap();

        let mut w = wiki(wiki_dir, &project);
        w.config.last_ingest_commit = Some(base);
        let report = audit(&w, &AuditOptions::default()).unwrap();
        for code in ["ingest_compare_failed", "project_state_invalid"] {
            let diagnostic = report
                .diagnostics
                .iter()
                .find(|diagnostic| diagnostic.code == code)
                .unwrap_or_else(|| panic!("missing {code}: {:#?}", report.diagnostics));
            assert!(
                diagnostic.message.contains("not valid UTF-8"),
                "{diagnostic:?}"
            );
            assert_eq!(diagnostic.severity, Severity::Error);
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_line_provenance_participates_in_staleness_without_hiding_mismatch() {
        let root = temp_dir("file-line-stale");
        let project = root.join("project");
        let wiki_dir = root.join("wiki");
        fs::create_dir_all(project.join("src")).unwrap();
        git(&project, &["init", "-q"]);
        fs::write(project.join("src/live.rs"), "const VERSION: u8 = 1;\n").unwrap();
        git(&project, &["add", "src/live.rs"]);
        git(&project, &["commit", "-qm", "initial"]);
        let base = git(&project, &["rev-parse", "HEAD"]);
        fs::write(project.join("src/live.rs"), "const VERSION: u8 = 2;\n").unwrap();
        git(&project, &["commit", "-qam", "change"]);
        let revision = git(&project, &["rev-parse", "HEAD"]);

        let mut w = wiki(wiki_dir, &project);
        w.config.last_ingest_commit = Some(base);
        save_page(
            &w,
            "code/live",
            "**Live module.** Documents [[index]].\n\nFile: `src/live.rs`",
            &[],
        );
        save_page(&w, "index", "**Index.** See [[code/live]].", &[]);

        let report = audit(
            &w,
            &AuditOptions {
                project_root: Some(project),
                project_revision: Some(revision),
            },
        )
        .unwrap();
        assert!(has_code(&report, "stale_page", Some("code/live")));
        assert!(!has_code(&report, "uncovered_changes", None));
        assert!(has_code(
            &report,
            "source_metadata_mismatch",
            Some("code/live")
        ));
        for code in ["source_metadata_missing", "source_metadata_mismatch"] {
            assert!(report.diagnostics.iter().any(|diagnostic| {
                diagnostic.code == code
                    && diagnostic.page.as_deref() == Some("code/live")
                    && diagnostic.severity == Severity::Error
            }));
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn malformed_sources_cannot_escape_the_project() {
        let root = temp_dir("traversal");
        let project = root.join("project");
        fs::create_dir_all(&project).unwrap();
        let w = wiki(root.join("wiki"), &project);
        // Simulate a manually corrupted legacy page. Normal Wookie writes now
        // reject this source at their metadata boundary.
        let unsafe_page = Page {
            id: "code/unsafe".into(),
            fm: Frontmatter {
                title: "code/unsafe".into(),
                description: "Unsafe source".into(),
                created: today(),
                updated: today(),
                sources: vec!["../secret".into()],
                ..Default::default()
            },
            body: "**Unsafe source.** See [[index]].".into(),
        };
        let unsafe_path = w.page_path("code/unsafe").unwrap();
        fs::create_dir_all(unsafe_path.parent().unwrap()).unwrap();
        fs::write(unsafe_path, unsafe_page.render()).unwrap();
        save_page(&w, "index", "**Index.** See [[code/unsafe]].", &[]);

        let report = audit(&w, &AuditOptions::default()).unwrap();
        assert!(has_code(&report, "invalid_source", Some("code/unsafe")));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn protocol_health_reports_unknown_sections_and_catalog_failures() {
        let root = temp_dir("protocol-health");
        let project = root.join("project");
        fs::create_dir_all(&project).unwrap();
        let w = wiki(root.join("wiki"), &project);
        save_page(&w, "index", "**Index.** Project knowledge.", &[]);
        let protocol_path = w.dir.join("protocols/review.md");
        fs::create_dir_all(protocol_path.parent().unwrap()).unwrap();
        fs::write(
            &protocol_path,
            "+++\nsection = \"not-configured\"\n+++\n**{{title}}.** Review {{id}}.\n",
        )
        .unwrap();

        let unknown_section = audit(&w, &AuditOptions::default()).unwrap();
        let diagnostic = unknown_section
            .diagnostics
            .iter()
            .find(|diagnostic| diagnostic.code == code::PROTOCOL_SECTION_INVALID)
            .expect("missing unknown protocol-section diagnostic");
        assert_eq!(diagnostic.severity, Severity::Error);
        assert_eq!(diagnostic.source.as_deref(), Some("protocols/review.md"));

        fs::write(
            &protocol_path,
            "**Broken protocol.** Unknown {{placeholder}}.\n",
        )
        .unwrap();
        let invalid_catalog = audit(&w, &AuditOptions::default()).unwrap();
        let diagnostic = invalid_catalog
            .diagnostics
            .iter()
            .find(|diagnostic| diagnostic.code == code::PROTOCOL_CATALOG_INVALID)
            .expect("missing invalid protocol-catalog diagnostic");
        assert_eq!(diagnostic.severity, Severity::Error);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn finding_health_enforces_tags_sources_and_required_sections() {
        let root = temp_dir("finding-health");
        let project = root.join("project");
        fs::create_dir_all(project.join("src")).unwrap();
        fs::write(project.join("src/lib.rs"), "fn live() {}\n").unwrap();
        let w = wiki(root.join("wiki"), &project);

        let save_finding = |id: &str, tags: &[&str], sources: &[&str], body: &str| {
            let mut page = Page {
                id: id.into(),
                fm: Frontmatter {
                    title: id.into(),
                    description: format!("Description for {id}"),
                    created: today(),
                    updated: today(),
                    tags: tags.iter().map(|tag| tag.to_string()).collect(),
                    sources: sources.iter().map(|source| source.to_string()).collect(),
                    ..Default::default()
                },
                body: body.into(),
            };
            w.save_page_raw(&mut page, false).unwrap();
        };
        save_finding(
            "findings/valid",
            &["finding", "status/open", "severity/high"],
            &["src/lib.rs"],
            "**Valid finding.** Complete contract.\n\n## Owner\n\nA\n\n## Remediation\n\nB\n\n## Verification evidence\n\nC",
        );
        save_finding(
            "findings/missing",
            &["finding"],
            &[],
            "**Missing finding fields.** Incomplete contract.",
        );
        save_finding(
            "findings/ambiguous",
            &[
                "finding",
                "status/open",
                "status/fixed",
                "severity/high",
                "severity/unknown",
            ],
            &[],
            "**Ambiguous finding fields.** Incomplete contract.",
        );
        save_page(
            &w,
            "index",
            "**Index.** See [[findings/valid]], [[findings/missing]], and [[findings/ambiguous]].",
            &[],
        );

        let report = audit(&w, &AuditOptions::default()).unwrap();
        for id in ["findings/missing", "findings/ambiguous"] {
            for diagnostic_code in [
                code::FINDING_STATUS_INVALID,
                code::FINDING_SEVERITY_INVALID,
                code::FINDING_SOURCES_MISSING,
                code::FINDING_SECTIONS_MISSING,
            ] {
                assert!(
                    has_code(&report, diagnostic_code, Some(id)),
                    "missing {diagnostic_code} for {id}: {:#?}",
                    report.diagnostics
                );
            }
        }
        for diagnostic_code in [
            code::FINDING_STATUS_INVALID,
            code::FINDING_SEVERITY_INVALID,
            code::FINDING_SOURCES_MISSING,
            code::FINDING_SECTIONS_MISSING,
        ] {
            assert!(!has_code(&report, diagnostic_code, Some("findings/valid")));
        }
        for diagnostic_code in [code::FINDING_STATUS_INVALID, code::FINDING_SEVERITY_INVALID] {
            assert!(report.diagnostics.iter().any(|diagnostic| {
                diagnostic.code == diagnostic_code && diagnostic.severity == Severity::Error
            }));
        }
        for diagnostic_code in [
            code::FINDING_SOURCES_MISSING,
            code::FINDING_SECTIONS_MISSING,
        ] {
            assert!(report.diagnostics.iter().any(|diagnostic| {
                diagnostic.code == diagnostic_code && diagnostic.severity == Severity::Warning
            }));
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn control_characters_in_legacy_frontmatter_are_reported_per_page() {
        let root = temp_dir("metadata-controls");
        let project = root.join("project");
        fs::create_dir_all(&project).unwrap();
        let w = wiki(root.join("wiki"), &project);
        for (id, field) in [
            ("guides/title-control", "title: Bad\tTitle"),
            ("guides/tag-control", "title: Tag\ntags: [\"bad\ttag\"]"),
            (
                "guides/status-control",
                "title: Status\nstatus: \"open\tbad\"",
            ),
        ] {
            let path = w.page_path(id).unwrap();
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(
                path,
                format!(
                    "---\n{field}\ndescription: \"Legacy metadata\"\ncreated: 2026-01-01\nupdated: 2026-01-01\n---\n\n**Legacy page.** Metadata was edited directly.\n"
                ),
            )
            .unwrap();
        }

        let report = audit(&w, &AuditOptions::default()).unwrap();
        for id in [
            "guides/title-control",
            "guides/tag-control",
            "guides/status-control",
        ] {
            assert!(has_code(&report, code::INVALID_PAGE, Some(id)), "{id}");
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unresolved_ingest_recovery_is_a_health_error() {
        let root = temp_dir("ingest-recovery");
        let project = root.join("project");
        fs::create_dir_all(&project).unwrap();
        let w = wiki(root.join("wiki"), &project);
        save_page(&w, "index", "**Index.** Project knowledge.", &[]);
        fs::write(w.dir.join(".ingest-reconciliation-recovery.json"), "{}").unwrap();

        let report = audit(&w, &AuditOptions::default()).unwrap();
        assert!(has_code(&report, code::INGEST_RECOVERY_REQUIRED, None));
        assert!(report.summary.errors > 0);
        assert_eq!(report.data["ingest_recovery_required"], json!(true));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn audit_remains_available_but_suppresses_revision_under_writer_lock() {
        let root = temp_dir("writer-transition");
        let project = root.join("project");
        fs::create_dir_all(&project).unwrap();
        let w = wiki(root.join("wiki"), &project);
        save_page(&w, "index", "**Index.** Project knowledge.", &[]);
        let guard = w.acquire_mutation_guard().unwrap();

        let report = audit(&w, &AuditOptions::default()).unwrap();
        assert!(has_code(&report, code::AUDIT_STATE_IN_TRANSITION, None));
        assert!(report.summary.errors > 0);
        assert_eq!(report.data["audit_state_in_transition"], json!(true));
        assert_eq!(report.snapshot.wiki.revision, None);

        drop(guard);
        fs::remove_dir_all(root).unwrap();
    }
}
