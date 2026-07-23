//! Transaction primitives for publishing a validated set of wiki changes.
//!
//! The filesystem cannot provide a truly atomic multi-file rename. Wookie's
//! contract is therefore explicit: preflight against a base revision, acquire
//! one publication lock, durably record every before/after image, apply, and
//! roll back on ordinary failure. A surviving journal makes crash recovery an
//! explicit operator action instead of guessing which state is authoritative.
//! Page bytes (including frontmatter) and portable permission metadata are
//! restored. Filesystem timestamps, ownership, ACLs, and extended attributes
//! are outside this v1 journal contract.

use crate::page::{first_sentence, humanize, rewrite_links, today, Frontmatter, Page, PinLevel};
use crate::report::{code, Diagnostic, Report, Severity, Snapshot};
use crate::snapshot;
use crate::wiki::{
    atomic_write, atomic_write_with_permissions, contained_path, create_contained_dir_all,
    validate_id, AtomicWritePermissions, Wiki,
};
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub const CHANGESET_SCHEMA: &str = "wookie.changeset/v1";
pub const PUBLISH_PLAN_SCHEMA: &str = "wookie.publish-plan/v1";
pub const JOURNAL_SCHEMA: &str = "wookie.publish-journal/v1";
pub const LOCK_SCHEMA: &str = "wookie.publish-lock/v1";

pub const PUBLISH_LOCK_PATH: &str = ".publish.lock";
pub const PUBLISH_JOURNAL_PATH: &str = ".publish-journal.json";
const MAX_JOURNAL_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CHANGESET_CHANGES: usize = 512;
const MAX_PUBLISH_PATHS: usize = 512;
const MAX_PUBLISH_PATH_ARG_BYTES: usize = 12 * 1024;
const MAX_REVISION_BYTES: usize = 512;
const MAX_MESSAGE_BYTES: usize = 16 * 1024;
const MAX_LOCK_OWNER_BYTES: u64 = 16 * 1024;
const MAX_COMMIT_PATH_OUTPUT_BYTES: u64 = 1024 * 1024;
const MAX_GIT_TREE_ENTRY_BYTES: usize = 16 * 1024;
const MAX_GIT_STDERR_BYTES: usize = 64 * 1024;
const MAX_GIT_REVISION_OUTPUT_BYTES: usize = 1024;
const MAX_GIT_STATUS_OUTPUT_BYTES: usize = 1024 * 1024;
const MAX_GIT_COMMIT_OBJECT_BYTES: usize = 1024 * 1024;

struct BoundedCommandOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_truncated: bool,
    stderr_truncated: bool,
}

fn drain_bounded(mut reader: impl std::io::Read, limit: usize) -> std::io::Result<(Vec<u8>, bool)> {
    let mut retained = Vec::with_capacity(limit.min(8 * 1024));
    let mut truncated = false;
    let mut chunk = [0_u8; 8 * 1024];
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(retained.len());
        let keep = remaining.min(read);
        retained.extend_from_slice(&chunk[..keep]);
        truncated |= keep < read;
    }
    Ok((retained, truncated))
}

/// Run a read-only verification subprocess with bounded stdout while draining
/// stderr concurrently. Stdout reads one byte beyond its contract and then
/// kills/reaps the child on overflow; stderr keeps draining but retains only a
/// bounded prefix so a verbose process cannot deadlock or exhaust memory.
fn run_bounded_command(
    command: &mut Command,
    stdout_limit: Option<usize>,
) -> Result<BoundedCommandOutput> {
    let stdout_read_limit = stdout_limit
        .map(|limit| {
            limit
                .checked_add(1)
                .context("bounded stdout limit overflows memory size")
        })
        .transpose()?;
    command.stdout(if stdout_limit.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    });
    command.stderr(Stdio::piped());
    let mut child = command.spawn().context("starting bounded subprocess")?;
    let Some(stderr) = child.stderr.take() else {
        if stdout_limit.is_some() {
            let _ = child.kill();
        }
        let _ = child.wait();
        bail!("bounded subprocess has no stderr pipe");
    };
    let stderr_handle = std::thread::spawn(move || drain_bounded(stderr, MAX_GIT_STDERR_BYTES));
    let (stdout, stdout_truncated, stdout_error) = match stdout_limit {
        Some(limit) => match child.stdout.take() {
            Some(stdout) => {
                let read_limit = stdout_read_limit.unwrap_or(limit);
                let mut retained = Vec::with_capacity(read_limit.min(8 * 1024));
                match stdout
                    .take(u64::try_from(read_limit).unwrap_or(u64::MAX))
                    .read_to_end(&mut retained)
                {
                    Ok(_) => {
                        let truncated = retained.len() > limit;
                        if truncated {
                            retained.truncate(limit);
                            let _ = child.kill();
                        }
                        (retained, truncated, None)
                    }
                    Err(error) => {
                        let _ = child.kill();
                        (
                            retained,
                            false,
                            Some(anyhow!(error).context("reading bounded subprocess stdout")),
                        )
                    }
                }
            }
            None => {
                let _ = child.kill();
                (
                    Vec::new(),
                    false,
                    Some(anyhow!("bounded subprocess has no stdout pipe")),
                )
            }
        },
        None => (Vec::new(), false, None),
    };
    // Always reap the child and join the stderr drainer, including overflow
    // and read-error paths. This avoids zombies and pipe deadlocks while
    // read-only verification commands fail fast on oversized stdout.
    let first_wait = child.wait();
    let stderr_result = stderr_handle
        .join()
        .map_err(|_| anyhow!("bounded stderr reader panicked"))?
        .context("reading bounded subprocess stderr");
    let status = match first_wait {
        Ok(status) => status,
        Err(first_error) => child.wait().with_context(|| {
            format!("waiting for bounded subprocess after initial error: {first_error}")
        })?,
    };
    let (stderr, stderr_truncated) = stderr_result?;
    if let Some(error) = stdout_error {
        return Err(error);
    }
    Ok(BoundedCommandOutput {
        status,
        stdout,
        stderr,
        stdout_truncated,
        stderr_truncated,
    })
}

fn bounded_stderr(output: &BoundedCommandOutput) -> String {
    let mut rendered = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if output.stderr_truncated {
        if !rendered.is_empty() {
            rendered.push('\n');
        }
        rendered.push_str("...[stderr truncated]");
    }
    rendered
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetadataPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clear_status: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sources: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin_level: Option<PinLevel>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clear_pin_level: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aliases: Option<Vec<String>>,
}

impl MetadataPatch {
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.description.is_none()
            && self.tags.is_none()
            && self.status.is_none()
            && !self.clear_status
            && self.sources.is_none()
            && self.pin.is_none()
            && self.pin_level.is_none()
            && !self.clear_pin_level
            && self.aliases.is_none()
    }

    fn validate(&self) -> Result<()> {
        if self.status.is_some() && self.clear_status {
            bail!("metadata cannot set and clear status at the same time");
        }
        if self.pin_level.is_some() && self.clear_pin_level {
            bail!("metadata cannot set and clear pin_level at the same time");
        }
        if let Some(sources) = &self.sources {
            for source in sources {
                let path = Path::new(source);
                if source.is_empty()
                    || path.is_absolute()
                    || source.contains('\\')
                    || path
                        .components()
                        .any(|component| !matches!(component, std::path::Component::Normal(_)))
                {
                    bail!("source path must be a clean project-relative path: '{source}'");
                }
            }
        }
        Ok(())
    }

    fn apply_to(&self, fm: &mut Frontmatter) -> Result<()> {
        self.validate()?;
        if let Some(value) = &self.title {
            fm.title = value.clone();
        }
        if let Some(value) = &self.description {
            fm.description = value.clone();
        }
        if let Some(value) = &self.tags {
            fm.tags = value.clone();
        }
        if self.clear_status {
            fm.status = None;
        } else if let Some(value) = &self.status {
            fm.status = Some(value.clone());
        }
        if let Some(value) = &self.sources {
            fm.sources = value.clone();
        }
        if let Some(value) = self.pin {
            fm.pin = value;
        }
        if self.clear_pin_level {
            fm.pin_level = None;
        } else if let Some(value) = self.pin_level {
            fm.pin_level = Some(value);
        }
        if let Some(value) = &self.aliases {
            fm.aliases = value.clone();
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase", deny_unknown_fields)]
pub enum Change {
    Create {
        id: String,
        #[serde(default)]
        body: String,
        #[serde(default)]
        metadata: MetadataPatch,
    },
    Update {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body: Option<String>,
        #[serde(default)]
        metadata: MetadataPatch,
    },
    Delete {
        id: String,
    },
    Move {
        from: String,
        to: String,
        #[serde(default = "default_true")]
        rewrite_links: bool,
    },
}

const fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeSet {
    pub schema: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default)]
    pub changes: Vec<Change>,
}

impl ChangeSet {
    #[cfg(test)]
    pub fn new(base_revision: Option<String>) -> Self {
        Self {
            schema: CHANGESET_SCHEMA.to_string(),
            base_revision,
            message: None,
            changes: Vec::new(),
        }
    }

    #[cfg(test)]
    pub fn push(&mut self, change: Change) {
        self.changes.push(change);
    }

    /// Parse JSON when the first non-whitespace byte is `{`, otherwise TOML.
    /// The schema is validated before returning.
    pub fn parse(raw: &str) -> Result<Self> {
        let change_set: Self = if raw.trim_start().starts_with('{') {
            serde_json::from_str(raw).context("parsing JSON change set")?
        } else {
            toml::from_str(raw).context("parsing TOML change set")?
        };
        change_set.validate_schema()?;
        Ok(change_set)
    }

    pub fn validate_schema(&self) -> Result<()> {
        if self.schema != CHANGESET_SCHEMA {
            bail!(
                "unsupported change-set schema '{}' (expected '{CHANGESET_SCHEMA}')",
                self.schema
            );
        }
        if self.changes.len() > MAX_CHANGESET_CHANGES {
            bail!("change set contains more than {MAX_CHANGESET_CHANGES} changes");
        }
        if let Some(revision) = &self.base_revision {
            if revision.is_empty()
                || revision.len() > MAX_REVISION_BYTES
                || revision.chars().any(char::is_control)
            {
                bail!("base_revision must be a non-empty, single-line revision identifier");
            }
        }
        if let Some(message) = &self.message {
            if !publish_message_is_valid(message) {
                bail!("publish message is invalid or exceeds {MAX_MESSAGE_BYTES} bytes");
            }
        }
        Ok(())
    }
}

fn publish_message_is_valid(message: &str) -> bool {
    message.len() <= MAX_MESSAGE_BYTES
        && message.chars().all(|character| {
            matches!(character, '\n' | '\t')
                || (!character.is_control()
                    && !matches!(
                        character,
                        '\u{061c}'
                            | '\u{200e}'
                            | '\u{200f}'
                            | '\u{202a}'..='\u{202e}'
                            | '\u{2066}'..='\u{2069}'
                    ))
        })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OperationKind {
    Create,
    Update,
    Delete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RequestedChangeKind {
    Create,
    Update,
    Delete,
    Move,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestedChange {
    pub kind: RequestedChangeKind,
    pub page: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destination: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedOperation {
    pub kind: OperationKind,
    pub page: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_fingerprint: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishPlan {
    pub schema: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_revision: Option<String>,
    /// Deterministic fingerprint of the complete raw page catalog checked by
    /// preflight. It catches dirty, uncommitted changes that Git HEAD cannot.
    pub observed_content_hash: String,
    pub requested: Vec<RequestedChange>,
    pub operations: Vec<PlannedOperation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PageDiff {
    pub page: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
}

impl PublishPlan {
    /// Wiki-relative paths suitable for a path-scoped history commit.
    pub fn relative_paths(&self) -> Vec<String> {
        self.operations
            .iter()
            .map(|operation| format!("pages/{}.md", operation.page))
            .collect()
    }
}

/// In-memory view presented to doctor/critique/source validators. It contains
/// the complete proposed page set, never paths outside `pages/`.
#[derive(Debug, Clone)]
pub struct PublishOverlay {
    pages: BTreeMap<String, Page>,
}

impl PublishOverlay {
    #[cfg(test)]
    pub fn page(&self, id: &str) -> Option<&Page> {
        self.pages.get(id)
    }

    #[cfg(test)]
    pub fn contains(&self, id: &str) -> bool {
        self.pages.contains_key(id)
    }

    pub fn page_ids(&self) -> impl Iterator<Item = &str> {
        self.pages.keys().map(String::as_str)
    }

    pub fn pages(&self) -> impl Iterator<Item = &Page> {
        self.pages.values()
    }
}

#[derive(Debug, Clone)]
pub struct Preflight {
    pub plan: PublishPlan,
    pub report: Report,
    pub overlay: PublishOverlay,
    deltas: Vec<FileDelta>,
    catalog_before: BTreeMap<String, FileSnapshot>,
    catalog_before_state: CatalogState,
    catalog_after_state: CatalogState,
    config_before: FileSnapshot,
    unlock_controls_before: BTreeMap<String, FileSnapshot>,
    effective_policy_sha256: String,
    change_set_sha256: String,
}

impl Preflight {
    pub fn add_diagnostics(&mut self, diagnostics: impl IntoIterator<Item = Diagnostic>) {
        self.report.extend(diagnostics);
    }

    pub fn is_publishable(&self) -> bool {
        !self.report.has_errors()
    }

    /// Hash the deterministic, body-free plan representation used by rule
    /// review receipts. Page bodies are cryptographically represented by the
    /// plan's SHA-256 fingerprints and complete catalog hash.
    pub fn plan_sha256(&self) -> Result<String> {
        let encoded = serde_json::to_vec(&self.plan)?;
        Ok(framed_sha256(
            b"wookie.publish-plan-receipt/v1",
            &[&encoded],
        ))
    }

    /// Hash of the exact configuration bytes and portable metadata that
    /// governed this preflight. Rule receipts bind it so an uncommitted policy
    /// edit cannot reuse an earlier review.
    pub fn config_sha256(&self) -> Result<String> {
        let encoded = serde_json::to_vec(&self.config_before)?;
        Ok(framed_sha256(
            b"wookie.publish-config-receipt/v1",
            &[&encoded],
        ))
    }

    pub fn effective_policy_sha256(&self) -> &str {
        &self.effective_policy_sha256
    }

    /// Stable token for a checked manifest, complete plan, raw catalog,
    /// configuration, and effective publish policy. It can be supplied to a
    /// later apply as an optional compare-and-publish guard.
    pub fn review_token(&self) -> Result<String> {
        let plan = self.plan_sha256()?;
        let config = self.config_sha256()?;
        Ok(framed_sha256(
            b"wookie.publish-review-token/v1",
            &[
                self.change_set_sha256.as_bytes(),
                plan.as_bytes(),
                config.as_bytes(),
                self.effective_policy_sha256.as_bytes(),
            ],
        ))
    }

    /// Recheck the exact state that produced this preflight without writing.
    /// Callers use this while holding the shared mutation lock before they
    /// persist a review receipt.
    pub(crate) fn verify_current_state(
        &self,
        wiki: &Wiki,
        supplied_revision: Option<&str>,
    ) -> Result<()> {
        let current_revision = current_wiki_revision(wiki)?;
        let current_or_supplied = current_revision.as_deref().or(supplied_revision);
        if self.plan.observed_revision.is_some()
            && self.plan.observed_revision.as_deref() != current_or_supplied
        {
            bail!("wiki revision changed during review; review the proposal again");
        }
        if snapshot_catalog(wiki)? != self.catalog_before {
            bail!("wiki page catalog changed during review; review the proposal again");
        }
        let config_path = wiki.contained_path(Path::new("wookie.toml"))?;
        if snapshot_file(&config_path)? != self.config_before {
            bail!("wookie.toml changed during review; review the proposal again");
        }
        if snapshot_unlock_controls_for_paths(wiki, self.unlock_controls_before.keys())?
            != self.unlock_controls_before
        {
            bail!("section lock controls changed during review; review the proposal again");
        }
        if current_effective_publish_policy_sha256(wiki)? != self.effective_policy_sha256 {
            bail!("effective publish policy changed during review; review the proposal again");
        }
        Ok(())
    }

    /// Exact before/after page images. Kept out of `PublishPlan` so compact
    /// JSON plans do not accidentally duplicate every page body.
    pub fn diffs(&self) -> Vec<PageDiff> {
        self.diffs_limited(self.deltas.len())
    }

    /// Number of changed page images available to an output renderer. This
    /// lets bounded previews report omissions without cloning every full page.
    pub fn diff_count(&self) -> usize {
        self.deltas.len()
    }

    /// Clone at most `limit` exact page images. Normal previews should keep
    /// this limit small and derive compact excerpts; `diffs` is the explicit
    /// exhaustive path used by `--full-diff`.
    pub fn diffs_limited(&self, limit: usize) -> Vec<PageDiff> {
        self.deltas
            .iter()
            .take(limit)
            .map(|delta| PageDiff {
                page: delta
                    .relative_path
                    .strip_prefix("pages/")
                    .and_then(|path| path.strip_suffix(".md"))
                    .unwrap_or(&delta.relative_path)
                    .to_string(),
                before: delta.before.content.clone(),
                after: delta.after.content.clone(),
            })
            .collect()
    }

    /// Concise operator plan. `include_diff` prints exact replacement images;
    /// callers should keep it opt-in for large pages.
    pub fn render_human(&self, include_diff: bool) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "Publish plan: {} operation(s), {} error(s), {} warning(s)",
            self.plan.operations.len(),
            self.report.summary.errors,
            self.report.summary.warnings
        );
        for operation in &self.plan.operations {
            let _ = writeln!(
                out,
                "{:?} {} — {}",
                operation.kind, operation.page, operation.reason
            );
        }
        if !self.report.diagnostics.is_empty() {
            out.push_str("Diagnostics:\n");
            for diagnostic in &self.report.diagnostics {
                let location = diagnostic
                    .page
                    .as_deref()
                    .or(diagnostic.source.as_deref())
                    .map(|value| format!(" [{}]", crate::report::terminal_safe(value)))
                    .unwrap_or_default();
                let _ = writeln!(
                    out,
                    "{} {}{}: {}",
                    diagnostic.severity.as_str().to_uppercase(),
                    diagnostic.code,
                    location,
                    crate::report::terminal_safe(&diagnostic.message)
                );
                if let Some(suggestion) = &diagnostic.suggestion {
                    let _ = writeln!(
                        out,
                        "  Suggestion: {}",
                        crate::report::terminal_safe(suggestion)
                    );
                }
            }
        }
        if include_diff {
            for diff in self.diffs() {
                let _ = writeln!(out, "--- pages/{}.md", diff.page);
                let _ = writeln!(out, "+++ pages/{}.md", diff.page);
                if let Some(before) = diff.before {
                    for line in before.lines() {
                        let _ = writeln!(out, "-{line}");
                    }
                }
                if let Some(after) = diff.after {
                    for line in after.lines() {
                        let _ = writeln!(out, "+{line}");
                    }
                }
            }
        }
        out.trim_end().to_string()
    }
}

#[derive(Debug, Clone)]
struct FileDelta {
    relative_path: String,
    before: FileSnapshot,
    after: FileSnapshot,
}

/// Build the proposed page graph without writing. Structural conflicts are
/// reported as diagnostics so `publish --check` can return a complete audit;
/// unreadable storage remains an operational error.
pub fn preflight(
    wiki: &Wiki,
    change_set: &ChangeSet,
    actual_revision: Option<&str>,
    mut snapshot: Snapshot,
) -> Result<Preflight> {
    change_set.validate_schema()?;
    let encoded_change_set = serde_json::to_vec(change_set)?;
    let change_set_sha256 = framed_sha256(
        b"wookie.publish-changeset/v1",
        &[encoded_change_set.as_slice()],
    );
    // The loaded `Wiki` derives lock policy, history behavior, and other
    // publication semantics from this file. Snapshot it before examining the
    // catalog so a concurrent config edit invalidates the complete plan.
    let config_before = snapshot_file(&wiki.contained_path(Path::new("wookie.toml"))?)?;
    let effective_policy_sha256 = effective_publish_policy_sha256(wiki)?;
    let unlock_controls_before = snapshot_unlock_controls(wiki)?;
    if snapshot.wiki.revision.is_none() {
        snapshot.wiki.revision = actual_revision.map(str::to_owned);
    }
    let mut report = Report::new("publish-check", snapshot);
    verify_base_revision_diagnostic(
        change_set.base_revision.as_deref(),
        actual_revision,
        &mut report,
    );
    if wiki.publish.require_base_revision && change_set.base_revision.is_none() {
        report.push(
            Diagnostic::new(
                code::PUBLISH_PLAN_INVALID,
                Severity::Error,
                "publish policy requires a full base revision",
            )
            .suggestion("set base_revision to the current full wiki revision ID"),
        );
    }

    let mut pages = BTreeMap::new();
    let mut originals = BTreeMap::new();
    let mut proposed_metadata = BTreeMap::new();
    for (id, path) in wiki.page_files_strict()? {
        let before = snapshot_file(&path)?;
        let content = before
            .content
            .as_deref()
            .ok_or_else(|| anyhow!("page '{}' disappeared during preflight", id))?;
        pages.insert(id.clone(), Page::parse(&id, content));
        proposed_metadata.insert(id.clone(), before.metadata.clone());
        originals.insert(id, before);
    }
    let catalog_before_state = catalog_state(&originals)?;
    let observed_content_hash = catalog_before_state.raw_content_sha256.clone();
    // Publish receipts and the publish report must name the same exact raw
    // catalog. Replace any caller-provided parsed/rendered approximation.
    report.snapshot.wiki.content_hash = Some(observed_content_hash.clone());

    let mut changed = BTreeSet::new();
    let mut reasons = BTreeMap::new();
    let mut directly_touched = BTreeSet::new();
    let mut requested = Vec::new();

    if change_set.changes.is_empty() {
        report.push(Diagnostic::new(
            code::PUBLISH_PLAN_INVALID,
            Severity::Error,
            "change set contains no changes",
        ));
    }

    for change in &change_set.changes {
        let direct_ids: Vec<&str> = match change {
            Change::Create { id, .. } | Change::Update { id, .. } | Change::Delete { id } => {
                vec![id]
            }
            Change::Move { from, to, .. } => vec![from, to],
        };
        let mut duplicate = false;
        for id in &direct_ids {
            if let Err(error) = validate_id(id) {
                report.push(
                    Diagnostic::new(
                        code::PUBLISH_PLAN_INVALID,
                        Severity::Error,
                        error.to_string(),
                    )
                    .page(*id),
                );
                duplicate = true;
            }
        }
        if duplicate {
            continue;
        }
        for id in &direct_ids {
            if directly_touched.contains(*id) {
                report.push(
                    Diagnostic::new(
                        code::PUBLISH_PLAN_INVALID,
                        Severity::Error,
                        format!("page '{id}' is directly changed more than once"),
                    )
                    .page(*id)
                    .suggestion("combine edits into one change"),
                );
                duplicate = true;
            }
        }
        if duplicate {
            continue;
        }
        directly_touched.extend(direct_ids.iter().map(|id| (*id).to_string()));

        match change {
            Change::Create { id, body, metadata } => {
                requested.push(requested_change(RequestedChangeKind::Create, id, None));
                check_writable(wiki, id, &mut report);
                if pages.contains_key(id) {
                    report.push(plan_error(id, format!("page '{id}' already exists")));
                    continue;
                }
                if let Err(error) = metadata.validate() {
                    report.push(plan_error(id, error.to_string()));
                    continue;
                }
                let mut page = new_page(id, body);
                metadata.apply_to(&mut page.fm)?;
                pages.insert(id.clone(), page);
                proposed_metadata.insert(id.clone(), Some(default_file_metadata()));
                changed.insert(id.clone());
                reasons.insert(id.clone(), "create page".to_string());
            }
            Change::Update { id, body, metadata } => {
                requested.push(requested_change(RequestedChangeKind::Update, id, None));
                check_writable(wiki, id, &mut report);
                if body.is_none() && metadata.is_empty() {
                    report.push(plan_error(id, format!("update for '{id}' has no changes")));
                    continue;
                }
                if let Err(error) = metadata.validate() {
                    report.push(plan_error(id, error.to_string()));
                    continue;
                }
                let Some(page) = pages.get_mut(id) else {
                    report.push(plan_error(id, format!("page '{id}' does not exist")));
                    continue;
                };
                let before = page.render();
                if let Some(body) = body {
                    page.body = body.clone();
                }
                metadata.apply_to(&mut page.fm)?;
                // `updated` describes an effective change; it must not turn an
                // otherwise identical update request into a synthetic delta.
                if page.render() == before {
                    continue;
                }
                page.fm.updated = today();
                changed.insert(id.clone());
                reasons.insert(id.clone(), "update page".to_string());
            }
            Change::Delete { id } => {
                requested.push(requested_change(RequestedChangeKind::Delete, id, None));
                check_writable(wiki, id, &mut report);
                if pages.remove(id).is_none() {
                    report.push(plan_error(id, format!("page '{id}' does not exist")));
                    continue;
                }
                proposed_metadata.remove(id);
                changed.insert(id.clone());
                reasons.insert(id.clone(), "delete page".to_string());
            }
            Change::Move {
                from,
                to,
                rewrite_links: should_rewrite,
            } => {
                requested.push(requested_change(RequestedChangeKind::Move, from, Some(to)));
                check_writable(wiki, from, &mut report);
                check_writable(wiki, to, &mut report);
                if pages.contains_key(to) {
                    report.push(plan_error(to, format!("page '{to}' already exists")));
                    continue;
                }
                let Some(mut moved) = pages.remove(from) else {
                    report.push(plan_error(from, format!("page '{from}' does not exist")));
                    continue;
                };
                let moved_metadata = proposed_metadata.remove(from).unwrap_or(None);
                moved.id = to.clone();
                moved.fm.updated = today();
                if *should_rewrite {
                    moved.body = rewrite_links(&moved.body, from, to).0;
                }
                pages.insert(to.clone(), moved);
                proposed_metadata.insert(to.clone(), moved_metadata);
                changed.insert(from.clone());
                changed.insert(to.clone());
                reasons.insert(from.clone(), format!("move page to '{to}'"));
                reasons.insert(to.clone(), format!("move page from '{from}'"));

                if *should_rewrite {
                    for (id, page) in &mut pages {
                        if id == to {
                            continue;
                        }
                        let (rewritten, did_rewrite) = rewrite_links(&page.body, from, to);
                        if did_rewrite {
                            check_writable(wiki, id, &mut report);
                            page.body = rewritten;
                            page.fm.updated = today();
                            changed.insert(id.clone());
                            reasons.insert(
                                id.clone(),
                                format!("rewrite backlink for move '{from}' -> '{to}'"),
                            );
                        }
                    }
                }
            }
        }
    }

    // A publication validates the complete resulting catalog, not only the
    // metadata fields named by its manifest. This catches malformed existing
    // metadata before it can be preserved or rewritten transactionally.
    for (id, page) in &pages {
        if let Err(error) = page.validate_frontmatter() {
            report.push(plan_error(id, error.to_string()));
        }
    }

    let mut deltas = Vec::new();
    let mut operations = Vec::new();
    for id in changed {
        let before = originals.get(&id).cloned().unwrap_or_default();
        let after = match pages.get(&id) {
            Some(page) => {
                let rendered = page.render();
                if rendered.len() as u64 > snapshot::MAX_SNAPSHOT_PAGE_BYTES {
                    report.push(plan_error(
                        &id,
                        format!(
                            "rendered page is {} bytes, exceeding the {}-byte page limit",
                            rendered.len(),
                            snapshot::MAX_SNAPSHOT_PAGE_BYTES
                        ),
                    ));
                    // Never place an oversized image in the deterministic
                    // plan, durable journal, or write set. The requested
                    // change remains visible in the report and blocks apply.
                    continue;
                }
                FileSnapshot {
                    content: Some(rendered),
                    metadata: proposed_metadata.get(&id).cloned().unwrap_or(None),
                }
            }
            None => FileSnapshot::default(),
        };
        if before.content == after.content {
            continue;
        }
        let kind = match (before.content.is_some(), after.content.is_some()) {
            (false, true) => OperationKind::Create,
            (true, true) => OperationKind::Update,
            (true, false) => OperationKind::Delete,
            (false, false) => continue,
        };
        operations.push(PlannedOperation {
            kind,
            page: id.clone(),
            before_fingerprint: before.content.as_deref().map(fingerprint),
            after_fingerprint: after.content.as_deref().map(fingerprint),
            reason: reasons
                .get(&id)
                .cloned()
                .unwrap_or_else(|| "publish change".to_string()),
        });
        deltas.push(FileDelta {
            relative_path: format!("pages/{id}.md"),
            before,
            after,
        });
    }

    // Publish destinations before updating backlinks, and remove old names
    // last. This minimizes broken-link windows for readers that do not yet
    // participate in the publication lock.
    operations.sort_by_key(|operation| (operation_rank(operation.kind), operation.page.clone()));
    deltas.sort_by_key(|delta| {
        let rank = match (
            delta.before.content.is_some(),
            delta.after.content.is_some(),
        ) {
            (false, true) => 0,
            (true, true) => 1,
            (true, false) => 2,
            (false, false) => 3,
        };
        (rank, delta.relative_path.clone())
    });

    if operations.is_empty() {
        report.push(
            Diagnostic::new(
                code::PUBLISH_PLAN_INVALID,
                Severity::Error,
                "change set produces no effective page operations",
            )
            .suggestion("remove no-op changes or update the manifest with an actual page change"),
        );
    }
    let path_bytes = operations
        .iter()
        .map(|operation| "pages/".len() + operation.page.len() + ".md".len() + 1)
        .sum::<usize>();
    if operations.len() > MAX_PUBLISH_PATHS || path_bytes > MAX_PUBLISH_PATH_ARG_BYTES {
        report.push(
            Diagnostic::new(
                code::PUBLISH_PLAN_INVALID,
                Severity::Error,
                format!(
                    "publish plan exceeds path bounds ({} operations, {} path bytes; maximum {MAX_PUBLISH_PATHS} and {MAX_PUBLISH_PATH_ARG_BYTES})",
                    operations.len(), path_bytes
                ),
            )
            .suggestion("split the manifest into smaller reviewed publications"),
        );
    }

    report.insert_data("requested_changes", json!(requested.len()));
    report.insert_data("planned_operations", json!(operations.len()));
    let mut catalog_after = originals.clone();
    for delta in &deltas {
        let id = delta
            .relative_path
            .strip_prefix("pages/")
            .and_then(|path| path.strip_suffix(".md"))
            .context("publish delta path is outside the page catalog")?;
        if delta.after.content.is_some() {
            catalog_after.insert(id.to_string(), delta.after.clone());
        } else {
            catalog_after.remove(id);
        }
    }
    let catalog_after_state = catalog_state(&catalog_after)?;
    let plan = PublishPlan {
        schema: PUBLISH_PLAN_SCHEMA.to_string(),
        base_revision: change_set.base_revision.clone(),
        observed_revision: actual_revision.map(str::to_owned),
        observed_content_hash,
        requested,
        operations,
    };
    Ok(Preflight {
        plan,
        report,
        overlay: PublishOverlay { pages },
        deltas,
        catalog_before: originals,
        catalog_before_state,
        catalog_after_state,
        config_before,
        unlock_controls_before,
        effective_policy_sha256,
        change_set_sha256,
    })
}

fn requested_change(
    kind: RequestedChangeKind,
    page: &str,
    destination: Option<&str>,
) -> RequestedChange {
    RequestedChange {
        kind,
        page: page.to_string(),
        destination: destination.map(str::to_owned),
    }
}

fn operation_rank(kind: OperationKind) -> u8 {
    match kind {
        OperationKind::Create => 0,
        OperationKind::Update => 1,
        OperationKind::Delete => 2,
    }
}

fn plan_error(id: &str, message: String) -> Diagnostic {
    Diagnostic::new(code::PUBLISH_PLAN_INVALID, Severity::Error, message).page(id)
}

fn check_writable(wiki: &Wiki, id: &str, report: &mut Report) {
    if let Err(error) = wiki.assert_writable(id) {
        report.push(
            Diagnostic::new(code::RULE_LOCKED, Severity::Error, error.to_string())
                .page(id)
                .suggestion("approve the rule change through the rules workflow"),
        );
    }
}

fn verify_base_revision_diagnostic(
    expected: Option<&str>,
    actual: Option<&str>,
    report: &mut Report,
) {
    if let Some(expected) = expected {
        if actual != Some(expected) {
            report.push(
                Diagnostic::new(
                    code::PUBLISH_CONFLICT,
                    Severity::Error,
                    format!(
                        "base revision is '{}', but the observed revision is '{}'",
                        expected,
                        actual.unwrap_or("unresolved")
                    ),
                )
                .suggestion("regenerate the publish plan against the current full revision ID"),
            );
        }
    }
}

fn verify_base_revision(expected: Option<&str>, actual: Option<&str>) -> Result<()> {
    if let Some(expected) = expected {
        if actual != Some(expected) {
            bail!(
                "publish base revision '{}' does not match current revision '{}'",
                expected,
                actual.unwrap_or("unresolved")
            );
        }
    }
    Ok(())
}

fn current_wiki_revision(wiki: &Wiki) -> Result<Option<String>> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(&wiki.dir)
        .args(["rev-parse", "--verify", "HEAD"]);
    let output = run_bounded_command(&mut command, Some(MAX_GIT_REVISION_OUTPUT_BYTES))
        .with_context(|| format!("resolving wiki revision in {}", wiki.dir.display()))?;
    if output.stdout_truncated {
        bail!("wiki revision output exceeds the safe byte limit");
    }
    if !output.status.success() {
        if wiki.dir.join(".git").exists() {
            bail!(
                "could not resolve wiki revision after acquiring the publish lock: {}",
                bounded_stderr(&output)
            );
        }
        return Ok(None);
    }
    let revision = String::from_utf8(output.stdout).context("wiki revision is not UTF-8")?;
    let revision = revision.trim();
    if revision.is_empty() {
        Ok(None)
    } else {
        Ok(Some(revision.to_string()))
    }
}

fn new_page(id: &str, body: &str) -> Page {
    let title = humanize(id);
    let summary = Page::parse(id, body).summary();
    Page {
        id: id.to_string(),
        fm: Frontmatter {
            title: title.clone(),
            description: first_sentence(&summary),
            tags: Vec::new(),
            created: today(),
            updated: today(),
            status: None,
            sources: Vec::new(),
            pin: false,
            pin_level: None,
            aliases: vec![title],
            extra: Vec::new(),
        },
        body: body.to_string(),
    }
}

fn fingerprint(content: &str) -> String {
    framed_sha256(b"wookie.page-content/v1", &[content.as_bytes()])
}

fn catalog_state(pages: &BTreeMap<String, FileSnapshot>) -> Result<CatalogState> {
    let mut raw = Vec::with_capacity(pages.len());
    let mut states = BTreeMap::new();
    for (id, page) in pages {
        let content = page
            .content
            .as_deref()
            .ok_or_else(|| anyhow!("catalog page '{id}' has no content snapshot"))?;
        let metadata = page
            .metadata
            .clone()
            .ok_or_else(|| anyhow!("catalog page '{id}' has no metadata snapshot"))?;
        raw.push((id.as_str(), content.as_bytes()));
        states.insert(
            id.clone(),
            CatalogPageState {
                raw_sha256: snapshot::raw_page_sha256(content.as_bytes()),
                metadata,
            },
        );
    }
    Ok(CatalogState {
        raw_content_sha256: snapshot::catalog_content_hash_from_raw(raw)?,
        pages: states,
    })
}

fn hash_field(hash: &mut Sha256, bytes: &[u8]) {
    hash.update(u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_be_bytes());
    hash.update(bytes);
}

fn framed_sha256(domain: &[u8], fields: &[&[u8]]) -> String {
    let mut hash = Sha256::new();
    hash_field(&mut hash, domain);
    for field in fields {
        hash_field(&mut hash, field);
    }
    format!("sha256:{:x}", hash.finalize())
}

fn effective_publish_policy_sha256(wiki: &Wiki) -> Result<String> {
    let policy = json!({
        "auto_commit": wiki.auto_commit,
        "history": &wiki.history,
        "audit": &wiki.audit,
        "publish": &wiki.publish,
        "sections": wiki.sections(),
    });
    let encoded = serde_json::to_vec(&policy)?;
    Ok(framed_sha256(
        b"wookie.effective-publish-policy/v1",
        &[&encoded],
    ))
}

fn current_effective_publish_policy_sha256(wiki: &Wiki) -> Result<String> {
    let current = current_wiki(wiki)?;
    effective_publish_policy_sha256(&current)
}

fn unlock_control_paths(wiki: &Wiki) -> BTreeSet<String> {
    let mut paths = BTreeSet::from([".unlocks.toml".to_string()]);
    for (section, config) in wiki.sections() {
        if config.is_locked() {
            paths.insert(format!(".unlocks/{section}.toml"));
        }
    }
    paths
}

fn snapshot_unlock_controls(wiki: &Wiki) -> Result<BTreeMap<String, FileSnapshot>> {
    let paths = unlock_control_paths(wiki);
    snapshot_unlock_controls_for_paths(wiki, paths.iter())
}

fn snapshot_unlock_controls_for_paths<'a>(
    wiki: &Wiki,
    paths: impl IntoIterator<Item = &'a String>,
) -> Result<BTreeMap<String, FileSnapshot>> {
    let mut controls = BTreeMap::new();
    for relative in paths {
        let path = wiki.contained_path(Path::new(relative))?;
        controls.insert(relative.clone(), snapshot_file(&path)?);
    }
    Ok(controls)
}

fn snapshot_catalog(wiki: &Wiki) -> Result<BTreeMap<String, FileSnapshot>> {
    let mut pages = BTreeMap::new();
    for (id, path) in wiki.page_files_strict()? {
        pages.insert(id, snapshot_file(&path)?);
    }
    Ok(pages)
}

fn verify_catalog_state(wiki: &Wiki, expected: &CatalogState) -> Result<()> {
    let current = catalog_state(&snapshot_catalog(wiki)?)?;
    if &current != expected {
        bail!("wiki catalog differs from the journaled full-catalog state");
    }
    Ok(())
}

fn verify_unrelated_catalog_state(wiki: &Wiki, journal: &PublishJournal) -> Result<()> {
    let mut current = catalog_state(&snapshot_catalog(wiki)?)?.pages;
    let mut expected = journal.environment.catalog_before.pages.clone();
    for entry in &journal.entries {
        let id = entry
            .relative_path
            .strip_prefix("pages/")
            .and_then(|path| path.strip_suffix(".md"))
            .context("journal path is outside the page catalog")?;
        current.remove(id);
        expected.remove(id);
    }
    if current != expected {
        bail!("an unrelated wiki page changed during the publication transaction");
    }
    Ok(())
}

fn current_wiki(wiki: &Wiki) -> Result<Wiki> {
    let home = wiki
        .dir
        .parent()
        .context("wiki directory has no Wookie home")?;
    crate::wiki::open(home, &wiki.slug)
        .context("reloading effective publish policy from persistent configuration")
}

fn verify_journal_control_state(wiki: &Wiki, journal: &PublishJournal, after: bool) -> Result<()> {
    let expected_config = if after {
        &journal.environment.config_after
    } else {
        &journal.environment.config_before
    };
    let config_path = wiki.contained_path(Path::new("wookie.toml"))?;
    if snapshot_file(&config_path)? != *expected_config {
        bail!("wookie.toml differs from the journaled configuration state");
    }
    let current = current_wiki(wiki)?;
    let expected_policy = if after {
        &journal.environment.effective_policy_after_sha256
    } else {
        &journal.environment.effective_policy_before_sha256
    };
    if effective_publish_policy_sha256(&current)? != *expected_policy {
        bail!("effective publish policy differs from the journaled state");
    }
    let required_control_paths = unlock_control_paths(&current);
    let journal_control_paths = journal
        .environment
        .unlock_controls_after
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>();
    if journal_control_paths != required_control_paths {
        bail!("journal does not cover every configured section lock control");
    }
    let controls =
        snapshot_unlock_controls_for_paths(wiki, journal.environment.unlock_controls_after.keys())?;
    if controls != journal.environment.unlock_controls_after {
        bail!("section lock controls differ from the journaled post-relock state");
    }
    let configured = current.sections();
    let affected_locked_rules = journal
        .entries
        .iter()
        .filter_map(|entry| {
            entry
                .relative_path
                .strip_prefix("pages/")
                .and_then(|path| path.strip_suffix(".md"))
                .and_then(|id| id.split_once('/').map(|(section, _)| section))
        })
        .filter(|section| {
            configured.get(*section).is_some_and(|config| {
                config.kind == crate::wiki::SectionKind::Rules && config.is_locked()
            })
        })
        .map(str::to_string)
        .collect::<BTreeSet<_>>();
    if affected_locked_rules != journal.environment.relocked_rule_sections {
        bail!("journal does not account for every affected locked rules section");
    }
    for section in &journal.environment.relocked_rule_sections {
        let configured_as_rules = configured.get(section).is_some_and(|config| {
            config.kind == crate::wiki::SectionKind::Rules && config.is_locked()
        });
        if !configured_as_rules || current.is_unlocked(section) {
            bail!("rules section '{section}' is not verifiably relocked");
        }
    }
    Ok(())
}

fn verify_journal_environment(wiki: &Wiki, journal: &PublishJournal, after: bool) -> Result<()> {
    verify_catalog_state(
        wiki,
        if after {
            &journal.environment.catalog_after
        } else {
            &journal.environment.catalog_before
        },
    )?;
    verify_journal_control_state(wiki, journal, after)
}

/// Canonical framed SHA-256 of page ids and exact raw bytes. File metadata is
/// still compared separately by the publish transaction and journal.
pub fn raw_catalog_sha256(wiki: &Wiki) -> Result<String> {
    snapshot::wiki_content_hash(wiki)
}

fn ensure_publish_targets_clean(wiki: &Wiki, paths: &[String]) -> Result<()> {
    if !wiki.auto_commit || paths.is_empty() {
        return Ok(());
    }
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(&wiki.dir)
        .args([
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--",
        ])
        .args(paths);
    let output = run_bounded_command(&mut command, Some(MAX_GIT_STATUS_OUTPUT_BYTES))
        .with_context(|| format!("inspecting publish targets in {}", wiki.dir.display()))?;
    if output.stdout_truncated {
        bail!("publish target status exceeds the safe byte limit");
    }
    if !output.status.success() {
        bail!(
            "cannot inspect publish target paths: {}",
            bounded_stderr(&output)
        );
    }
    if !output.stdout.is_empty() {
        bail!(
            "publish target paths contain pre-existing staged, unstaged, or untracked Git changes; commit or restore them before applying"
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    metadata: Option<FileMetadata>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileMetadata {
    readonly: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    unix_mode: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogPageState {
    raw_sha256: String,
    metadata: FileMetadata,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogState {
    raw_content_sha256: String,
    pages: BTreeMap<String, CatalogPageState>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalEnvironment {
    catalog_before: CatalogState,
    catalog_after: CatalogState,
    config_before: FileSnapshot,
    config_after: FileSnapshot,
    effective_policy_before_sha256: String,
    effective_policy_after_sha256: String,
    unlock_controls_before: BTreeMap<String, FileSnapshot>,
    unlock_controls_after: BTreeMap<String, FileSnapshot>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    relocked_rule_sections: BTreeSet<String>,
}

fn default_file_metadata() -> FileMetadata {
    FileMetadata {
        readonly: false,
        #[cfg(unix)]
        unix_mode: Some(0o600),
        #[cfg(not(unix))]
        unix_mode: None,
    }
}

fn snapshot_file(path: &Path) -> Result<FileSnapshot> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(FileSnapshot::default()),
        Err(error) => Err(error).with_context(|| format!("inspecting {}", path.display())),
        Ok(initial) => {
            if initial.file_type().is_symlink() || !initial.is_file() {
                bail!("managed file {} must be a regular file", path.display());
            }
            let raw = snapshot::read_raw_page(path)
                .with_context(|| format!("reading managed file {}", path.display()))?;
            let content = String::from_utf8(raw)
                .with_context(|| format!("managed file {} is not valid UTF-8", path.display()))?;
            // `read_raw_page` verifies opened-handle and path identity after a
            // bounded read. Capture permissions from the immediately following
            // verified stat; content and metadata remain exact conflict inputs.
            let metadata = fs::symlink_metadata(path)
                .with_context(|| format!("rechecking managed file {}", path.display()))?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!("managed file {} changed after snapshot", path.display());
            }
            #[cfg(unix)]
            let unix_mode = {
                use std::os::unix::fs::PermissionsExt;
                // `PermissionsExt::mode` may include the file-type bits from
                // `st_mode`. Journals store only portable permission bits so
                // a newly created file compares equal to its planned image.
                Some(metadata.permissions().mode() & 0o7777)
            };
            #[cfg(not(unix))]
            let unix_mode = None;
            Ok(FileSnapshot {
                content: Some(content),
                metadata: Some(FileMetadata {
                    readonly: metadata.permissions().readonly(),
                    unix_mode,
                }),
            })
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecoveryState {
    Prepared,
    Applying,
    Applied,
    RollingBack,
    RollbackFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalEntry {
    relative_path: String,
    before: FileSnapshot,
    after: FileSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishJournal {
    schema: String,
    transaction_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    base_revision: Option<String>,
    /// Exact wiki HEAD observed under the publication lock immediately before
    /// page application/finalization. Git-backed publishes must produce one
    /// reviewed child commit from this revision.
    pre_finalizer_head: Option<String>,
    /// Present when acceptance must leave the page paths committed in Git.
    /// Recovery can then finish or reverse history deterministically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    commit_message: Option<String>,
    environment: JournalEnvironment,
    state: RecoveryState,
    applied: usize,
    entries: Vec<JournalEntry>,
}

static TRANSACTION_COUNTER: AtomicU64 = AtomicU64::new(0);

fn transaction_id() -> String {
    let sequence = TRANSACTION_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(
        "{}-{}-{sequence}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default(),
        std::process::id()
    )
}

struct PublishLock {
    path: PathBuf,
    owner_path: PathBuf,
    token: String,
    released: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum LockPurpose {
    Publisher,
    Mutation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LockRecord {
    schema: String,
    transaction_id: String,
    pid: u32,
    #[serde(default)]
    thread_id: String,
    created_at: String,
    purpose: LockPurpose,
}

fn configure_bounded_read(options: &mut OpenOptions) {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        const O_NOFOLLOW: i32 = 0x20_000;
        const O_NONBLOCK: i32 = 0x800;
        options.custom_flags(O_NOFOLLOW | O_NONBLOCK);
    }
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        const O_NOFOLLOW: i32 = 0x100;
        const O_NONBLOCK: i32 = 0x4;
        options.custom_flags(O_NOFOLLOW | O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly",
        windows
    )))]
    let _ = options;
}

#[cfg(unix)]
fn same_bounded_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    right.is_file()
        && !right.file_type().is_symlink()
        && left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

#[cfg(not(unix))]
fn same_bounded_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    right.is_file()
        && !right.file_type().is_symlink()
        && left.len() == right.len()
        && left.modified().ok() == right.modified().ok()
        && left.created().ok() == right.created().ok()
}

/// Read attacker-controllable persistent state through one bounded,
/// non-following handle and reject path replacement during the read.
fn read_bounded_regular_file(path: &Path, max_bytes: u64, label: &str) -> Result<Vec<u8>> {
    let before = fs::symlink_metadata(path)
        .with_context(|| format!("inspecting {label} {}", path.display()))?;
    if before.file_type().is_symlink() || !before.is_file() {
        bail!("{label} must be a regular non-symlink file");
    }
    if before.len() > max_bytes {
        bail!("{label} exceeds the {max_bytes}-byte limit");
    }
    let mut options = OpenOptions::new();
    options.read(true);
    configure_bounded_read(&mut options);
    let file = options
        .open(path)
        .with_context(|| format!("opening {label} {}", path.display()))?;
    let opened = file
        .metadata()
        .with_context(|| format!("inspecting opened {label} {}", path.display()))?;
    if !same_bounded_file(&before, &opened) {
        bail!("{label} changed while it was opened");
    }
    let mut raw = Vec::with_capacity(usize::try_from(opened.len()).unwrap_or_default());
    file.take(max_bytes + 1)
        .read_to_end(&mut raw)
        .with_context(|| format!("reading {label} {}", path.display()))?;
    if raw.len() as u64 > max_bytes {
        bail!("{label} exceeds the {max_bytes}-byte limit");
    }
    let after = fs::symlink_metadata(path)
        .with_context(|| format!("rechecking {label} {}", path.display()))?;
    if !same_bounded_file(&opened, &after) || after.len() != raw.len() as u64 {
        bail!("{label} changed while it was read");
    }
    Ok(raw)
}

fn read_lock_record(path: &Path) -> Result<LockRecord> {
    let raw = read_bounded_regular_file(path, MAX_LOCK_OWNER_BYTES, "publication lock owner")?;
    let record: LockRecord = serde_json::from_slice(&raw)
        .context("publication lock contains malformed owner metadata")?;
    if record.schema != LOCK_SCHEMA {
        bail!("unsupported publication lock schema '{}'", record.schema);
    }
    Ok(record)
}

fn current_thread_id() -> String {
    format!("{:?}", std::thread::current().id())
}

fn lock_owned_by_current_thread(path: &Path) -> bool {
    fs::read_dir(path).ok().is_some_and(|entries| {
        entries.flatten().any(|entry| {
            read_lock_record(&entry.path()).ok().is_some_and(|record| {
                record.pid == std::process::id()
                    && !record.thread_id.is_empty()
                    && record.thread_id == current_thread_id()
            })
        })
    })
}

impl PublishLock {
    fn acquire(wiki: &Wiki, purpose: LockPurpose) -> Result<Self> {
        let path = wiki.contained_path(Path::new(PUBLISH_LOCK_PATH))?;
        let token = transaction_id();
        fs::create_dir(&path).with_context(|| {
            format!(
                "acquiring publication lock {} (recover an interrupted publish before retrying)",
                path.display()
            )
        })?;
        let owner_path = path.join(format!("owner-{}-{token}.json", std::process::id()));
        let setup = (|| -> Result<()> {
            let record = LockRecord {
                schema: LOCK_SCHEMA.to_string(),
                transaction_id: token.clone(),
                pid: std::process::id(),
                thread_id: current_thread_id(),
                created_at: chrono::Utc::now().to_rfc3339(),
                purpose,
            };
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&owner_path)?;
            file.write_all(&serde_json::to_vec(&record)?)?;
            file.sync_all()?;
            #[cfg(unix)]
            if let Ok(directory) = fs::File::open(&path) {
                let _ = directory.sync_all();
            }
            Ok(())
        })();
        if let Err(error) = setup {
            let _ = fs::remove_file(&owner_path);
            let _ = fs::remove_dir(&path);
            return Err(error).context("initializing publication lock");
        }
        Ok(Self {
            path,
            owner_path,
            token,
            released: false,
        })
    }

    fn release(&mut self) -> Result<()> {
        if self.released {
            return Ok(());
        }
        let current = read_lock_record(&self.owner_path)
            .with_context(|| format!("reading publication lock {}", self.path.display()))?;
        if current.schema != LOCK_SCHEMA
            || current.transaction_id != self.token
            || current.pid != std::process::id()
        {
            bail!("publication lock ownership changed; refusing to remove it");
        }
        fs::remove_file(&self.owner_path)
            .with_context(|| format!("removing publication lock owner {}", self.path.display()))?;
        fs::remove_dir(&self.path)
            .with_context(|| format!("removing publication lock {}", self.path.display()))?;
        self.released = true;
        Ok(())
    }

    fn is_for(&self, wiki: &Wiki) -> bool {
        self.path == wiki.dir.join(PUBLISH_LOCK_PATH) && !self.released
    }
}

fn relock_rules_under_publish_lock(
    wiki: &Wiki,
    lock: &PublishLock,
    sections: &BTreeSet<String>,
) -> Result<()> {
    if sections.is_empty() {
        return Ok(());
    }
    if !lock.is_for(wiki) {
        bail!("publication lock belongs to a different wiki");
    }
    let configured = wiki.sections();
    create_contained_dir_all(&wiki.dir, Path::new(".unlocks"))?;
    for section in sections {
        validate_id(section)?;
        if section.contains('/')
            || !configured
                .get(section)
                .is_some_and(|config| config.kind == crate::wiki::SectionKind::Rules)
        {
            bail!("cannot relock unknown rules section '{section}'");
        }
        let relative = Path::new(".unlocks").join(format!("{section}.toml"));
        let path = contained_path(&wiki.dir, &relative)?;
        atomic_write(&path, b"locked = true\n")
            .with_context(|| format!("relocking rules section '{section}'"))?;
    }
    Ok(())
}

/// Short-lived exclusion guard used by every ordinary wiki mutation. It uses
/// the exact lock representation and ownership checks as a publication.
pub(crate) struct MutationGuard {
    _lock: PublishLock,
    wiki_dir: PathBuf,
}

pub(crate) fn acquire_mutation_guard(wiki: &Wiki) -> Result<MutationGuard> {
    acquire_mutation_guard_inner(wiki, false)
}

/// Recovery is the only mutation allowed while an ingest reconciliation
/// marker exists. Publication journals remain independently authoritative.
pub(crate) fn acquire_ingest_recovery_guard(wiki: &Wiki) -> Result<MutationGuard> {
    acquire_mutation_guard_inner(wiki, true)
}

fn acquire_mutation_guard_inner(wiki: &Wiki, allow_ingest_recovery: bool) -> Result<MutationGuard> {
    ensure_no_publish_journal(wiki)?;
    if !allow_ingest_recovery {
        ensure_no_ingest_recovery(wiki)?;
    }
    let started = Instant::now();
    let timeout = Duration::from_millis(wiki.history.lock_timeout_ms);
    let lock_path = wiki.contained_path(Path::new(PUBLISH_LOCK_PATH))?;
    loop {
        match PublishLock::acquire(wiki, LockPurpose::Mutation) {
            Ok(lock) => {
                ensure_no_publish_journal(wiki)?;
                if !allow_ingest_recovery {
                    ensure_no_ingest_recovery(wiki)?;
                }
                return Ok(MutationGuard {
                    _lock: lock,
                    wiki_dir: wiki.dir.clone(),
                });
            }
            Err(error) => match fs::symlink_metadata(&lock_path) {
                Err(inspect) if inspect.kind() == std::io::ErrorKind::NotFound => {
                    match fs::symlink_metadata(&wiki.dir) {
                        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                            if started.elapsed() >= timeout {
                                return Err(error).with_context(|| {
                                    format!(
                                        "timed out after {} ms waiting for wiki publication lock",
                                        wiki.history.lock_timeout_ms
                                    )
                                });
                            }
                            continue;
                        }
                        Err(root_error) if root_error.kind() == std::io::ErrorKind::NotFound => {
                            return Err(error).context(
                                "wiki moved or was removed while waiting for its mutation lock",
                            );
                        }
                        _ => return Err(error),
                    }
                }
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                    if lock_owned_by_current_thread(&lock_path) {
                        return Err(error)
                            .context("wiki mutation lock is not reentrant in the current thread");
                    }
                    if started.elapsed() >= timeout {
                        return Err(error).with_context(|| {
                            format!(
                                "timed out after {} ms waiting for wiki publication lock",
                                wiki.history.lock_timeout_ms
                            )
                        });
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                _ => return Err(error),
            },
        }
    }
}

/// Attempt the shared writer guard exactly once. Retrieval-cache persistence
/// uses this path so an active or interrupted publication never blocks a
/// read-only query merely to update disposable derived state.
pub(crate) fn try_acquire_mutation_guard(wiki: &Wiki) -> Option<MutationGuard> {
    ensure_no_publish_journal(wiki).ok()?;
    ensure_no_ingest_recovery(wiki).ok()?;
    let lock = PublishLock::acquire(wiki, LockPurpose::Mutation).ok()?;
    if ensure_no_publish_journal(wiki).is_err() || ensure_no_ingest_recovery(wiki).is_err() {
        drop(lock);
        return None;
    }
    Some(MutationGuard {
        _lock: lock,
        wiki_dir: wiki.dir.clone(),
    })
}

fn ensure_no_ingest_recovery(wiki: &Wiki) -> Result<()> {
    let path = wiki.contained_path(Path::new(".ingest-reconciliation-recovery.json"))?;
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspecting {}", path.display())),
        Ok(_) => bail!(
            "an unresolved ingest reconciliation marker exists at {}; run explicit `wookie ingest --recover accept|rollback` before any wiki mutation",
            path.display()
        ),
    }
}

fn ensure_no_publish_journal(wiki: &Wiki) -> Result<()> {
    let path = wiki.contained_path(Path::new(PUBLISH_JOURNAL_PATH))?;
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspecting {}", path.display())),
        Ok(_) => bail!(
            "an interrupted publication journal exists at {}; run explicit `wookie publish --recover rollback|accept` before any wiki mutation",
            path.display()
        ),
    }
}

impl MutationGuard {
    pub(crate) fn is_for(&self, wiki: &Wiki) -> bool {
        self.wiki_dir == wiki.dir
    }

    /// Retarget ownership after the guarded wiki directory itself is renamed.
    /// The lock directory and owner record move with the wiki, so only the
    /// bookkeeping paths change; ownership is revalidated before returning.
    pub(crate) fn relocate_after_rename(&mut self, old_dir: &Path, new_dir: &Path) -> Result<()> {
        if self.wiki_dir != old_dir || old_dir.parent() != new_dir.parent() {
            bail!("mutation guard cannot be relocated outside its Wookie home");
        }
        let owner_name = self
            ._lock
            .owner_path
            .file_name()
            .context("publication lock owner has no file name")?
            .to_owned();
        let new_lock_path = new_dir.join(PUBLISH_LOCK_PATH);
        let new_owner_path = new_lock_path.join(owner_name);
        let record = read_lock_record(&new_owner_path)
            .with_context(|| format!("reading relocated lock {}", new_owner_path.display()))?;
        if record.schema != LOCK_SCHEMA
            || record.transaction_id != self._lock.token
            || record.pid != std::process::id()
            || record.purpose != LockPurpose::Mutation
        {
            bail!("relocated publication lock ownership changed");
        }
        self._lock.path = new_lock_path;
        self._lock.owner_path = new_owner_path;
        self.wiki_dir = new_dir.to_path_buf();
        Ok(())
    }
}

impl Drop for PublishLock {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

/// A begun transaction owns the wiki-wide publication lock and a durable
/// rollback journal. Call `finish` only after the caller's Git commit or other
/// finalizer succeeds; otherwise call `rollback`.
pub struct PublishTransaction<'a> {
    wiki: &'a Wiki,
    lock: PublishLock,
    journal: PublishJournal,
    plan: PublishPlan,
    finished: bool,
}

impl<'a> PublishTransaction<'a> {
    #[cfg(test)]
    pub fn begin(
        wiki: &'a Wiki,
        preflight: Preflight,
        actual_revision: Option<&str>,
    ) -> Result<Self> {
        Self::begin_with_approved_locked_pages(
            wiki,
            preflight,
            actual_revision,
            &BTreeSet::new(),
            &BTreeSet::new(),
            None,
        )
    }

    fn begin_with_approved_locked_pages(
        wiki: &'a Wiki,
        preflight: Preflight,
        actual_revision: Option<&str>,
        approved_locked_pages: &BTreeSet<String>,
        relock_rule_sections: &BTreeSet<String>,
        recovery_commit_message: Option<&str>,
    ) -> Result<Self> {
        if preflight.report.has_errors() {
            bail!(
                "publish preflight has {} error(s); no files were changed",
                preflight.report.summary.errors
            );
        }
        if preflight.plan.observed_revision.as_deref() != actual_revision {
            bail!("observed wiki revision changed after preflight; regenerate the plan");
        }
        verify_base_revision(preflight.plan.base_revision.as_deref(), actual_revision)?;

        ensure_no_publish_journal(wiki)?;
        let lock = PublishLock::acquire(wiki, LockPurpose::Publisher)?;
        ensure_no_publish_journal(wiki)?;
        // This check belongs inside the same lock as catalog revalidation and
        // journal creation. Otherwise a second CLI or MCP publisher can dirty
        // a target in the gap between a command-layer check and transaction.
        ensure_publish_targets_clean(wiki, &preflight.plan.relative_paths())?;
        let locked_revision = current_wiki_revision(wiki)?;
        let locked_or_supplied = locked_revision.as_deref().or(actual_revision);
        if preflight.plan.observed_revision.is_some()
            && locked_or_supplied != preflight.plan.observed_revision.as_deref()
        {
            bail!("wiki revision changed after preflight; regenerate the plan");
        }
        verify_base_revision(preflight.plan.base_revision.as_deref(), locked_or_supplied)?;
        let configured = wiki.sections();
        let required_rule_relocks = preflight
            .plan
            .operations
            .iter()
            .filter_map(|operation| operation.page.split_once('/').map(|(section, _)| section))
            .filter(|section| {
                configured.get(*section).is_some_and(|config| {
                    config.kind == crate::wiki::SectionKind::Rules && config.is_locked()
                })
            })
            .map(str::to_string)
            .collect::<BTreeSet<_>>();
        if required_rule_relocks != *relock_rule_sections {
            bail!("publish relock scope does not cover every affected locked rules section");
        }
        for operation in &preflight.plan.operations {
            if approved_locked_pages.contains(&operation.page) {
                continue;
            }
            wiki.assert_writable(&operation.page).with_context(|| {
                format!(
                    "page '{}' became non-writable after preflight",
                    operation.page
                )
            })?;
        }
        // Exact comparison is the concurrency boundary. SHA-256 hashes make
        // receipts and plans reviewable, but equality never trusts a digest.
        if snapshot_catalog(wiki)? != preflight.catalog_before {
            bail!("wiki page catalog changed after preflight; regenerate the plan");
        }
        let config_path = wiki.contained_path(Path::new("wookie.toml"))?;
        if snapshot_file(&config_path)? != preflight.config_before {
            bail!("wookie.toml changed after preflight; regenerate the plan");
        }
        if snapshot_unlock_controls_for_paths(wiki, preflight.unlock_controls_before.keys())?
            != preflight.unlock_controls_before
        {
            bail!("section lock controls changed after preflight; regenerate the plan");
        }
        if current_effective_publish_policy_sha256(wiki)? != preflight.effective_policy_sha256 {
            bail!("effective publish policy changed after preflight; regenerate the plan");
        }
        relock_rules_under_publish_lock(wiki, &lock, relock_rule_sections)?;
        let unlock_controls_after =
            snapshot_unlock_controls_for_paths(wiki, preflight.unlock_controls_before.keys())?;
        for section in relock_rule_sections {
            if wiki.is_unlocked(section) {
                bail!("rules section '{section}' remained unlocked after relock");
            }
        }
        let entries: Vec<JournalEntry> = preflight
            .deltas
            .into_iter()
            .map(|delta| JournalEntry {
                relative_path: delta.relative_path,
                before: delta.before,
                after: delta.after,
            })
            .collect();
        for entry in &entries {
            let path = contained_path(&wiki.dir, Path::new(&entry.relative_path))?;
            let current = snapshot_file(&path)?;
            if current != entry.before {
                bail!(
                    "{} changed after preflight; no files were changed",
                    entry.relative_path
                );
            }
        }
        let journal = PublishJournal {
            schema: JOURNAL_SCHEMA.to_string(),
            transaction_id: lock.token.clone(),
            base_revision: preflight.plan.base_revision.clone(),
            pre_finalizer_head: locked_revision.clone(),
            commit_message: recovery_commit_message.map(crate::history::canonical_commit_message),
            environment: JournalEnvironment {
                catalog_before: preflight.catalog_before_state,
                catalog_after: preflight.catalog_after_state,
                config_before: preflight.config_before.clone(),
                config_after: preflight.config_before,
                effective_policy_before_sha256: preflight.effective_policy_sha256.clone(),
                effective_policy_after_sha256: preflight.effective_policy_sha256,
                unlock_controls_before: preflight.unlock_controls_before,
                unlock_controls_after,
                relocked_rule_sections: relock_rule_sections.clone(),
            },
            state: RecoveryState::Prepared,
            applied: 0,
            entries,
        };
        write_journal(wiki, &journal)?;
        Ok(Self {
            wiki,
            lock,
            journal,
            plan: preflight.plan,
            finished: false,
        })
    }

    pub fn plan(&self) -> &PublishPlan {
        &self.plan
    }

    pub fn apply(&mut self) -> Result<()> {
        if self.journal.state != RecoveryState::Prepared {
            bail!("publication transaction is not in the prepared state");
        }
        self.journal.state = RecoveryState::Applying;
        write_journal(self.wiki, &self.journal)?;

        for index in 0..self.journal.entries.len() {
            let entry = &self.journal.entries[index];
            let path = contained_path(&self.wiki.dir, Path::new(&entry.relative_path))?;
            let result = write_snapshot(&self.wiki.dir, &path, &entry.after);
            if let Err(cause) = result {
                let rollback = rollback_journal(self.wiki, &mut self.journal);
                return match rollback {
                    Ok(()) => {
                        if let Err(verify_error) =
                            verify_journal_environment(self.wiki, &self.journal, false)
                        {
                            self.journal.state = RecoveryState::RollbackFailed;
                            let _ = write_journal(self.wiki, &self.journal);
                            self.finished = true;
                            return Err(anyhow!(
                                "publish failed: {cause:#}; rollback state could not be verified: {verify_error:#}; journal retained for recovery"
                            ));
                        }
                        match remove_journal(self.wiki) {
                            Ok(()) => {
                                self.finished = true;
                                Err(cause.context("publish failed; all changes were rolled back"))
                            }
                            Err(cleanup_error) => Err(anyhow!(
                                "publish failed and changes were rolled back, but the completed journal could not be removed: {cleanup_error:#}; original error: {cause:#}"
                            )),
                        }
                    }
                    Err(rollback_error) => Err(anyhow!(
                        "publish failed: {cause:#}; rollback was incomplete: {rollback_error:#}; journal retained for recovery"
                    )),
                };
            }
            self.journal.applied = index + 1;
            write_journal(self.wiki, &self.journal)?;
        }
        self.journal.state = RecoveryState::Applied;
        write_journal(self.wiki, &self.journal)
    }

    /// Accept the applied filesystem state. Call this only after all external
    /// finalization (notably the single Git commit) has succeeded.
    pub fn finish(mut self) -> Result<PublishPlan> {
        if self.journal.state != RecoveryState::Applied {
            bail!("cannot finish a publication that has not been fully applied");
        }
        if let Err(error) = verify_journal_images(self.wiki, &self.journal, true) {
            // The finalizer (including Git hooks) ran while the journal and
            // lock were held. A changed image is no longer safe to accept or
            // silently roll back after external history may have succeeded.
            self.finished = true;
            return Err(error.context(
                "publication result changed during finalization; journal retained for explicit recovery",
            ));
        }
        if let Err(error) = verify_journal_environment(self.wiki, &self.journal, true) {
            self.finished = true;
            return Err(error.context(
                "publication catalog or control state changed during finalization; journal retained for explicit recovery",
            ));
        }
        if self.journal.commit_message.is_some() {
            if let Err(error) = verify_committed_journal_images(self.wiki, &self.journal, true) {
                self.finished = true;
                return Err(error.context(
                    "publication history does not match the reviewed result; journal retained for explicit recovery",
                ));
            }
            if let Err(error) = verify_expected_publish_commit(self.wiki, &self.journal) {
                self.finished = true;
                return Err(error.context(
                    "publication commit lineage/message does not match the reviewed transaction; journal retained",
                ));
            }
        }
        // External finalization has already succeeded. From this point on an
        // administrative cleanup failure must never roll back files behind a
        // successful Git commit. The applied journal can be accepted later.
        self.finished = true;
        remove_journal(self.wiki)?;
        self.lock.release()?;
        Ok(self.plan.clone())
    }

    pub fn rollback(mut self) -> Result<()> {
        if self.journal.commit_message.is_some() {
            if let Err(error) = verify_pre_finalizer_head_unchanged(self.wiki, &self.journal) {
                self.finished = true;
                return Err(error.context(
                    "wiki HEAD changed during failed finalization; journal retained for recovery",
                ));
            }
            if let Err(error) = verify_index_and_head_images(self.wiki, &self.journal, false) {
                self.finished = true;
                return Err(error.context(
                    "publication history is uncertain after finalizer failure; journal retained for explicit recovery",
                ));
            }
        }
        if let Err(error) = verify_unrelated_catalog_state(self.wiki, &self.journal)
            .and_then(|()| verify_journal_control_state(self.wiki, &self.journal, true))
        {
            self.finished = true;
            return Err(error.context(
                "unrelated catalog or control state changed during failed finalization; journal retained",
            ));
        }
        if let Err(error) = rollback_journal(self.wiki, &mut self.journal) {
            self.finished = true;
            return Err(error.context("filesystem rollback was incomplete; journal retained"));
        }
        if let Err(error) = verify_journal_images(self.wiki, &self.journal, false) {
            self.finished = true;
            return Err(error.context("rollback verification failed; journal retained"));
        }
        if let Err(error) = verify_journal_environment(self.wiki, &self.journal, false) {
            self.finished = true;
            return Err(
                error.context("full rollback state differs from the journal; journal retained")
            );
        }
        if self.journal.commit_message.is_some() {
            if let Err(error) = verify_committed_journal_images(self.wiki, &self.journal, false) {
                self.finished = true;
                return Err(error.context(
                    "rolled-back history does not match before-images; journal retained",
                ));
            }
        }
        self.finished = true;
        remove_journal(self.wiki)?;
        self.lock.release()
    }
}

impl Drop for PublishTransaction<'_> {
    fn drop(&mut self) {
        if !self.finished && self.journal.state == RecoveryState::Prepared {
            let _ = remove_journal(self.wiki);
        }
        // Once apply begins, especially after it reaches Applied, unwinding
        // may cross a successful external commit. Retain the journal and let
        // explicit recovery choose accept or rollback; Drop never guesses.
    }
}

/// Apply a preflight, invoke the caller's finalizer while the lock and journal
/// remain active, then accept or restore the filesystem state.
#[cfg(test)]
pub fn transact<F>(
    wiki: &Wiki,
    preflight: Preflight,
    actual_revision: Option<&str>,
    finalize: F,
) -> Result<PublishPlan>
where
    F: FnOnce(&PublishPlan) -> Result<()>,
{
    let mut transaction = PublishTransaction::begin(wiki, preflight, actual_revision)?;
    transaction.apply()?;
    if let Err(cause) = finalize(transaction.plan()) {
        return match transaction.rollback() {
            Ok(()) => Err(cause.context("publication finalizer failed; changes were rolled back")),
            Err(rollback_error) => Err(anyhow!(
                "publication finalizer failed: {cause:#}; rollback was incomplete: {rollback_error:#}"
            )),
        };
    }
    transaction.finish()
}

/// Apply an explicitly approved plan without opening a section-wide unlock
/// window. Only the exact locked pages in this set bypass `assert_writable`;
/// the shared publication lock still protects the complete revalidation,
/// journal, write, and finalizer sequence.
pub fn transact_with_approved_locked_pages<F>(
    wiki: &Wiki,
    preflight: Preflight,
    actual_revision: Option<&str>,
    approved_locked_pages: &BTreeSet<String>,
    relock_rule_sections: &BTreeSet<String>,
    recovery_commit_message: Option<&str>,
    finalize: F,
) -> Result<PublishPlan>
where
    F: FnOnce(&PublishPlan) -> Result<()>,
{
    let mut transaction = PublishTransaction::begin_with_approved_locked_pages(
        wiki,
        preflight,
        actual_revision,
        approved_locked_pages,
        relock_rule_sections,
        recovery_commit_message,
    )?;
    transaction.apply()?;
    if let Err(cause) = finalize(transaction.plan()) {
        return match transaction.rollback() {
            Ok(()) => Err(cause.context("publication finalizer failed; changes were rolled back")),
            Err(rollback_error) => Err(anyhow!(
                "publication finalizer failed: {cause:#}; rollback was incomplete: {rollback_error:#}"
            )),
        };
    }
    transaction.finish()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryAction {
    Rollback,
    Accept,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryStatus {
    pub transaction_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_revision: Option<String>,
    pub state: RecoveryState,
    pub applied: usize,
    pub total: usize,
}

/// Return compact recovery metadata without exposing before/after page bodies.
/// A malformed or redirected journal is an error, not an absent transaction.
pub fn recovery_status(wiki: &Wiki) -> Result<Option<RecoveryStatus>> {
    let path = wiki.contained_path(Path::new(PUBLISH_JOURNAL_PATH))?;
    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("inspecting {}", path.display())),
        Ok(_) => {}
    }
    let journal = read_journal(&path)?;
    Ok(Some(RecoveryStatus {
        transaction_id: journal.transaction_id,
        base_revision: journal.base_revision,
        state: journal.state,
        applied: journal.applied,
        total: journal.entries.len(),
    }))
}

/// Recover a crash journal. `force_stale_lock` is deliberately explicit:
/// removing a lock while another publisher is alive can destroy its work.
pub fn recover(wiki: &Wiki, action: RecoveryAction, force_stale_lock: bool) -> Result<()> {
    let journal_path = wiki.contained_path(Path::new(PUBLISH_JOURNAL_PATH))?;
    let lock_path = wiki.contained_path(Path::new(PUBLISH_LOCK_PATH))?;
    let journal_exists = match fs::symlink_metadata(&journal_path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            bail!("publication journal must be a regular file")
        }
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(error).context("inspecting publication journal"),
    };

    // A short-lived writer has no journal. A publisher can also die just
    // before journal creation or just after successful journal cleanup. In
    // every no-journal case recovery may clear only a verified dead lock; it
    // never guesses at page contents.
    if !journal_exists {
        if action != RecoveryAction::Rollback {
            bail!("cannot accept a publication without a recovery journal");
        }
        if !force_stale_lock {
            bail!("no publication journal exists; force is required to clear a stale writer lock");
        }
        if !lock_path.exists() {
            bail!("no publication journal or mutation lock exists to recover");
        }
        let Some((owner, owner_path)) = read_lock_owner(&lock_path)? else {
            return remove_stale_ownerless_lock(
                &lock_path,
                Duration::from_secs(wiki.history.lock_stale_seconds),
            );
        };
        if process_is_alive(owner.pid) {
            bail!("mutation/publication lock belongs to a live process; refusing recovery");
        }
        return remove_verified_lock(&lock_path, &owner, &owner_path);
    }

    let mut journal = read_journal(&journal_path)?;
    if lock_path.exists() {
        let Some((owner, owner_path)) = read_lock_owner(&lock_path)? else {
            if !force_stale_lock {
                bail!(
                    "publication lock has no owner metadata; force is required after its stale interval"
                );
            }
            remove_stale_ownerless_lock(
                &lock_path,
                Duration::from_secs(wiki.history.lock_stale_seconds),
            )?;
            // The ownerless directory is gone; recovery acquires a verified
            // publisher lock below before trusting the journal again.
            let mut lock = PublishLock::acquire(wiki, LockPurpose::Publisher)?;
            let locked_journal = read_journal(&journal_path)?;
            if locked_journal != journal {
                bail!("publication journal changed during recovery lock acquisition");
            }
            journal = locked_journal;
            recover_locked_journal(wiki, &mut journal, action)?;
            remove_journal(wiki)?;
            return lock.release();
        };
        if process_is_alive(owner.pid) {
            bail!("publication lock belongs to a live process; refusing recovery");
        }
        if !force_stale_lock {
            let detail = if owner.purpose == LockPurpose::Publisher
                && owner.transaction_id == journal.transaction_id
            {
                "publication lock still exists"
            } else {
                "publication lock does not match the recovery journal"
            };
            bail!("{detail}; confirm no writer is running before forcing recovery");
        }
        // A crash can leave the journal paired with a later mutation waiter
        // lock. Force may clear that mismatch only after its signed owner is
        // verified dead; live ownership was rejected above.
        remove_verified_lock(&lock_path, &owner, &owner_path)?;
    }
    let mut lock = PublishLock::acquire(wiki, LockPurpose::Publisher)?;
    let locked_journal = read_journal(&journal_path)?;
    if locked_journal != journal {
        bail!("publication journal changed during recovery lock acquisition");
    }
    journal = locked_journal;
    recover_locked_journal(wiki, &mut journal, action)?;
    remove_journal(wiki)?;
    lock.release()
}

fn recover_locked_journal(
    wiki: &Wiki,
    journal: &mut PublishJournal,
    action: RecoveryAction,
) -> Result<()> {
    match action {
        RecoveryAction::Rollback => {
            let history_state = classify_recovery_history(wiki, journal)?;
            verify_unrelated_catalog_state(wiki, journal)
                .context("unrelated catalog state changed before recovery rollback")?;
            verify_journal_control_state(wiki, journal, false)
                .context("configuration or lock controls changed before recovery rollback")?;
            rollback_journal(wiki, journal)?;
            verify_journal_images(wiki, journal, false)
                .context("recovery rollback did not restore every before-image")?;
            verify_journal_environment(wiki, journal, false)
                .context("recovery rollback did not restore the full journaled state")?;
            finish_recovery_history(wiki, journal, RecoveryAction::Rollback, &history_state)?;
            verify_journal_images(wiki, journal, false)
                .context("recovery history finalization changed restored before-images")?;
            verify_journal_environment(wiki, journal, false)
                .context("recovery history finalization changed the full rollback state")?;
            if journal.commit_message.is_some() {
                verify_committed_journal_images(wiki, journal, false).context(
                    "recovery history does not match the restored before-images; journal retained",
                )?;
                verify_recovery_rollback_lineage(wiki, journal, &history_state)?;
            }
        }
        RecoveryAction::Accept => {
            // Classify lineage before any Git mutation. In particular, never
            // stage or commit an accepted image on top of an unrelated HEAD.
            // An exact already-published child is verification-only.
            let history_state = classify_recovery_history(wiki, journal)?;
            verify_journal_images(wiki, journal, true)
                .context("cannot accept recovered publication because page images do not match")?;
            verify_journal_environment(wiki, journal, true)
                .context("cannot accept recovered publication because full state diverged")?;
            finish_recovery_history(wiki, journal, RecoveryAction::Accept, &history_state)?;
            verify_journal_images(wiki, journal, true)
                .context("recovery history finalization changed page images; journal retained")?;
            verify_journal_environment(wiki, journal, true)
                .context("recovery history finalization changed full state; journal retained")?;
            if journal.commit_message.is_some() {
                verify_committed_journal_images(wiki, journal, true).context(
                    "recovery history does not match the accepted page images; journal retained",
                )?;
                verify_expected_publish_commit(wiki, journal).context(
                    "accepted publication commit lineage/message is not the reviewed transaction",
                )?;
            }
        }
    }
    Ok(())
}

fn verify_journal_images(wiki: &Wiki, journal: &PublishJournal, after: bool) -> Result<()> {
    for entry in &journal.entries {
        let path = contained_path(&wiki.dir, Path::new(&entry.relative_path))?;
        let expected = if after { &entry.after } else { &entry.before };
        if snapshot_file(&path)? != *expected {
            bail!(
                "{} does not match the journal's {} image",
                entry.relative_path,
                if after { "after" } else { "before" }
            );
        }
    }
    Ok(())
}

fn journal_paths(journal: &PublishJournal) -> Result<Vec<String>> {
    let paths = journal
        .entries
        .iter()
        .map(|entry| entry.relative_path.clone())
        .collect::<Vec<_>>();
    if paths.is_empty() {
        bail!("cannot verify committed publication without page paths");
    }
    Ok(paths)
}

fn verify_index_and_head_images(wiki: &Wiki, journal: &PublishJournal, after: bool) -> Result<()> {
    let paths = journal_paths(journal)?;
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(&wiki.dir)
        .args(["diff", "--cached", "--quiet", "--"])
        .args(&paths);
    let index =
        run_bounded_command(&mut command, None).context("verifying publication Git index")?;
    match index.status.code() {
        Some(0) => verify_head_images(wiki, journal, after),
        Some(1) => bail!("published page paths remain staged in the Git index"),
        _ => bail!(
            "cannot verify publication Git index: {}",
            bounded_stderr(&index)
        ),
    }
}

fn verify_committed_journal_images(
    wiki: &Wiki,
    journal: &PublishJournal,
    after: bool,
) -> Result<()> {
    let paths = journal_paths(journal)?;
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(&wiki.dir)
        .args([
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--",
        ])
        .args(&paths);
    let status = run_bounded_command(&mut command, Some(MAX_GIT_STATUS_OUTPUT_BYTES))
        .context("verifying committed publication path status")?;
    if status.stdout_truncated {
        bail!("committed publication path status exceeds the safe byte limit");
    }
    if !status.status.success() {
        bail!(
            "cannot verify committed publication paths: {}",
            bounded_stderr(&status)
        );
    }
    if !status.stdout.is_empty() {
        bail!("published page paths are not clean against HEAD");
    }

    verify_head_images(wiki, journal, after)
}

fn verify_revision_tree_entry(
    wiki: &Wiki,
    revision: &str,
    entry: &JournalEntry,
    expected: &FileSnapshot,
) -> Result<()> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(&wiki.dir)
        .args(["ls-tree", "-z", revision, "--", &entry.relative_path]);
    let output = run_bounded_command(&mut command, Some(MAX_GIT_TREE_ENTRY_BYTES))
        .with_context(|| format!("verifying committed mode for {}", entry.relative_path))?;
    if output.stdout_truncated || output.stdout.len() > MAX_GIT_TREE_ENTRY_BYTES {
        bail!(
            "committed tree entry for {} is oversized",
            entry.relative_path
        );
    }
    if !output.status.success() {
        bail!(
            "cannot inspect committed mode for {}: {}",
            entry.relative_path,
            bounded_stderr(&output)
        );
    }
    if expected.content.is_none() {
        if !output.stdout.is_empty() {
            bail!(
                "committed history still contains deleted tree entry {}",
                entry.relative_path
            );
        }
        return Ok(());
    }
    if output.stdout.last() != Some(&0) || output.stdout[..output.stdout.len() - 1].contains(&0) {
        bail!(
            "committed tree entry for {} is malformed",
            entry.relative_path
        );
    }
    let record = &output.stdout[..output.stdout.len() - 1];
    let tab = record
        .iter()
        .position(|byte| *byte == b'\t')
        .context("committed tree entry has no path separator")?;
    if &record[tab + 1..] != entry.relative_path.as_bytes() {
        bail!(
            "committed tree entry path differs for {}",
            entry.relative_path
        );
    }
    let fields = record[..tab]
        .split(|byte| *byte == b' ')
        .collect::<Vec<_>>();
    if fields.len() != 3 || fields[1] != b"blob" || fields[2].is_empty() {
        bail!(
            "committed tree entry for {} is not a regular blob",
            entry.relative_path
        );
    }
    let executable = expected
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.unix_mode)
        .is_some_and(|mode| mode & 0o111 != 0);
    let expected_mode: &[u8] = if executable { b"100755" } else { b"100644" };
    if fields[0] != expected_mode {
        bail!(
            "committed tree mode differs for {} (expected {}, found {})",
            entry.relative_path,
            String::from_utf8_lossy(expected_mode),
            String::from_utf8_lossy(fields[0])
        );
    }
    Ok(())
}

fn verify_revision_images(
    wiki: &Wiki,
    journal: &PublishJournal,
    revision: &str,
    after: bool,
) -> Result<()> {
    for entry in &journal.entries {
        let object = format!("{revision}:{}", entry.relative_path);
        let mut command = Command::new("git");
        command
            .arg("-C")
            .arg(&wiki.dir)
            .args(["show", "--no-textconv", &object]);
        let blob_limit = usize::try_from(snapshot::MAX_SNAPSHOT_PAGE_BYTES)
            .context("snapshot page byte limit does not fit in memory")?;
        let output = run_bounded_command(&mut command, Some(blob_limit))
            .with_context(|| format!("verifying committed image for {}", entry.relative_path))?;
        let expected_snapshot = if after { &entry.after } else { &entry.before };
        verify_revision_tree_entry(wiki, revision, entry, expected_snapshot)?;
        if output.stdout_truncated || output.stdout.len() > blob_limit {
            bail!(
                "committed history content for {} exceeds the snapshot safety limit",
                entry.relative_path
            );
        }
        match &expected_snapshot.content {
            Some(expected) if !output.status.success() => bail!(
                "committed history is missing {}: {}",
                entry.relative_path,
                bounded_stderr(&output)
            ),
            Some(expected) if output.stdout != expected.as_bytes() => {
                bail!(
                    "committed history content differs for {}",
                    entry.relative_path
                )
            }
            None if output.status.success() => {
                bail!(
                    "committed history still contains deleted page {}",
                    entry.relative_path
                )
            }
            None | Some(_) => {}
        }
    }
    Ok(())
}

fn verify_head_images(wiki: &Wiki, journal: &PublishJournal, after: bool) -> Result<()> {
    verify_revision_images(wiki, journal, "HEAD", after)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RecoveryHistoryState {
    NoHistory,
    Uncommitted { base: String },
    Published { commit: String },
    RolledBack { commit: String },
}

fn current_head_required(wiki: &Wiki) -> Result<String> {
    current_wiki_revision(wiki)?.context("wiki Git history has no HEAD")
}

fn verify_commit_changed_paths(wiki: &Wiki, journal: &PublishJournal, commit: &str) -> Result<()> {
    let mut command = Command::new("git");
    command.arg("-C").arg(&wiki.dir).args([
        "diff-tree",
        "--no-commit-id",
        "--name-only",
        "--no-renames",
        "-r",
        "-z",
        "--root",
        commit,
    ]);
    let path_limit = usize::try_from(MAX_COMMIT_PATH_OUTPUT_BYTES)
        .context("publication path output limit does not fit in memory")?;
    let output = run_bounded_command(&mut command, Some(path_limit))
        .context("listing publication commit paths")?;
    if output.stdout_truncated {
        bail!(
            "publication commit path list exceeds the {MAX_COMMIT_PATH_OUTPUT_BYTES}-byte verification limit"
        );
    }
    if !output.status.success() {
        bail!(
            "cannot list publication commit paths: {}",
            bounded_stderr(&output)
        );
    }
    let raw = output.stdout;
    if raw.last() != Some(&0) {
        bail!("publication commit path list is not NUL-terminated");
    }
    let mut actual = BTreeSet::new();
    for path in raw[..raw.len() - 1].split(|byte| *byte == 0) {
        if path.is_empty() || !actual.insert(path.to_vec()) {
            bail!("publication commit path list is malformed or duplicated");
        }
    }
    let expected = journal
        .entries
        .iter()
        .map(|entry| entry.relative_path.as_bytes().to_vec())
        .collect::<BTreeSet<_>>();
    if actual != expected {
        bail!(
            "publication commit changed paths outside or missing from the reviewed journal (expected {}, found {})",
            expected.len(),
            actual.len()
        );
    }
    Ok(())
}

fn commit_parent(wiki: &Wiki, revision: &str) -> Result<String> {
    let parent_revision = format!("{revision}^");
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(&wiki.dir)
        .args(["rev-parse", "--verify", &parent_revision]);
    let parent = run_bounded_command(&mut command, Some(MAX_GIT_REVISION_OUTPUT_BYTES))
        .context("reading publication commit parent")?;
    if parent.stdout_truncated {
        bail!("publication commit parent output exceeds the safe byte limit");
    }
    if !parent.status.success() {
        bail!(
            "publication commit has no verifiable parent: {}",
            bounded_stderr(&parent)
        );
    }
    Ok(String::from_utf8_lossy(&parent.stdout).trim().to_string())
}

fn verify_commit_edge_at(
    wiki: &Wiki,
    revision: &str,
    expected_parent: &str,
    expected_message: &str,
) -> Result<String> {
    let mut resolve = Command::new("git");
    resolve
        .arg("-C")
        .arg(&wiki.dir)
        .args(["rev-parse", "--verify", revision]);
    let resolved = run_bounded_command(&mut resolve, Some(MAX_GIT_REVISION_OUTPUT_BYTES))
        .context("resolving publication commit")?;
    if resolved.stdout_truncated {
        bail!("publication commit resolution exceeds the safe byte limit");
    }
    if !resolved.status.success() {
        bail!(
            "publication commit cannot be resolved: {}",
            bounded_stderr(&resolved)
        );
    }
    let commit = String::from_utf8_lossy(&resolved.stdout).trim().to_string();
    if commit == expected_parent {
        bail!("expected a publication commit, but wiki HEAD did not advance");
    }
    if commit_parent(wiki, &commit)? != expected_parent {
        bail!("publication HEAD is not exactly one commit after its reviewed base");
    }
    let mut inspect = Command::new("git");
    inspect
        .arg("-C")
        .arg(&wiki.dir)
        .args(["cat-file", "commit", &commit]);
    let object = run_bounded_command(&mut inspect, Some(MAX_GIT_COMMIT_OBJECT_BYTES))
        .context("reading publication commit object")?;
    if object.stdout_truncated {
        bail!("publication commit object exceeds the safe byte limit");
    }
    if !object.status.success() {
        bail!(
            "cannot read publication commit object: {}",
            bounded_stderr(&object)
        );
    }
    let message = object
        .stdout
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|separator| &object.stdout[separator + 2..])
        .context("publication commit object has no message separator")?;
    let canonical = crate::history::canonical_commit_message(expected_message);
    let mut expected = canonical.into_bytes();
    if !expected.is_empty() {
        expected.push(b'\n');
    }
    if message != expected {
        bail!("publication commit message differs from the reviewed message");
    }
    Ok(commit)
}

fn verify_commit_edge(
    wiki: &Wiki,
    expected_parent: &str,
    expected_message: &str,
) -> Result<String> {
    verify_commit_edge_at(wiki, "HEAD", expected_parent, expected_message)
}

fn verify_expected_publish_commit(wiki: &Wiki, journal: &PublishJournal) -> Result<String> {
    let base = journal
        .pre_finalizer_head
        .as_deref()
        .context("publication journal has no pre-finalizer HEAD")?;
    let message = journal
        .commit_message
        .as_deref()
        .context("publication journal has no expected commit message")?;
    let commit = verify_commit_edge(wiki, base, message)?;
    verify_commit_changed_paths(wiki, journal, &commit)?;
    Ok(commit)
}

fn verify_pre_finalizer_head_unchanged(wiki: &Wiki, journal: &PublishJournal) -> Result<()> {
    let expected = journal
        .pre_finalizer_head
        .as_deref()
        .context("publication journal has no pre-finalizer HEAD")?;
    let actual = current_head_required(wiki)?;
    if actual != expected {
        bail!("wiki HEAD advanced from {expected} to {actual}");
    }
    Ok(())
}

fn classify_recovery_history(
    wiki: &Wiki,
    journal: &PublishJournal,
) -> Result<RecoveryHistoryState> {
    let Some(message) = journal.commit_message.as_deref() else {
        return Ok(RecoveryHistoryState::NoHistory);
    };
    let base = journal
        .pre_finalizer_head
        .as_deref()
        .context("publication journal has no pre-finalizer HEAD")?;
    let head = current_head_required(wiki)?;
    if head == base {
        return Ok(RecoveryHistoryState::Uncommitted {
            base: base.to_string(),
        });
    }
    let parent = commit_parent(wiki, &head)?;
    if parent == base {
        verify_commit_edge_at(wiki, &head, base, message)?;
        verify_commit_changed_paths(wiki, journal, &head)?;
        verify_revision_images(wiki, journal, &head, true)
            .context("published recovery commit does not contain the journal after-images")?;
        return Ok(RecoveryHistoryState::Published { commit: head });
    }

    // A crash can happen after a compensating rollback commit succeeds but
    // before the journal is removed. Recognize only the exact two-edge chain:
    // reviewed base -> reviewed publication -> Wookie rollback, with the
    // corresponding after/before images at each commit.
    let publish_commit = parent;
    verify_commit_edge_at(wiki, &publish_commit, base, message)
        .context("recovery history diverged before the rollback commit")?;
    verify_commit_changed_paths(wiki, journal, &publish_commit)
        .context("recovery publication commit changed unreviewed paths")?;
    verify_revision_images(wiki, journal, &publish_commit, true)
        .context("recovery publication commit does not contain the journal after-images")?;
    verify_commit_edge_at(
        wiki,
        &head,
        &publish_commit,
        &format!("wookie: recover rollback {}", journal.transaction_id),
    )
    .context("recovery history is not the exact compensating rollback")?;
    verify_commit_changed_paths(wiki, journal, &head)
        .context("recovery rollback commit changed paths outside the journal")?;
    verify_revision_images(wiki, journal, &head, false)
        .context("recovery rollback commit does not contain the journal before-images")?;
    Ok(RecoveryHistoryState::RolledBack { commit: head })
}

fn verify_recovery_rollback_lineage(
    wiki: &Wiki,
    journal: &PublishJournal,
    state: &RecoveryHistoryState,
) -> Result<()> {
    match state {
        RecoveryHistoryState::NoHistory => Ok(()),
        RecoveryHistoryState::Uncommitted { base } => {
            let head = current_head_required(wiki)?;
            if &head != base {
                bail!("rollback unexpectedly advanced history from {base} to {head}");
            }
            Ok(())
        }
        RecoveryHistoryState::Published { commit } => {
            let rollback = verify_commit_edge(
                wiki,
                commit,
                &format!("wookie: recover rollback {}", journal.transaction_id),
            )?;
            verify_commit_changed_paths(wiki, journal, &rollback)
        }
        RecoveryHistoryState::RolledBack { commit } => {
            let head = current_head_required(wiki)?;
            if &head != commit {
                bail!("rollback history changed after recovery validation");
            }
            Ok(())
        }
    }
}

fn remove_stale_ownerless_lock(path: &Path, stale_after: Duration) -> Result<()> {
    let first = fs::symlink_metadata(path)
        .with_context(|| format!("inspecting ownerless publication lock {}", path.display()))?;
    if first.file_type().is_symlink() || !first.is_dir() {
        bail!("publication lock must be a real directory");
    }
    if read_lock_owner(path)?.is_some() {
        bail!("publication lock gained owner metadata; refusing ownerless recovery");
    }
    let age = first
        .modified()
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .context("cannot establish the age of the ownerless publication lock")?;
    if age < stale_after {
        bail!(
            "ownerless publication lock is not stale yet (age {}s, requires {}s); retry after confirming no writer is starting",
            age.as_secs(),
            stale_after.as_secs()
        );
    }

    // An owner is normally written immediately after mkdir. Recheck after a
    // scheduling window and verify the directory itself was not replaced.
    std::thread::sleep(Duration::from_millis(50));
    let second = fs::symlink_metadata(path)
        .with_context(|| format!("rechecking ownerless publication lock {}", path.display()))?;
    if !same_lock_directory(&first, &second) || read_lock_owner(path)?.is_some() {
        bail!("publication lock changed during ownerless recovery; refusing removal");
    }
    fs::remove_dir(path).context("removing stale ownerless publication lock")
}

#[cfg(unix)]
fn same_lock_directory(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    right.is_dir()
        && !right.file_type().is_symlink()
        && left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.modified().ok() == right.modified().ok()
}

#[cfg(not(unix))]
fn same_lock_directory(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    right.is_dir()
        && !right.file_type().is_symlink()
        && left.modified().ok() == right.modified().ok()
        && left.created().ok() == right.created().ok()
}

fn remove_verified_lock(path: &Path, expected: &LockRecord, owner_path: &Path) -> Result<()> {
    let current = read_lock_record(owner_path)?;
    if &current != expected {
        bail!("lock ownership changed during recovery; refusing removal");
    }
    fs::remove_file(owner_path).context("removing stale lock owner")?;
    fs::remove_dir(path).context("removing stale lock directory")
}

/// Return the single valid owner record. An empty directory means lock
/// initialization was interrupted; any other malformed shape is rejected.
fn read_lock_owner(path: &Path) -> Result<Option<(LockRecord, PathBuf)>> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("publication lock must be a real directory");
    }
    let mut owner = None;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let entry_path = entry.path();
        let metadata = fs::symlink_metadata(&entry_path)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!("publication lock contains a non-regular owner entry");
        }
        if owner.is_some() {
            bail!("publication lock contains multiple owner entries");
        }
        let record = read_lock_record(&entry_path)?;
        let expected_name = format!("owner-{}-{}.json", record.pid, record.transaction_id);
        if entry.file_name().to_string_lossy() != expected_name {
            bail!("publication lock owner filename does not match its metadata");
        }
        owner = Some((record, entry_path));
    }
    Ok(owner)
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }
    let Ok(pid) = i32::try_from(pid) else {
        return true;
    };
    // SAFETY: signal 0 only checks process existence/permissions.
    if unsafe { kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().kind() == std::io::ErrorKind::PermissionDenied
}

#[cfg(windows)]
fn process_is_alive(pid: u32) -> bool {
    type Handle = *mut std::ffi::c_void;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const STILL_ACTIVE: u32 = 259;
    #[link(name = "Kernel32")]
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, process_id: u32) -> Handle;
        fn GetExitCodeProcess(process: Handle, exit_code: *mut u32) -> i32;
        fn CloseHandle(object: Handle) -> i32;
    }
    // SAFETY: initialized output storage is passed and every opened handle is
    // closed. Permission failures are treated conservatively as live.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return true;
        }
        let mut exit_code = 0;
        let success = GetExitCodeProcess(handle, &mut exit_code);
        let _ = CloseHandle(handle);
        success != 0 && exit_code == STILL_ACTIVE
    }
}

#[cfg(not(any(unix, windows)))]
fn process_is_alive(_pid: u32) -> bool {
    true
}

fn write_journal(wiki: &Wiki, journal: &PublishJournal) -> Result<()> {
    validate_journal(journal)?;
    let path = wiki.contained_path(Path::new(PUBLISH_JOURNAL_PATH))?;
    let bytes = serde_json::to_vec_pretty(journal)?;
    if bytes.len() as u64 > MAX_JOURNAL_BYTES {
        bail!("publication journal exceeds {MAX_JOURNAL_BYTES} bytes");
    }
    atomic_write_with_permissions(
        &path,
        bytes,
        Some(AtomicWritePermissions {
            readonly: false,
            #[cfg(unix)]
            unix_mode: Some(0o600),
            #[cfg(not(unix))]
            unix_mode: None,
        }),
    )
    .with_context(|| format!("writing {}", path.display()))
}

fn read_journal(path: &Path) -> Result<PublishJournal> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("no publication journal at {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("publication journal must be a regular file");
    }
    if metadata.len() > MAX_JOURNAL_BYTES {
        bail!("publication journal exceeds {MAX_JOURNAL_BYTES} bytes");
    }
    let raw = read_bounded_regular_file(path, MAX_JOURNAL_BYTES, "publication journal")?;
    let journal: PublishJournal = serde_json::from_slice(&raw)?;
    if journal.schema != JOURNAL_SCHEMA {
        bail!(
            "unsupported publication journal schema '{}'",
            journal.schema
        );
    }
    validate_journal(&journal)?;
    Ok(journal)
}

fn valid_sha256(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn validate_file_snapshot(snapshot: &FileSnapshot, label: &str) -> Result<()> {
    if snapshot.content.is_some() != snapshot.metadata.is_some() {
        bail!("{label} metadata does not match its content state");
    }
    if snapshot
        .content
        .as_ref()
        .is_some_and(|content| content.len() as u64 > snapshot::MAX_SNAPSHOT_PAGE_BYTES)
    {
        bail!("{label} exceeds the bounded snapshot size");
    }
    if snapshot
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.unix_mode)
        .is_some_and(|mode| mode > 0o7777)
    {
        bail!("{label} contains invalid Unix permissions");
    }
    Ok(())
}

fn validate_catalog_state(state: &CatalogState, label: &str) -> Result<()> {
    if !valid_sha256(&state.raw_content_sha256) {
        bail!("{label} has an invalid raw catalog SHA-256");
    }
    let mut fingerprints = Vec::with_capacity(state.pages.len());
    for (id, page) in &state.pages {
        validate_id(id)?;
        if !valid_sha256(&page.raw_sha256) {
            bail!("{label} page '{id}' has an invalid raw SHA-256");
        }
        if page.unix_mode().is_some_and(|mode| mode > 0o7777) {
            bail!("{label} page '{id}' has invalid Unix permissions");
        }
        fingerprints.push(snapshot::RawPageFingerprint {
            id: id.clone(),
            raw_sha256: page.raw_sha256.clone(),
        });
    }
    if snapshot::catalog_content_hash(&fingerprints)? != state.raw_content_sha256 {
        bail!("{label} raw catalog SHA-256 does not match its page entries");
    }
    Ok(())
}

impl CatalogPageState {
    fn unix_mode(&self) -> Option<u32> {
        self.metadata.unix_mode
    }
}

fn catalog_page_from_snapshot(snapshot: &FileSnapshot) -> Result<Option<CatalogPageState>> {
    validate_file_snapshot(snapshot, "journal page snapshot")?;
    Ok(match (&snapshot.content, &snapshot.metadata) {
        (Some(content), Some(metadata)) => Some(CatalogPageState {
            raw_sha256: snapshot::raw_page_sha256(content.as_bytes()),
            metadata: metadata.clone(),
        }),
        (None, None) => None,
        _ => unreachable!("validated snapshot content/metadata agreement"),
    })
}

fn validate_journal(journal: &PublishJournal) -> Result<()> {
    if journal.transaction_id.is_empty()
        || journal.transaction_id.len() > 256
        || !journal
            .transaction_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        bail!("publication journal has an invalid transaction id");
    }
    if journal.entries.is_empty()
        || journal.entries.len() > MAX_PUBLISH_PATHS
        || journal.applied > journal.entries.len()
    {
        bail!("publication journal has invalid entry counts");
    }
    if journal.state == RecoveryState::Prepared && journal.applied != 0 {
        bail!("prepared publication journal must not contain applied entries");
    }
    if journal.state == RecoveryState::Applied && journal.applied != journal.entries.len() {
        bail!("applied publication journal does not cover every entry");
    }
    let environment = &journal.environment;
    validate_catalog_state(&environment.catalog_before, "journal before-catalog")?;
    validate_catalog_state(&environment.catalog_after, "journal after-catalog")?;
    validate_file_snapshot(&environment.config_before, "journal before-configuration")?;
    validate_file_snapshot(&environment.config_after, "journal after-configuration")?;
    if environment.config_before != environment.config_after {
        bail!("publication journal cannot change wookie.toml");
    }
    if !valid_sha256(&environment.effective_policy_before_sha256)
        || !valid_sha256(&environment.effective_policy_after_sha256)
        || environment.effective_policy_before_sha256 != environment.effective_policy_after_sha256
    {
        bail!("publication journal has invalid or changing effective policy state");
    }
    if environment
        .unlock_controls_before
        .keys()
        .collect::<Vec<_>>()
        != environment.unlock_controls_after.keys().collect::<Vec<_>>()
    {
        bail!("publication journal lock-control paths differ before and after relock");
    }
    for (relative, before) in &environment.unlock_controls_before {
        let valid = relative == ".unlocks.toml"
            || relative
                .strip_prefix(".unlocks/")
                .and_then(|path| path.strip_suffix(".toml"))
                .is_some_and(|section| !section.contains('/') && validate_id(section).is_ok());
        if !valid {
            bail!("publication journal has an invalid lock-control path");
        }
        validate_file_snapshot(before, "journal before lock control")?;
        validate_file_snapshot(
            environment
                .unlock_controls_after
                .get(relative)
                .context("journal after lock control is missing")?,
            "journal after lock control",
        )?;
    }
    for section in &environment.relocked_rule_sections {
        validate_id(section)?;
        if section.contains('/') {
            bail!("journal relocked section must be one page-id segment");
        }
        let expected_path = format!(".unlocks/{section}.toml");
        let expected = environment
            .unlock_controls_after
            .get(&expected_path)
            .and_then(|snapshot| snapshot.content.as_deref());
        if expected != Some("locked = true\n") {
            bail!("journal relocked section lacks its canonical locked marker");
        }
    }
    if let Some(revision) = &journal.base_revision {
        if revision.is_empty()
            || revision.len() > MAX_REVISION_BYTES
            || revision.chars().any(char::is_control)
        {
            bail!("publication journal has an invalid base revision");
        }
    }
    if let Some(revision) = &journal.pre_finalizer_head {
        if revision.is_empty()
            || revision.len() > MAX_REVISION_BYTES
            || revision.chars().any(char::is_control)
        {
            bail!("publication journal has an invalid pre-finalizer HEAD");
        }
    }
    if let Some(message) = &journal.commit_message {
        if !publish_message_is_valid(message) {
            bail!("publication journal has an invalid recovery commit message");
        }
        if message != &crate::history::canonical_commit_message(message) {
            bail!("publication journal has a non-canonical recovery commit message");
        }
        if journal.pre_finalizer_head.is_none() {
            bail!("Git-backed publication journal is missing its pre-finalizer HEAD");
        }
    }
    let mut paths = BTreeSet::new();
    let mut path_bytes = 0_usize;
    for entry in &journal.entries {
        let Some(id) = entry
            .relative_path
            .strip_prefix("pages/")
            .and_then(|path| path.strip_suffix(".md"))
        else {
            bail!(
                "publication journal path is outside the page store: '{}'",
                entry.relative_path
            );
        };
        validate_id(id)?;
        if entry.relative_path != format!("pages/{id}.md") || !paths.insert(&entry.relative_path) {
            bail!(
                "publication journal contains an invalid or duplicate page path: '{}'",
                entry.relative_path
            );
        }
        path_bytes = path_bytes.saturating_add(entry.relative_path.len() + 1);
        if entry.before == entry.after {
            bail!(
                "publication journal contains a no-op page entry: '{}'",
                entry.relative_path
            );
        }
        for snapshot in [&entry.before, &entry.after] {
            validate_file_snapshot(snapshot, "publication journal page image")?;
        }
        if environment.catalog_before.pages.get(id)
            != catalog_page_from_snapshot(&entry.before)?.as_ref()
            || environment.catalog_after.pages.get(id)
                != catalog_page_from_snapshot(&entry.after)?.as_ref()
        {
            bail!(
                "publication journal page entry does not match its full catalog state: '{}'",
                entry.relative_path
            );
        }
    }
    if path_bytes > MAX_PUBLISH_PATH_ARG_BYTES {
        bail!("publication journal path arguments exceed the safe byte limit");
    }
    Ok(())
}

fn finish_recovery_history(
    wiki: &Wiki,
    journal: &PublishJournal,
    action: RecoveryAction,
    history_state: &RecoveryHistoryState,
) -> Result<()> {
    let Some(original_message) = journal.commit_message.as_deref() else {
        return Ok(());
    };
    let paths = journal
        .entries
        .iter()
        .map(|entry| entry.relative_path.clone())
        .collect::<Vec<_>>();
    if paths.is_empty() {
        bail!("refusing to finalize recovery history without page paths");
    }
    match (action, history_state) {
        (RecoveryAction::Accept, RecoveryHistoryState::Published { .. })
        | (RecoveryAction::Accept, RecoveryHistoryState::NoHistory) => return Ok(()),
        (RecoveryAction::Accept, RecoveryHistoryState::RolledBack { .. }) => {
            bail!("cannot accept a publication that has already been rolled back")
        }
        (RecoveryAction::Accept, RecoveryHistoryState::Uncommitted { base }) => {
            if &current_head_required(wiki)? != base {
                bail!("wiki HEAD changed after recovery lineage validation");
            }
        }
        (RecoveryAction::Rollback, RecoveryHistoryState::Published { commit }) => {
            if &current_head_required(wiki)? != commit {
                bail!("wiki HEAD changed after recovery lineage validation");
            }
        }
        (RecoveryAction::Rollback, RecoveryHistoryState::Uncommitted { base }) => {
            if &current_head_required(wiki)? != base {
                bail!("wiki HEAD changed after recovery lineage validation");
            }
        }
        (RecoveryAction::Rollback, RecoveryHistoryState::RolledBack { commit }) => {
            if &current_head_required(wiki)? != commit {
                bail!("wiki HEAD changed after recovery lineage validation");
            }
            return Ok(());
        }
        (RecoveryAction::Rollback, RecoveryHistoryState::NoHistory) => return Ok(()),
    }
    if action == RecoveryAction::Rollback {
        crate::history::reset_paths(&wiki.dir, &paths)
            .context("restoring the Git index during publication recovery")?;
    }
    let message = match action {
        RecoveryAction::Accept => original_message.to_string(),
        RecoveryAction::Rollback => format!("wookie: recover rollback {}", journal.transaction_id),
    };
    crate::history::commit_paths(&wiki.dir, &message, &paths, &wiki.history)
        .map(|_| ())
        .context("finalizing Git history during publication recovery")
}

fn remove_journal(wiki: &Wiki) -> Result<()> {
    let path = wiki.contained_path(Path::new(PUBLISH_JOURNAL_PATH))?;
    match fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("removing {}", path.display())),
    }
}

fn rollback_journal(wiki: &Wiki, journal: &mut PublishJournal) -> Result<()> {
    journal.state = RecoveryState::RollingBack;
    write_journal(wiki, journal)?;
    let mut errors = Vec::new();
    for entry in journal.entries.iter().rev() {
        let path = match contained_path(&wiki.dir, Path::new(&entry.relative_path)) {
            Ok(path) => path,
            Err(error) => {
                errors.push(format!("{}: {error:#}", entry.relative_path));
                continue;
            }
        };
        match snapshot_file(&path) {
            Ok(current) if current == entry.before => continue,
            Ok(current) if current == entry.after => {}
            Ok(_) => {
                errors.push(format!(
                    "{} changed outside the transaction; refusing to overwrite it",
                    entry.relative_path
                ));
                continue;
            }
            Err(error) => {
                errors.push(format!("{}: {error:#}", entry.relative_path));
                continue;
            }
        }
        if let Err(error) = write_snapshot(&wiki.dir, &path, &entry.before) {
            errors.push(format!("{}: {error:#}", entry.relative_path));
        }
    }
    if errors.is_empty() {
        journal.state = RecoveryState::Prepared;
        journal.applied = 0;
        write_journal(wiki, journal)
    } else {
        journal.state = RecoveryState::RollbackFailed;
        let _ = write_journal(wiki, journal);
        bail!("{}", errors.join("; "))
    }
}

fn write_snapshot(root: &Path, path: &Path, snapshot: &FileSnapshot) -> Result<()> {
    match &snapshot.content {
        Some(content) => {
            let relative = path
                .strip_prefix(root)
                .with_context(|| format!("{} escaped storage root", path.display()))?;
            if let Some(parent) = relative.parent() {
                create_contained_dir_all(root, parent)?;
            }
            let permissions = snapshot
                .metadata
                .as_ref()
                .map(|metadata| AtomicWritePermissions {
                    readonly: metadata.readonly,
                    unix_mode: metadata.unix_mode,
                });
            atomic_write_with_permissions(path, content, permissions)?;
            Ok(())
        }
        None => match fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| format!("inspecting {}", path.display())),
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
                bail!(
                    "refusing to delete non-regular managed file {}",
                    path.display()
                )
            }
            Ok(_) => fs::remove_file(path)
                .with_context(|| format!("deleting managed file {}", path.display())),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AuditSettings, HistorySettings, PublishSettings, RetrievalSettings, SessionSettings,
    };
    use crate::wiki::WikiConfig;

    fn fixture(label: &str) -> (PathBuf, Wiki) {
        let root = std::env::temp_dir().join(format!(
            "wookie-publish-{label}-{}-{}",
            std::process::id(),
            TRANSACTION_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("pages")).unwrap();
        let slug = root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap()
            .to_string();
        let config = WikiConfig {
            name: slug.clone(),
            description: String::new(),
            project_roots: Vec::new(),
            auto_commit: Some(false),
            sessions: Default::default(),
            history: Default::default(),
            retrieval: Default::default(),
            audit: Default::default(),
            publish: Default::default(),
            last_ingest_commit: None,
            sections: BTreeMap::new(),
        };
        fs::write(
            root.join("wookie.toml"),
            toml::to_string_pretty(&config).unwrap(),
        )
        .unwrap();
        let wiki = Wiki {
            slug,
            dir: root.clone(),
            config,
            auto_commit: false,
            sessions: SessionSettings::default(),
            history: HistorySettings::default(),
            retrieval: RetrievalSettings::default(),
            audit: AuditSettings::default(),
            publish: PublishSettings::default(),
        };
        (root, wiki)
    }

    fn write_page(wiki: &Wiki, id: &str, body: &str) {
        let mut page = new_page(id, body);
        wiki.save_page_raw(&mut page, false).unwrap();
    }

    #[test]
    fn published_pages_share_markdown_aware_description_derivation() {
        let page = new_page(
            "bold-summary",
            "**Compact lead.** Later detail must stay out of the description.",
        );
        assert_eq!(page.fm.description, "Compact lead.");
    }

    fn test_journal_environment(entries: &[JournalEntry]) -> JournalEnvironment {
        let mut before = BTreeMap::new();
        let mut after = BTreeMap::new();
        for entry in entries {
            let Some(id) = entry
                .relative_path
                .strip_prefix("pages/")
                .and_then(|path| path.strip_suffix(".md"))
                .filter(|id| validate_id(id).is_ok())
            else {
                continue;
            };
            if entry.before.content.is_some() {
                before.insert(id.to_string(), entry.before.clone());
            }
            if entry.after.content.is_some() {
                after.insert(id.to_string(), entry.after.clone());
            }
        }
        let policy = framed_sha256(b"wookie.test-effective-policy/v1", &[b"test"]);
        JournalEnvironment {
            catalog_before: catalog_state(&before).unwrap(),
            catalog_after: catalog_state(&after).unwrap(),
            config_before: FileSnapshot::default(),
            config_after: FileSnapshot::default(),
            effective_policy_before_sha256: policy.clone(),
            effective_policy_after_sha256: policy,
            unlock_controls_before: BTreeMap::new(),
            unlock_controls_after: BTreeMap::new(),
            relocked_rule_sections: BTreeSet::new(),
        }
    }

    fn recovery_journal_environment(wiki: &Wiki, entries: &[JournalEntry]) -> JournalEnvironment {
        let current = snapshot_catalog(wiki).unwrap();
        let mut before = current.clone();
        let mut after = current;
        for entry in entries {
            let id = entry
                .relative_path
                .strip_prefix("pages/")
                .and_then(|path| path.strip_suffix(".md"))
                .unwrap();
            for (catalog, snapshot) in [(&mut before, &entry.before), (&mut after, &entry.after)] {
                if snapshot.content.is_some() {
                    catalog.insert(id.to_string(), snapshot.clone());
                } else {
                    catalog.remove(id);
                }
            }
        }
        let config = snapshot_file(&wiki.dir.join("wookie.toml")).unwrap();
        let policy = current_effective_publish_policy_sha256(wiki).unwrap();
        let controls = snapshot_unlock_controls(wiki).unwrap();
        JournalEnvironment {
            catalog_before: catalog_state(&before).unwrap(),
            catalog_after: catalog_state(&after).unwrap(),
            config_before: config.clone(),
            config_after: config,
            effective_policy_before_sha256: policy.clone(),
            effective_policy_after_sha256: policy,
            unlock_controls_before: controls.clone(),
            unlock_controls_after: controls,
            relocked_rule_sections: BTreeSet::new(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn bounded_read_only_command_kills_oversized_stdout() {
        let mut command = Command::new("sh");
        command.args([
            "-c",
            "while :; do printf '0123456789abcdef0123456789abcdef'; done",
        ]);
        let started = Instant::now();
        let output = run_bounded_command(&mut command, Some(1024)).unwrap();

        assert!(output.stdout_truncated);
        assert_eq!(output.stdout.len(), 1024);
        assert!(!output.status.success());
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn preflight_materializes_moves_and_backlinks() {
        let (root, wiki) = fixture("move");
        write_page(&wiki, "old", "**Old.**\n");
        write_page(&wiki, "ref", "**Reference.** See [[old|the old page]].\n");
        let mut changes = ChangeSet::new(Some("abc".to_string()));
        changes.push(Change::Move {
            from: "old".to_string(),
            to: "new".to_string(),
            rewrite_links: true,
        });

        let checked = preflight(&wiki, &changes, Some("abc"), Snapshot::new("test")).unwrap();
        assert!(checked.is_publishable());
        assert!(checked.overlay.contains("new"));
        assert!(!checked.overlay.contains("old"));
        assert!(checked.overlay.page("ref").unwrap().body.contains("[[new|"));
        assert_eq!(checked.plan.operations.len(), 3);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn base_conflict_is_a_machine_readable_preflight_error() {
        let (root, wiki) = fixture("conflict");
        let changes = ChangeSet::new(Some("expected".to_string()));
        let checked = preflight(&wiki, &changes, Some("actual"), Snapshot::new("test")).unwrap();
        assert!(checked.report.has_errors());
        assert!(checked
            .report
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == code::PUBLISH_CONFLICT));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn change_sets_parse_from_toml_and_reject_unknown_fields() {
        let raw = r#"
schema = "wookie.changeset/v1"
base_revision = "abc"

[[changes]]
op = "create"
id = "guides/example"
body = "**Example.**"
"#;
        let parsed = ChangeSet::parse(raw).unwrap();
        assert_eq!(parsed.changes.len(), 1);
        assert!(ChangeSet::parse(&raw.replace("body =", "unknown = 1\nbody =")).is_err());
    }

    #[test]
    fn checked_diff_is_available_but_not_embedded_in_compact_plan() {
        let (root, wiki) = fixture("diff");
        write_page(&wiki, "page", "**Before.**\n");
        let mut changes = ChangeSet::new(Some("abc".to_string()));
        changes.push(Change::Update {
            id: "page".to_string(),
            body: Some("**After.**\n".to_string()),
            metadata: MetadataPatch::default(),
        });
        let checked = preflight(&wiki, &changes, Some("abc"), Snapshot::new("test")).unwrap();

        let rendered = checked.render_human(true);
        assert!(rendered.contains("-**Before.**"));
        assert!(rendered.contains("+**After.**"));
        assert!(!serde_json::to_string(&checked.plan)
            .unwrap()
            .contains("Before"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn no_op_update_is_not_publishable_or_journalable() {
        let (root, wiki) = fixture("no-op-update");
        write_page(&wiki, "page", "**Unchanged.**\n");
        let existing = wiki.load_page("page").unwrap().body;
        let mut changes = ChangeSet::new(Some("abc".to_string()));
        changes.push(Change::Update {
            id: "page".to_string(),
            body: Some(existing),
            metadata: MetadataPatch::default(),
        });

        let checked = preflight(&wiki, &changes, Some("abc"), Snapshot::new("test")).unwrap();
        assert!(checked.report.has_errors());
        assert!(checked.plan.operations.is_empty());
        assert!(checked
            .report
            .diagnostics
            .iter()
            .any(|item| item.message.contains("no effective page operations")));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rendered_page_limit_includes_frontmatter_and_blocks_oversized_images() {
        let (root, wiki) = fixture("rendered-page-limit");
        let limit = usize::try_from(snapshot::MAX_SNAPSHOT_PAGE_BYTES).unwrap();
        let prefix = "**Boundary.**\n\n";
        let probe = format!("{prefix}x");
        let overhead = new_page("boundary", &probe).render().len() - probe.len();
        let mut exact_body = prefix.to_string();
        exact_body.push_str(&"x".repeat(limit - overhead - prefix.len()));
        assert_eq!(
            new_page("boundary", &exact_body).render().len(),
            limit,
            "test fixture must include frontmatter in the exact boundary"
        );

        let mut exact = ChangeSet::new(Some("abc".to_string()));
        exact.push(Change::Create {
            id: "boundary".to_string(),
            body: exact_body.clone(),
            metadata: MetadataPatch::default(),
        });
        let checked = preflight(&wiki, &exact, Some("abc"), Snapshot::new("test")).unwrap();
        assert!(checked.is_publishable());
        assert_eq!(checked.plan.operations.len(), 1);
        assert_eq!(
            checked.deltas[0].after.content.as_ref().unwrap().len(),
            limit
        );

        let mut oversized_body = exact_body;
        oversized_body.push('x');
        let mut oversized = ChangeSet::new(Some("abc".to_string()));
        oversized.push(Change::Create {
            id: "boundary".to_string(),
            body: oversized_body,
            metadata: MetadataPatch::default(),
        });
        let checked = preflight(&wiki, &oversized, Some("abc"), Snapshot::new("test")).unwrap();
        assert!(checked.report.has_errors());
        assert!(checked.plan.operations.is_empty());
        assert!(checked.deltas.is_empty());
        assert!(checked.report.diagnostics.iter().any(|diagnostic| {
            diagnostic.page.as_deref() == Some("boundary")
                && diagnostic.message.contains("exceeding")
                && diagnostic.message.contains("page limit")
        }));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn publish_plan_and_report_share_the_raw_sha256_catalog_identity() {
        let (root, wiki) = fixture("catalog-sha256");
        write_page(&wiki, "existing", "**Existing.**\n");
        let mut changes = ChangeSet::new(Some("abc".to_string()));
        changes.push(Change::Create {
            id: "new".to_string(),
            body: "**New.**\n".to_string(),
            metadata: MetadataPatch::default(),
        });
        let checked = preflight(
            &wiki,
            &changes,
            Some("abc"),
            Snapshot::new("test").wiki_content_hash("obsolete-producer-hash"),
        )
        .unwrap();
        let hash = &checked.plan.observed_content_hash;
        assert!(hash.starts_with("sha256:"), "{hash}");
        assert_eq!(hash.len(), "sha256:".len() + 64);
        assert_eq!(
            checked.report.snapshot.wiki.content_hash.as_ref(),
            Some(hash)
        );
        assert_eq!(raw_catalog_sha256(&wiki).unwrap(), *hash);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn transaction_restores_page_and_metadata_when_finalizer_fails() {
        let (root, wiki) = fixture("rollback");
        write_page(&wiki, "page", "**Before.**\n");
        let path = wiki.page_path("page").unwrap();
        let before = fs::read_to_string(&path).unwrap();
        let mut changes = ChangeSet::new(Some("abc".to_string()));
        changes.push(Change::Update {
            id: "page".to_string(),
            body: Some("**After.**\n".to_string()),
            metadata: MetadataPatch {
                tags: Some(vec!["changed".to_string()]),
                ..Default::default()
            },
        });
        let checked = preflight(&wiki, &changes, Some("abc"), Snapshot::new("test")).unwrap();
        let error = transact(&wiki, checked, Some("abc"), |_| bail!("commit failed"))
            .unwrap_err()
            .to_string();

        assert!(error.contains("rolled back"));
        assert_eq!(fs::read_to_string(path).unwrap(), before);
        assert!(!root.join(PUBLISH_JOURNAL_PATH).exists());
        assert!(!root.join(PUBLISH_LOCK_PATH).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn transaction_removes_created_page_when_finalizer_fails() {
        let (root, wiki) = fixture("rollback-create");
        let mut changes = ChangeSet::new(Some("abc".to_string()));
        changes.push(Change::Create {
            id: "created".to_string(),
            body: "**Created.**\n".to_string(),
            metadata: MetadataPatch::default(),
        });
        let checked = preflight(&wiki, &changes, Some("abc"), Snapshot::new("test")).unwrap();
        let error = transact(&wiki, checked, Some("abc"), |_| bail!("commit failed"))
            .unwrap_err()
            .to_string();

        assert!(error.contains("rolled back"), "{error}");
        assert!(!wiki.exists("created"));
        assert!(!root.join(PUBLISH_JOURNAL_PATH).exists());
        assert!(!root.join(PUBLISH_LOCK_PATH).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn finalizer_image_drift_retains_the_journal_without_implicit_rollback() {
        let (root, wiki) = fixture("finalizer-image-drift");
        write_page(&wiki, "page", "**Before.**\n");
        let mut changes = ChangeSet::new(Some("abc".to_string()));
        changes.push(Change::Update {
            id: "page".to_string(),
            body: Some("**Reviewed after image.**\n".to_string()),
            metadata: MetadataPatch::default(),
        });
        let checked = preflight(&wiki, &changes, Some("abc"), Snapshot::new("test")).unwrap();
        let error = transact(&wiki, checked, Some("abc"), |_| {
            fs::write(wiki.page_path("page")?, "**Hook-mutated image.**\n")?;
            Ok(())
        })
        .unwrap_err()
        .to_string();

        assert!(error.contains("journal retained"), "{error}");
        assert!(wiki
            .load_page("page")
            .unwrap()
            .body
            .contains("Hook-mutated"));
        assert!(root.join(PUBLISH_JOURNAL_PATH).is_file());
        assert!(!root.join(PUBLISH_LOCK_PATH).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn panic_after_apply_retains_journal_and_after_image() {
        let (root, wiki) = fixture("panic-after-apply");
        write_page(&wiki, "page", "**Before.**\n");
        let mut changes = ChangeSet::new(Some("abc".to_string()));
        changes.push(Change::Update {
            id: "page".to_string(),
            body: Some("**Applied before panic.**\n".to_string()),
            metadata: MetadataPatch::default(),
        });
        let checked = preflight(&wiki, &changes, Some("abc"), Snapshot::new("test")).unwrap();
        let mut transaction = PublishTransaction::begin(&wiki, checked, Some("abc")).unwrap();
        transaction.apply().unwrap();

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _transaction = transaction;
            panic!("simulated finalizer panic");
        }));

        assert!(panic.is_err());
        assert!(wiki
            .load_page("page")
            .unwrap()
            .body
            .contains("Applied before panic"));
        assert!(root.join(PUBLISH_JOURNAL_PATH).is_file());
        assert!(!root.join(PUBLISH_LOCK_PATH).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn ordinary_mutation_refuses_any_interrupted_publish_journal() {
        for (label, directory) in [("malformed", false), ("nonregular", true)] {
            let (root, wiki) = fixture(&format!("journal-guard-{label}"));
            let path = root.join(PUBLISH_JOURNAL_PATH);
            if directory {
                fs::create_dir(&path).unwrap();
            } else {
                fs::write(&path, "not valid json").unwrap();
            }
            let error = acquire_mutation_guard(&wiki)
                .err()
                .expect("journal must block ordinary mutation")
                .to_string();
            assert!(error.contains("explicit"), "{error}");
            assert!(!root.join(PUBLISH_LOCK_PATH).exists());
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn dropping_an_unapplied_transaction_cleans_its_journal() {
        let (root, wiki) = fixture("drop-prepared");
        let mut changes = ChangeSet::new(Some("abc".to_string()));
        changes.push(Change::Create {
            id: "page".to_string(),
            body: "**Page.**\n".to_string(),
            metadata: MetadataPatch::default(),
        });
        let checked = preflight(&wiki, &changes, Some("abc"), Snapshot::new("test")).unwrap();
        let transaction = PublishTransaction::begin(&wiki, checked, Some("abc")).unwrap();
        assert!(recovery_status(&wiki).unwrap().is_some());
        drop(transaction);

        assert!(recovery_status(&wiki).unwrap().is_none());
        assert!(!wiki.exists("page"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn shared_mutation_guard_excludes_publishers_and_releases() {
        let (root, wiki) = fixture("shared-guard");
        let guard = acquire_mutation_guard(&wiki).unwrap();
        assert!(root.join(PUBLISH_LOCK_PATH).is_dir());
        let error = PublishLock::acquire(&wiki, LockPurpose::Publisher)
            .err()
            .expect("publisher must be excluded")
            .to_string();
        assert!(error.contains("publication lock"), "{error}");
        assert!(recover(&wiki, RecoveryAction::Rollback, true).is_err());
        drop(guard);
        assert!(!root.join(PUBLISH_LOCK_PATH).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mutation_waiter_stops_when_the_wiki_directory_moves() {
        let (root, wiki) = fixture("waiter-moved-root");
        let guard = acquire_mutation_guard(&wiki).unwrap();
        let moved = root.with_file_name(format!(
            "{}-moved",
            root.file_name().unwrap().to_string_lossy()
        ));
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            acquire_mutation_guard(&wiki)
                .map(drop)
                .map_err(|error| error.to_string())
        });
        started_rx.recv().unwrap();
        std::thread::sleep(Duration::from_millis(50));
        fs::rename(&root, &moved).unwrap();

        let error = waiter.join().unwrap().unwrap_err();
        assert!(error.contains("moved or was removed"), "{error}");
        drop(guard);
        fs::remove_dir_all(moved).unwrap();
    }

    #[test]
    fn forced_rollback_can_clear_a_verified_dead_mutation_lock_without_a_journal() {
        let (root, wiki) = fixture("dead-mutation-guard");
        let lock_path = root.join(PUBLISH_LOCK_PATH);
        fs::create_dir(&lock_path).unwrap();
        let record = LockRecord {
            schema: LOCK_SCHEMA.to_string(),
            transaction_id: "dead-mutation".to_string(),
            // Platform PID limits are far below i32::MAX.
            pid: i32::MAX as u32,
            thread_id: String::new(),
            created_at: chrono::Utc::now().to_rfc3339(),
            purpose: LockPurpose::Mutation,
        };
        let owner_path = lock_path.join(format!(
            "owner-{}-{}.json",
            record.pid, record.transaction_id
        ));
        fs::write(&owner_path, serde_json::to_vec(&record).unwrap()).unwrap();

        assert!(recover(&wiki, RecoveryAction::Accept, true).is_err());
        assert!(lock_path.exists());
        recover(&wiki, RecoveryAction::Rollback, true).unwrap();
        assert!(!lock_path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn forced_rollback_conservatively_clears_a_stale_ownerless_lock() {
        let (root, mut wiki) = fixture("ownerless-lock");
        wiki.history.lock_stale_seconds = 0;
        let lock_path = root.join(PUBLISH_LOCK_PATH);
        fs::create_dir(&lock_path).unwrap();

        let normal = recover(&wiki, RecoveryAction::Rollback, false)
            .unwrap_err()
            .to_string();
        assert!(normal.contains("force is required"), "{normal}");
        assert!(lock_path.exists());

        recover(&wiki, RecoveryAction::Rollback, true).unwrap();
        assert!(!lock_path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recovery_rejects_oversized_lock_owner_metadata_without_removing_it() {
        let (root, wiki) = fixture("oversized-lock-owner");
        let lock_path = root.join(PUBLISH_LOCK_PATH);
        fs::create_dir(&lock_path).unwrap();
        let owner_path = lock_path.join("owner-1-oversized.json");
        let file = fs::File::create(&owner_path).unwrap();
        file.set_len(MAX_LOCK_OWNER_BYTES + 1).unwrap();

        let error = recover(&wiki, RecoveryAction::Rollback, true)
            .unwrap_err()
            .to_string();

        assert!(error.contains("byte limit"), "{error}");
        assert!(owner_path.is_file());
        assert!(lock_path.is_dir());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn recovery_rejects_symlinked_lock_owner_metadata_without_removing_it() {
        use std::os::unix::fs::symlink;

        let (root, wiki) = fixture("symlink-lock-owner");
        let lock_path = root.join(PUBLISH_LOCK_PATH);
        fs::create_dir(&lock_path).unwrap();
        let target = root.join("outside-owner.json");
        fs::write(&target, "{}").unwrap();
        let owner_path = lock_path.join("owner-1-symlink.json");
        symlink(&target, &owner_path).unwrap();

        let error = recover(&wiki, RecoveryAction::Rollback, true)
            .unwrap_err()
            .to_string();

        assert!(error.contains("non-regular owner"), "{error}");
        assert!(owner_path.is_symlink());
        assert_eq!(fs::read_to_string(&target).unwrap(), "{}");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn begin_rejects_any_catalog_change_after_preflight() {
        let (root, wiki) = fixture("catalog-conflict");
        write_page(&wiki, "existing", "**Before.**\n");
        let mut changes = ChangeSet::new(Some("abc".to_string()));
        changes.push(Change::Create {
            id: "new".to_string(),
            body: "**New.**\n".to_string(),
            metadata: MetadataPatch::default(),
        });
        let checked = preflight(&wiki, &changes, Some("abc"), Snapshot::new("test")).unwrap();
        write_page(&wiki, "existing", "**Changed concurrently.**\n");

        let error = PublishTransaction::begin(&wiki, checked, Some("abc"))
            .err()
            .expect("catalog change should conflict")
            .to_string();
        assert!(error.contains("catalog changed"), "{error}");
        assert!(!root.join(PUBLISH_LOCK_PATH).exists());
        assert!(!root.join(PUBLISH_JOURNAL_PATH).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn begin_rejects_config_change_after_preflight() {
        let (root, wiki) = fixture("config-conflict");
        let config_path = root.join("wookie.toml");
        fs::write(&config_path, "name = \"before\"\n").unwrap();
        let mut changes = ChangeSet::new(Some("abc".to_string()));
        changes.push(Change::Create {
            id: "new".to_string(),
            body: "**New.**\n".to_string(),
            metadata: MetadataPatch::default(),
        });
        let checked = preflight(&wiki, &changes, Some("abc"), Snapshot::new("test")).unwrap();
        fs::write(&config_path, "name = \"changed concurrently\"\n").unwrap();

        let error = PublishTransaction::begin(&wiki, checked, Some("abc"))
            .err()
            .expect("config change should conflict")
            .to_string();
        assert!(error.contains("wookie.toml changed"), "{error}");
        assert!(!root.join(PUBLISH_LOCK_PATH).exists());
        assert!(!root.join(PUBLISH_JOURNAL_PATH).exists());
        assert!(!wiki.exists("new"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn journal_commit_messages_match_change_set_validation() {
        for message in ["", "subject\n\nbody", "subject\nbody\tvalue"] {
            let change_set = ChangeSet {
                schema: CHANGESET_SCHEMA.to_string(),
                base_revision: None,
                message: Some(message.to_string()),
                changes: Vec::new(),
            };
            assert!(change_set.validate_schema().is_ok());

            let entries = vec![JournalEntry {
                relative_path: "pages/example.md".to_string(),
                before: FileSnapshot::default(),
                after: FileSnapshot {
                    content: Some("**Example.**\n".to_string()),
                    metadata: Some(default_file_metadata()),
                },
            }];
            let journal = PublishJournal {
                schema: JOURNAL_SCHEMA.to_string(),
                transaction_id: "valid-transaction".to_string(),
                base_revision: None,
                pre_finalizer_head: Some("abc".to_string()),
                commit_message: Some(message.to_string()),
                environment: test_journal_environment(&entries),
                state: RecoveryState::Prepared,
                applied: 0,
                entries,
            };
            assert!(validate_journal(&journal).is_ok());
        }

        let entries = vec![JournalEntry {
            relative_path: "pages/example.md".to_string(),
            before: FileSnapshot::default(),
            after: FileSnapshot {
                content: Some("**Example.**\n".to_string()),
                metadata: Some(default_file_metadata()),
            },
        }];
        let noncanonical = PublishJournal {
            schema: JOURNAL_SCHEMA.to_string(),
            transaction_id: "noncanonical-message".to_string(),
            base_revision: None,
            pre_finalizer_head: Some("abc".to_string()),
            commit_message: Some("subject\n".to_string()),
            environment: test_journal_environment(&entries),
            state: RecoveryState::Prepared,
            applied: 0,
            entries,
        };
        let error = validate_journal(&noncanonical).unwrap_err().to_string();
        assert!(error.contains("non-canonical"), "{error}");

        for invalid in [
            "contains\0nul",
            "terminal\u{001b}escape",
            "bidi\u{202e}override",
            "carriage\rreturn",
        ] {
            let change_set = ChangeSet {
                schema: CHANGESET_SCHEMA.to_string(),
                base_revision: None,
                message: Some(invalid.to_string()),
                changes: Vec::new(),
            };
            assert!(change_set.validate_schema().is_err());
            let entries = vec![JournalEntry {
                relative_path: "pages/example.md".to_string(),
                before: FileSnapshot::default(),
                after: FileSnapshot {
                    content: Some("**Example.**\n".to_string()),
                    metadata: Some(default_file_metadata()),
                },
            }];
            let journal = PublishJournal {
                schema: JOURNAL_SCHEMA.to_string(),
                transaction_id: "valid-transaction".to_string(),
                base_revision: None,
                pre_finalizer_head: Some("abc".to_string()),
                commit_message: Some(invalid.to_string()),
                environment: test_journal_environment(&entries),
                state: RecoveryState::Prepared,
                applied: 0,
                entries,
            };
            assert!(validate_journal(&journal).is_err());
        }
    }

    #[test]
    fn journal_rejects_an_empty_entry_set() {
        let entries = Vec::new();
        let journal = PublishJournal {
            schema: JOURNAL_SCHEMA.to_string(),
            transaction_id: "empty-journal".to_string(),
            base_revision: None,
            pre_finalizer_head: Some("abc".to_string()),
            commit_message: Some("must not commit the whole wiki".to_string()),
            environment: test_journal_environment(&entries),
            state: RecoveryState::Prepared,
            applied: 0,
            entries,
        };
        let error = validate_journal(&journal).unwrap_err().to_string();
        assert!(error.contains("entry counts"), "{error}");
    }

    #[test]
    fn journal_rejects_forged_non_page_path() {
        let metadata = FileMetadata {
            readonly: false,
            unix_mode: None,
        };
        let entries = vec![JournalEntry {
            relative_path: ".git/config".to_string(),
            before: FileSnapshot::default(),
            after: FileSnapshot {
                content: Some("forged".to_string()),
                metadata: Some(metadata),
            },
        }];
        let journal = PublishJournal {
            schema: JOURNAL_SCHEMA.to_string(),
            transaction_id: "forged-path".to_string(),
            base_revision: None,
            pre_finalizer_head: None,
            commit_message: None,
            environment: test_journal_environment(&entries),
            state: RecoveryState::Prepared,
            applied: 0,
            entries,
        };

        let error = validate_journal(&journal).unwrap_err().to_string();
        assert!(error.contains("outside the page store"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_replacement_installs_content_and_mode_together() {
        use std::os::unix::fs::PermissionsExt;

        let (root, wiki) = fixture("atomic-snapshot-mode");
        let path = wiki.page_path("mode-test").unwrap();
        fs::write(&path, "old").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o400)).unwrap();
        let snapshot = FileSnapshot {
            content: Some("new".to_string()),
            metadata: Some(FileMetadata {
                readonly: false,
                unix_mode: Some(0o640),
            }),
        };

        write_snapshot(&root, &path, &snapshot).unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
            0o640
        );
        assert_eq!(snapshot_file(&path).unwrap(), snapshot);
        fs::remove_dir_all(root).unwrap();
    }

    fn stage_applied_create_journal(wiki: &Wiki, id: &str, transaction_id: &str) {
        let relative_path = format!("pages/{id}.md");
        let after = FileSnapshot {
            content: Some("**Recovered create.**\n".to_string()),
            metadata: Some(default_file_metadata()),
        };
        let path = contained_path(&wiki.dir, Path::new(&relative_path)).unwrap();
        write_snapshot(&wiki.dir, &path, &after).unwrap();
        let entries = vec![JournalEntry {
            relative_path,
            before: FileSnapshot::default(),
            after,
        }];
        let journal = PublishJournal {
            schema: JOURNAL_SCHEMA.to_string(),
            transaction_id: transaction_id.to_string(),
            base_revision: None,
            pre_finalizer_head: None,
            commit_message: None,
            environment: recovery_journal_environment(wiki, &entries),
            state: RecoveryState::Applied,
            applied: 1,
            entries,
        };
        write_journal(wiki, &journal).unwrap();
    }

    fn git_ok(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_head(root: &Path) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn stage_git_backed_applied_update_journal(
        wiki: &mut Wiki,
        id: &str,
        transaction_id: &str,
        message: &str,
    ) -> (String, String) {
        wiki.auto_commit = true;
        let path = wiki.page_path(id).unwrap();
        fs::write(&path, "**Before recovery.**\n").unwrap();
        git_ok(&wiki.dir, &["init", "-q"]);
        git_ok(&wiki.dir, &["add", "--", &format!("pages/{id}.md")]);
        git_ok(
            &wiki.dir,
            &[
                "-c",
                "user.name=wookie",
                "-c",
                "user.email=wookie@localhost",
                "commit",
                "-q",
                "--cleanup=verbatim",
                "-m",
                "initial",
                "--",
                &format!("pages/{id}.md"),
            ],
        );
        let base = git_head(&wiki.dir);
        let before = snapshot_file(&path).unwrap();
        let after = FileSnapshot {
            content: Some("**Accepted recovery image.**\n".to_string()),
            metadata: before.metadata.clone(),
        };
        write_snapshot(&wiki.dir, &path, &after).unwrap();
        let relative_path = format!("pages/{id}.md");
        let message = crate::history::canonical_commit_message(message);
        let entries = vec![JournalEntry {
            relative_path,
            before,
            after,
        }];
        let journal = PublishJournal {
            schema: JOURNAL_SCHEMA.to_string(),
            transaction_id: transaction_id.to_string(),
            base_revision: Some(base.clone()),
            pre_finalizer_head: Some(base.clone()),
            commit_message: Some(message.clone()),
            environment: recovery_journal_environment(wiki, &entries),
            state: RecoveryState::Applied,
            applied: 1,
            entries,
        };
        write_journal(wiki, &journal).unwrap();
        (base, message)
    }

    #[test]
    fn recovery_accepts_an_applied_create_journal() {
        let (root, wiki) = fixture("recover-accept-create");
        stage_applied_create_journal(&wiki, "created", "recover-accept-create");

        recover(&wiki, RecoveryAction::Accept, false).unwrap();

        assert!(wiki.exists("created"));
        assert!(!root.join(PUBLISH_JOURNAL_PATH).exists());
        assert!(!root.join(PUBLISH_LOCK_PATH).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recovery_accept_rejects_divergent_head_before_any_history_mutation() {
        let (root, mut wiki) = fixture("recover-accept-divergent-head");
        let (_base, _message) = stage_git_backed_applied_update_journal(
            &mut wiki,
            "page",
            "recover-divergent-head",
            "reviewed publication",
        );
        fs::write(root.join("unrelated"), "unrelated\n").unwrap();
        git_ok(&root, &["add", "--", "unrelated"]);
        git_ok(
            &root,
            &[
                "-c",
                "user.name=wookie",
                "-c",
                "user.email=wookie@localhost",
                "commit",
                "-q",
                "-m",
                "unrelated commit",
                "--",
                "unrelated",
            ],
        );
        let divergent = git_head(&root);

        let error = recover(&wiki, RecoveryAction::Accept, false)
            .unwrap_err()
            .to_string();

        assert!(
            error.contains("message differs") || error.contains("reviewed base"),
            "{error}"
        );
        assert_eq!(
            git_head(&root),
            divergent,
            "recovery mutated divergent history"
        );
        assert!(root.join(PUBLISH_JOURNAL_PATH).is_file());
        assert!(wiki
            .load_page("page")
            .unwrap()
            .body
            .contains("Accepted recovery image"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recovery_accept_of_exact_published_child_is_verification_only() {
        let (root, mut wiki) = fixture("recover-accept-published");
        let (_base, message) = stage_git_backed_applied_update_journal(
            &mut wiki,
            "page",
            "recover-published-child",
            "Subject\n# kept comment\ntrailing spaces  \r\n\n",
        );
        assert!(crate::history::commit_paths(
            &root,
            &message,
            &["pages/page.md".to_string()],
            &wiki.history,
        )
        .unwrap());
        let published = git_head(&root);

        recover(&wiki, RecoveryAction::Accept, false).unwrap();

        assert_eq!(git_head(&root), published, "accept created an extra commit");
        assert!(!root.join(PUBLISH_JOURNAL_PATH).exists());
        assert!(wiki
            .load_page("page")
            .unwrap()
            .body
            .contains("Accepted recovery image"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recovery_rollback_is_idempotent_after_compensating_commit() {
        let (root, mut wiki) = fixture("recover-rollback-idempotent");
        let (_base, message) = stage_git_backed_applied_update_journal(
            &mut wiki,
            "page",
            "recover-rollback-idempotent",
            "reviewed publication",
        );
        assert!(crate::history::commit_paths(
            &root,
            &message,
            &["pages/page.md".to_string()],
            &wiki.history,
        )
        .unwrap());
        let mut journal = read_journal(&root.join(PUBLISH_JOURNAL_PATH)).unwrap();

        // Simulate a crash in `recover` after the filesystem/history rollback
        // succeeds but before its caller removes the durable journal.
        recover_locked_journal(&wiki, &mut journal, RecoveryAction::Rollback).unwrap();
        let rolled_back = git_head(&root);
        assert!(root.join(PUBLISH_JOURNAL_PATH).is_file());
        assert!(wiki
            .load_page("page")
            .unwrap()
            .body
            .contains("Before recovery"));

        recover(&wiki, RecoveryAction::Rollback, false).unwrap();

        assert_eq!(
            git_head(&root),
            rolled_back,
            "retry created a second rollback commit"
        );
        assert!(!root.join(PUBLISH_JOURNAL_PATH).exists());
        assert!(wiki
            .load_page("page")
            .unwrap()
            .body
            .contains("Before recovery"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn forced_recovery_can_clear_a_verified_dead_mismatched_waiter_lock() {
        let (root, wiki) = fixture("recover-dead-waiter");
        stage_applied_create_journal(&wiki, "created", "journal-transaction");
        let lock_path = root.join(PUBLISH_LOCK_PATH);
        fs::create_dir(&lock_path).unwrap();
        let record = LockRecord {
            schema: LOCK_SCHEMA.to_string(),
            transaction_id: "later-mutation-waiter".to_string(),
            pid: i32::MAX as u32,
            thread_id: "dead-thread".to_string(),
            created_at: chrono::Utc::now().to_rfc3339(),
            purpose: LockPurpose::Mutation,
        };
        fs::write(
            lock_path.join(format!(
                "owner-{}-{}.json",
                record.pid, record.transaction_id
            )),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();

        recover(&wiki, RecoveryAction::Accept, true).unwrap();

        assert!(wiki.exists("created"));
        assert!(!root.join(PUBLISH_JOURNAL_PATH).exists());
        assert!(!lock_path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recovery_rolls_back_an_applied_create_journal() {
        let (root, wiki) = fixture("recover-rollback-create");
        stage_applied_create_journal(&wiki, "created", "recover-rollback-create");

        recover(&wiki, RecoveryAction::Rollback, false).unwrap();

        assert!(!wiki.exists("created"));
        assert!(!root.join(PUBLISH_JOURNAL_PATH).exists());
        assert!(!root.join(PUBLISH_LOCK_PATH).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn successful_transaction_applies_all_pages() {
        let (root, wiki) = fixture("success");
        write_page(&wiki, "one", "**One.**\n");
        let mut changes = ChangeSet::new(Some("abc".to_string()));
        changes.push(Change::Update {
            id: "one".to_string(),
            body: Some("**Updated.**\n".to_string()),
            metadata: MetadataPatch::default(),
        });
        changes.push(Change::Create {
            id: "two".to_string(),
            body: "**Two.**\n".to_string(),
            metadata: MetadataPatch::default(),
        });
        let checked = preflight(&wiki, &changes, Some("abc"), Snapshot::new("test")).unwrap();
        let plan = transact(&wiki, checked, Some("abc"), |_| Ok(())).unwrap();

        assert_eq!(plan.operations.len(), 2);
        assert!(wiki.load_page("one").unwrap().body.contains("Updated"));
        assert!(wiki.exists("two"));
        assert!(!root.join(PUBLISH_JOURNAL_PATH).exists());
        fs::remove_dir_all(root).unwrap();
    }
}
