//! Session-scoped coordination. Durable session metadata and append-only
//! notifications live beside `pages/`; per-session inbox state is local and
//! gitignored so acknowledging a message never creates wiki history noise.

use crate::wiki::{contained_path, create_contained_dir_all, Wiki};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering as CmpOrdering;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static UNIQUE_COUNTER: AtomicU64 = AtomicU64::new(0);
const MAX_SESSION_FILE_BYTES: u64 = 64 * 1024;
const MAX_ACTIVITY_FILE_BYTES: u64 = 16 * 1024;
const MAX_LEGACY_INBOX_BYTES: u64 = 16 * 1024 * 1024;
const MAX_NOTIFICATION_FILE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_GIT_CONTEXT_CAPTURE_BYTES: usize = 32 * 1024 * 1024;
const MAX_SESSION_SCAN_ENTRIES: usize = 200_000;
const MAX_SESSION_SCAN_BYTES: u64 = 32 * 1024 * 1024;
const MAX_ACTIVITY_ENTRIES_PER_SESSION: usize = 10_000;
const MAX_ACTIVITY_BYTES_PER_SESSION: u64 = 8 * 1024 * 1024;
const MAX_STORAGE_WARNINGS: usize = 100;
pub const DEFAULT_SESSION_LIST_LIMIT: usize = 100;
pub const DEFAULT_SESSION_SHOW_LIMIT: usize = 20;
pub const MAX_SESSION_RESPONSE_LIMIT: usize = 1_000;
const MAX_SESSION_SHOW_SUMMARY_BYTES: usize = 512;

fn max_session_lookback_seconds() -> u64 {
    crate::config::MAX_SESSION_LOOKBACK_HOURS.saturating_mul(60 * 60)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, clap::ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NotificationKind {
    CodeChange,
    Decision,
    Blocker,
    Handoff,
    Warning,
    #[default]
    Note,
}

impl std::fmt::Display for NotificationKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            NotificationKind::CodeChange => "code-change",
            NotificationKind::Decision => "decision",
            NotificationKind::Blocker => "blocker",
            NotificationKind::Handoff => "handoff",
            NotificationKind::Warning => "warning",
            NotificationKind::Note => "note",
        };
        f.write_str(value)
    }
}

#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    Eq,
    Ord,
    PartialEq,
    PartialOrd,
    clap::ValueEnum,
    Serialize,
    Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Importance {
    Low,
    #[default]
    Normal,
    High,
}

impl std::fmt::Display for Importance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Importance::Low => "low",
            Importance::Normal => "normal",
            Importance::High => "high",
        };
        f.write_str(value)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// Most recent activity observed for this session. Legacy session files
    /// omit it; `load_session` derives it from `updated_at` and activity events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<String>,
    /// How far before `created_at` this session's default inbox begins. This
    /// replaces the old O(history) snapshot in `inbox.toml`.
    #[serde(default)]
    pub notification_lookback_seconds: u64,
    /// Minimum interval between best-effort activity events.
    #[serde(default = "default_activity_debounce_seconds")]
    pub activity_debounce_seconds: u64,
    #[serde(default = "default_true")]
    pub heartbeat_on_activity: bool,
    pub status: String,
}

fn default_activity_debounce_seconds() -> u64 {
    30
}

fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct GitContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dirty_paths: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NotificationMeta {
    pub id: String,
    pub source_session: String,
    pub summary: String,
    pub kind: NotificationKind,
    pub importance: Importance,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// Empty means broadcast. Otherwise only these sessions receive it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<GitContext>,
    /// Caller-defined, one-line metadata for routing without parsing the body.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Notification {
    pub meta: NotificationMeta,
    pub body: String,
}

/// Legacy inbox format. New acknowledgements are append-only marker files.
#[derive(Default, Serialize, Deserialize)]
struct Inbox {
    #[serde(default)]
    states: BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct StartOptions {
    pub agent: Option<String>,
    pub label: Option<String>,
    pub notification_lookback_seconds: u64,
    pub activity_debounce_seconds: u64,
    pub heartbeat_on_activity: bool,
    pub max_agent_bytes: usize,
    pub max_label_bytes: usize,
}

impl Default for StartOptions {
    fn default() -> Self {
        Self {
            agent: None,
            label: None,
            notification_lookback_seconds: 0,
            activity_debounce_seconds: default_activity_debounce_seconds(),
            heartbeat_on_activity: true,
            max_agent_bytes: 128,
            max_label_bytes: 1024,
        }
    }
}

#[derive(Clone, Debug)]
pub struct NotificationLimits {
    pub max_summary_bytes: usize,
    pub max_body_bytes: usize,
    pub max_paths: usize,
    pub max_path_bytes: usize,
    pub max_targets: usize,
    pub max_idempotency_key_bytes: usize,
    pub max_metadata_entries: usize,
    pub max_metadata_key_bytes: usize,
    pub max_metadata_value_bytes: usize,
    pub max_git_dirty_paths: usize,
    pub max_git_branch_bytes: usize,
    pub max_git_commit_bytes: usize,
    pub max_git_worktree_bytes: usize,
}

impl Default for NotificationLimits {
    fn default() -> Self {
        Self {
            max_summary_bytes: 4 * 1024,
            max_body_bytes: 64 * 1024,
            max_paths: 256,
            max_path_bytes: 4096,
            max_targets: 256,
            max_idempotency_key_bytes: 256,
            max_metadata_entries: 32,
            max_metadata_key_bytes: 64,
            max_metadata_value_bytes: 1024,
            max_git_dirty_paths: 256,
            max_git_branch_bytes: 512,
            max_git_commit_bytes: 128,
            max_git_worktree_bytes: 4096,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct NotifyRequest {
    pub source_session: String,
    pub summary: String,
    pub kind: NotificationKind,
    pub importance: Importance,
    pub paths: Vec<String>,
    pub body: Option<String>,
    pub targets: Vec<String>,
    pub idempotency_key: Option<String>,
    pub git: Option<GitContext>,
    pub metadata: BTreeMap<String, String>,
    pub limits: NotificationLimits,
}

#[derive(Clone, Debug, Default)]
pub struct NotificationFilter {
    pub source_sessions: Vec<String>,
    pub kinds: Vec<NotificationKind>,
    pub min_importance: Option<Importance>,
    /// A notification matches when any affected path starts with one prefix.
    pub path_prefixes: Vec<String>,
    pub branches: Vec<String>,
    pub metadata: BTreeMap<String, String>,
    pub created_after: Option<String>,
    pub created_before: Option<String>,
    pub max_age_seconds: Option<u64>,
    pub text: Option<String>,
}

#[derive(Clone, Debug)]
pub struct InboxRequest {
    pub session_id: String,
    pub include_acknowledged: bool,
    /// Overrides the lookback stored on the session. Ignored when full history
    /// is requested with `include_acknowledged`.
    pub lookback_seconds: Option<u64>,
    pub filter: NotificationFilter,
    pub limit: Option<usize>,
    /// Skip this many matching notifications after applying the requested
    /// ordering. Continuations return the next safe value for this field.
    pub offset: usize,
    pub newest_first: bool,
}

impl Default for InboxRequest {
    fn default() -> Self {
        Self {
            session_id: String::new(),
            include_acknowledged: false,
            lookback_seconds: None,
            filter: NotificationFilter::default(),
            limit: None,
            offset: 0,
            // Fresh blockers must not be hidden behind an implicit result cap.
            newest_first: true,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct SessionListRequest {
    pub statuses: Vec<String>,
    pub agents: Vec<String>,
    pub label_contains: Option<String>,
    pub created_after: Option<String>,
    pub active_after: Option<String>,
    pub active_before: Option<String>,
    pub limit: Option<usize>,
    pub cursor: usize,
    pub newest_first: bool,
}

#[derive(Clone, Debug, Default)]
pub struct SessionShowRequest {
    pub limit: Option<usize>,
    pub cursor: usize,
}

#[derive(Clone, Debug)]
pub struct PruneRequest {
    pub closed_only: bool,
    pub older_than_seconds: Option<u64>,
    pub inactive_before: Option<String>,
    pub keep_latest: usize,
    pub dry_run: bool,
}

impl Default for PruneRequest {
    fn default() -> Self {
        Self {
            closed_only: true,
            older_than_seconds: Some(30 * 24 * 60 * 60),
            inactive_before: None,
            keep_latest: 0,
            dry_run: true,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct StorageWarning {
    pub path: String,
    pub message: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct NotificationListing {
    pub notification: NotificationMeta,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct InboxContinuation {
    /// Pass this value back as `offset` while preserving filters, ordering,
    /// and the returned page limit.
    pub offset: usize,
    pub limit: usize,
    pub remaining: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct InboxResult {
    pub session: String,
    pub unread_only: bool,
    pub newest_first: bool,
    pub offset: usize,
    pub total: usize,
    pub returned: usize,
    pub omitted: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continuation: Option<InboxContinuation>,
    pub notifications: Vec<NotificationListing>,
    pub warnings_total: usize,
    pub warnings_omitted: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<StorageWarning>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionListResult {
    pub sessions: Vec<Session>,
    pub total_matches: usize,
    pub returned: usize,
    pub omitted: usize,
    pub cursor: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continuation: Option<usize>,
    pub scan_complete: bool,
    pub entries_scanned: usize,
    pub bytes_scanned: u64,
    pub warnings_total: usize,
    pub warnings_omitted: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<StorageWarning>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionNotificationSummary {
    pub id: String,
    pub summary: String,
    pub summary_truncated: bool,
    pub kind: NotificationKind,
    pub importance: Importance,
    pub created_at: String,
    pub affected_path_count: usize,
    pub target_count: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionShowResult {
    pub session: Session,
    pub notifications_sent: Vec<SessionNotificationSummary>,
    pub total_notifications_sent: usize,
    pub notifications_returned: usize,
    pub notifications_omitted: usize,
    pub cursor: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub continuation: Option<usize>,
    pub scan_complete: bool,
    pub entries_scanned: usize,
    pub bytes_scanned: u64,
    pub warnings_total: usize,
    pub warnings_omitted: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<StorageWarning>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PruneResult {
    pub dry_run: bool,
    pub sessions: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<StorageWarning>,
}

#[derive(Default)]
struct NotificationScan {
    notifications: Vec<StoredNotification>,
    warnings: Vec<StorageWarning>,
    warnings_total: usize,
    complete: bool,
    entries_scanned: usize,
    bytes_scanned: u64,
}

struct NotificationPathScan {
    paths: Vec<PathBuf>,
    warnings: Vec<StorageWarning>,
    warnings_total: usize,
    complete: bool,
    entries_scanned: usize,
    bytes_scanned: u64,
}

struct SessionStorageScan {
    sessions: Vec<Session>,
    warnings: Vec<StorageWarning>,
    warnings_total: usize,
    complete: bool,
    entries_scanned: usize,
    bytes_scanned: u64,
}

#[derive(Clone, Debug)]
struct StoredNotification {
    meta: NotificationMeta,
    path: PathBuf,
}

#[derive(Default)]
struct PreparedNotificationFilter {
    created_after: Option<DateTime<Utc>>,
    created_before: Option<DateTime<Utc>>,
    max_age_cutoff: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct ActivityEvent {
    id: String,
    at: String,
    action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    status: Option<String>,
}

#[derive(Debug)]
struct StorageScanLimit {
    message: String,
}

impl std::fmt::Display for StorageScanLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for StorageScanLimit {}

#[derive(Debug)]
struct StorageScanBudget {
    entries: usize,
    bytes: u64,
    max_entries: usize,
    max_bytes: u64,
}

impl StorageScanBudget {
    fn session_storage() -> Self {
        Self {
            entries: 0,
            bytes: 0,
            max_entries: MAX_SESSION_SCAN_ENTRIES,
            max_bytes: MAX_SESSION_SCAN_BYTES,
        }
    }

    fn charge(&mut self, entries: usize, bytes: u64, label: &str) -> Result<()> {
        let next_entries = self.entries.saturating_add(entries);
        let next_bytes = self.bytes.saturating_add(bytes);
        if next_entries > self.max_entries || next_bytes > self.max_bytes {
            return Err(StorageScanLimit {
                message: format!(
                    "{label} exceeds the hard storage scan ceiling ({} entries or {} bytes)",
                    self.max_entries, self.max_bytes
                ),
            }
            .into());
        }
        self.entries = next_entries;
        self.bytes = next_bytes;
        Ok(())
    }
}

#[derive(Default)]
struct ActivityScan {
    events: Vec<(DateTime<Utc>, ActivityEvent)>,
    warnings: Vec<StorageWarning>,
    warnings_total: usize,
}

fn push_storage_warning(
    warnings: &mut Vec<StorageWarning>,
    total: &mut usize,
    warning: StorageWarning,
) {
    *total = total.saturating_add(1);
    if warnings.len() < MAX_STORAGE_WARNINGS {
        warnings.push(warning);
    }
}

fn scan_limit_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<StorageScanLimit>().is_some()
}

fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn clean_field(name: &str, value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        bail!("{name} cannot be empty");
    }
    if value.chars().any(char::is_control) {
        bail!("{name} must be one line and contain no control characters");
    }
    Ok(value.to_string())
}

fn clean_project_path(name: &str, value: &str, max_len: usize) -> Result<String> {
    let value = clean_field(name, value)?.replace('\\', "/");
    let bytes = value.as_bytes();
    if value.starts_with('/')
        || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
        || value
            .split('/')
            .any(|segment| matches!(segment, "." | ".."))
    {
        bail!("{name} must be a project-relative path without '.' or '..' segments");
    }
    if value.len() > max_len {
        bail!("{name} must be at most {max_len} bytes");
    }
    Ok(value)
}

fn validate_body(body: &str) -> Result<()> {
    if body
        .chars()
        .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
    {
        bail!("notification body contains an unsupported control character");
    }
    Ok(())
}

fn clean_optional_field(
    name: &str,
    value: Option<String>,
    max_len: usize,
) -> Result<Option<String>> {
    value
        .map(|value| {
            let value = clean_field(name, &value)?;
            if value.len() > max_len {
                bail!("{name} must be at most {max_len} bytes");
            }
            Ok(value)
        })
        .transpose()
}

fn parse_time(name: &str, value: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .with_context(|| format!("invalid {name} timestamp '{value}'"))
}

fn compare_timestamps(a: &str, b: &str) -> CmpOrdering {
    let parsed_a = DateTime::parse_from_rfc3339(a).map(|value| value.with_timezone(&Utc));
    let parsed_b = DateTime::parse_from_rfc3339(b).map(|value| value.with_timezone(&Utc));
    match (parsed_a, parsed_b) {
        (Ok(a_time), Ok(b_time)) => a_time.cmp(&b_time).then_with(|| a.cmp(b)),
        _ => a.cmp(b),
    }
}

fn seconds_duration(name: &str, seconds: u64) -> Result<Duration> {
    let seconds = i64::try_from(seconds).with_context(|| format!("{name} is too large"))?;
    Duration::try_seconds(seconds).with_context(|| format!("{name} is too large"))
}

fn checked_cutoff(name: &str, duration: Duration) -> Result<DateTime<Utc>> {
    Utc::now()
        .checked_sub_signed(duration)
        .with_context(|| format!("{name} produces an out-of-range timestamp"))
}

fn validate_generated_id(id: &str, prefix: &str) -> Result<()> {
    if !id.starts_with(&format!("{prefix}-"))
        || id.len() > 96
        || !id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        bail!("invalid {prefix} id '{id}'");
    }
    Ok(())
}

fn unique_id(prefix: &str, attempt: u32) -> String {
    let stamp = Utc::now().format("%Y%m%d-%H%M%S");
    let entropy = Utc::now().timestamp_subsec_nanos() as u64
        ^ ((std::process::id() as u64) << 16)
        ^ attempt as u64
        ^ UNIQUE_COUNTER
            .fetch_add(1, Ordering::Relaxed)
            .rotate_left(23);
    format!("{prefix}-{stamp}-{entropy:08x}")
}

fn sessions_dir(w: &Wiki) -> PathBuf {
    w.dir.join("sessions")
}

fn ensure_real_directory(path: &Path, label: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspecting {label} {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("{label} must be a real directory: {}", path.display());
    }
    Ok(())
}

fn read_bounded_regular_utf8(path: &Path, max_bytes: u64, label: &str) -> Result<String> {
    let path_metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspecting {label} {}", path.display()))?;
    if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
        bail!("{label} must be a regular file: {}", path.display());
    }
    let file =
        fs::File::open(path).with_context(|| format!("opening {label} {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("inspecting open {label} {}", path.display()))?;
    if !metadata.is_file() || metadata.len() > max_bytes {
        bail!("{label} exceeds {max_bytes} bytes or is not a regular file");
    }
    let mut raw = String::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_string(&mut raw)
        .with_context(|| format!("reading {label} {} as UTF-8", path.display()))?;
    if u64::try_from(raw.len()).unwrap_or(u64::MAX) > max_bytes {
        bail!("{label} exceeds {max_bytes} bytes");
    }
    Ok(raw)
}

fn ensure_sessions_dir(w: &Wiki) -> Result<PathBuf> {
    create_contained_dir_all(&w.dir, Path::new("sessions"))
}

fn session_dir(w: &Wiki, id: &str) -> Result<PathBuf> {
    validate_generated_id(id, "session")?;
    let path = contained_path(&w.dir, &Path::new("sessions").join(id))?;
    if path.exists() {
        ensure_real_directory(&path, "session directory")?;
    }
    Ok(path)
}

fn session_path(w: &Wiki, id: &str) -> Result<PathBuf> {
    Ok(session_dir(w, id)?.join("session.toml"))
}

fn inbox_path(w: &Wiki, id: &str) -> Result<PathBuf> {
    Ok(session_dir(w, id)?.join("inbox.toml"))
}

fn acknowledgements_dir(w: &Wiki, id: &str) -> Result<PathBuf> {
    Ok(session_dir(w, id)?.join("inbox"))
}

fn activity_dir(w: &Wiki, id: &str) -> Result<PathBuf> {
    Ok(session_dir(w, id)?.join("activity"))
}

#[cfg(windows)]
fn publish_new_file(temporary: &Path, destination: &Path) -> std::io::Result<()> {
    // Windows rename is no-replace when the destination already exists.
    fs::rename(temporary, destination)
}

#[cfg(not(windows))]
fn publish_new_file(temporary: &Path, destination: &Path) -> std::io::Result<()> {
    // A hard link atomically creates a new name and never replaces an existing
    // destination. The caller removes the temporary name after publication.
    fs::hard_link(temporary, destination)
}

/// Publish an immutable file without ever exposing a partial final record.
/// The complete, synced temporary is hard-linked into place; hard-link creation
/// is atomic and refuses an existing destination on Unix and Windows.
fn write_new(path: &Path, content: &str) -> Result<()> {
    let parent = path
        .parent()
        .context("state file has no parent directory")?;
    ensure_real_directory(parent, "state directory")?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .context("state file name is not valid UTF-8")?;
    let (temporary, mut file) = (0..100)
        .find_map(|_| {
            let sequence = UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed);
            let temporary = parent.join(format!(
                ".{file_name}.tmp-{}-{sequence}",
                std::process::id()
            ));
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            // Session records can contain source paths, agent activity, and
            // notification bodies. Set the creation mode before opening so a
            // temporary is never briefly visible with group/world access.
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&temporary) {
                Ok(file) => {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        if let Err(error) = file.set_permissions(fs::Permissions::from_mode(0o600))
                        {
                            drop(file);
                            let _ = fs::remove_file(&temporary);
                            return Some(Err(error));
                        }
                    }
                    Some(Ok((temporary, file)))
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => None,
                Err(error) => Some(Err(error)),
            }
        })
        .transpose()
        .with_context(|| format!("creating temporary state beside {}", path.display()))?
        .context("could not allocate a temporary state file")?;

    let result = (|| -> Result<()> {
        file.write_all(content.as_bytes())
            .with_context(|| format!("writing temporary state for {}", path.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing temporary state for {}", path.display()))?;
        drop(file);
        publish_new_file(&temporary, path)
            .with_context(|| format!("publishing immutable state {}", path.display()))?;
        #[cfg(unix)]
        if let Ok(directory) = fs::File::open(parent) {
            let _ = directory.sync_all();
        }
        Ok(())
    })();
    let _ = fs::remove_file(&temporary);
    result
}

fn load_session_with_budget(
    w: &Wiki,
    id: &str,
    mut budget: Option<&mut StorageScanBudget>,
) -> Result<(Session, Vec<StorageWarning>, usize)> {
    let path = session_path(w, id)?;
    if let Some(budget) = budget.as_deref_mut() {
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("inspecting session metadata {}", path.display()))?;
        budget.charge(0, metadata.len(), "session storage")?;
    }
    let raw = read_bounded_regular_utf8(&path, MAX_SESSION_FILE_BYTES, "session metadata")
        .with_context(|| format!("no session '{id}' (looked at {})", path.display()))?;
    let mut session: Session =
        toml::from_str(&raw).with_context(|| format!("parsing session '{id}'"))?;
    if session.id != id {
        bail!("session metadata id does not match its directory");
    }
    validate_generated_id(&session.id, "session")?;
    if clean_field("agent", &session.agent)?.len() > w.sessions.max_agent_bytes {
        bail!("session agent exceeds configured size limit");
    }
    if let Some(label) = &session.label {
        if clean_field("label", label)?.len() > w.sessions.max_label_bytes {
            bail!("session label exceeds configured size limit");
        }
    }
    let created_at = parse_time("session created", &session.created_at)?;
    let mut effective_updated_at = parse_time("session updated", &session.updated_at)?;
    if effective_updated_at < created_at {
        bail!("session updated timestamp precedes its creation timestamp");
    }
    if let Some(last_seen) = &session.last_seen_at {
        if parse_time("session last seen", last_seen)? < created_at {
            bail!("session last-seen timestamp precedes its creation timestamp");
        }
    }
    if !matches!(session.status.as_str(), "active" | "closed") {
        bail!("session status must be active or closed");
    }
    if session.notification_lookback_seconds > max_session_lookback_seconds() {
        bail!(
            "session notification lookback exceeds the hard ceiling of {} hours",
            crate::config::MAX_SESSION_LOOKBACK_HOURS
        );
    }
    seconds_duration(
        "session notification lookback",
        session.notification_lookback_seconds,
    )?;
    if !(1..=crate::config::MAX_SESSION_ACTIVITY_DEBOUNCE_SECONDS)
        .contains(&session.activity_debounce_seconds)
    {
        bail!(
            "session activity debounce must be between 1 and {} seconds",
            crate::config::MAX_SESSION_ACTIVITY_DEBOUNCE_SECONDS
        );
    }
    seconds_duration(
        "session activity debounce",
        session.activity_debounce_seconds,
    )?;
    if session.last_seen_at.is_none() {
        session.last_seen_at = Some(session.updated_at.clone());
    }

    let ActivityScan {
        mut events,
        warnings,
        warnings_total,
    } = scan_activity(w, id, budget)?;
    events.sort_by(|(a_time, a), (b_time, b)| a_time.cmp(b_time).then(a.id.cmp(&b.id)));
    for (event_time, event) in events {
        if event_time >= effective_updated_at {
            effective_updated_at = event_time;
            session.updated_at = event.at.clone();
            session.last_seen_at = Some(event.at);
            if let Some(status) = event.status {
                session.status = status;
            }
        }
    }
    Ok((session, warnings, warnings_total))
}

fn load_session(w: &Wiki, id: &str) -> Result<Session> {
    load_session_with_budget(w, id, None).map(|(session, _, _)| session)
}

fn parse_activity_event(path: &Path) -> Result<(DateTime<Utc>, ActivityEvent)> {
    let raw = read_bounded_regular_utf8(path, MAX_ACTIVITY_FILE_BYTES, "activity entry")?;
    let event: ActivityEvent = toml::from_str(&raw)?;
    validate_generated_id(&event.id, "activity")?;
    if path.file_stem().and_then(|stem| stem.to_str()) != Some(event.id.as_str()) {
        bail!("activity id does not match its filename");
    }
    let timestamp = parse_time("activity", &event.at)?;
    if clean_field("activity action", &event.action)?.len() > 128 {
        bail!("activity action exceeds 128 bytes");
    }
    if event
        .status
        .as_deref()
        .is_some_and(|status| !matches!(status, "active" | "closed"))
    {
        bail!("invalid activity status");
    }
    Ok((timestamp, event))
}

fn scan_activity(
    w: &Wiki,
    id: &str,
    mut budget: Option<&mut StorageScanBudget>,
) -> Result<ActivityScan> {
    let path = activity_dir(w, id)?;
    if !path.exists() {
        return Ok(ActivityScan::default());
    }
    ensure_real_directory(&path, "activity directory")?;
    let entries = fs::read_dir(&path)
        .with_context(|| format!("reading activity directory {}", path.display()))?;
    let mut scan = ActivityScan::default();
    let mut local_entries = 0_usize;
    let mut local_bytes = 0_u64;
    for entry in entries {
        local_entries = local_entries.saturating_add(1);
        if local_entries > MAX_ACTIVITY_ENTRIES_PER_SESSION {
            return Err(StorageScanLimit {
                message: format!(
                    "session '{id}' activity exceeds the hard scan ceiling of {MAX_ACTIVITY_ENTRIES_PER_SESSION} entries"
                ),
            }
            .into());
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                push_storage_warning(
                    &mut scan.warnings,
                    &mut scan.warnings_total,
                    StorageWarning {
                        path: path.display().to_string(),
                        message: format!("cannot inspect activity entry: {error}"),
                    },
                );
                if let Some(budget) = budget.as_deref_mut() {
                    budget.charge(1, 0, "session storage")?;
                }
                continue;
            }
        };
        let entry_path = entry.path();
        let entry_bytes = match fs::symlink_metadata(&entry_path) {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                push_storage_warning(
                    &mut scan.warnings,
                    &mut scan.warnings_total,
                    StorageWarning {
                        path: entry_path.display().to_string(),
                        message: error.to_string(),
                    },
                );
                0
            }
        };
        local_bytes = local_bytes.saturating_add(entry_bytes);
        if local_bytes > MAX_ACTIVITY_BYTES_PER_SESSION {
            return Err(StorageScanLimit {
                message: format!(
                    "session '{id}' activity exceeds the hard scan ceiling of {MAX_ACTIVITY_BYTES_PER_SESSION} bytes"
                ),
            }
            .into());
        }
        if let Some(budget) = budget.as_deref_mut() {
            budget.charge(1, entry_bytes, "session storage")?;
        }
        if entry_path
            .extension()
            .and_then(|extension| extension.to_str())
            != Some("toml")
        {
            continue;
        }
        match parse_activity_event(&entry_path) {
            Ok(event) => scan.events.push(event),
            Err(error) => push_storage_warning(
                &mut scan.warnings,
                &mut scan.warnings_total,
                StorageWarning {
                    path: entry_path.display().to_string(),
                    message: error.to_string(),
                },
            ),
        }
    }
    Ok(scan)
}

fn create_session_file(
    w: &Wiki,
    guard: &crate::publish::MutationGuard,
    session: &Session,
) -> Result<()> {
    w.ensure_mutation_guard(guard)?;
    write_new(
        &session_path(w, &session.id)?,
        &toml::to_string_pretty(session)?,
    )
}

fn load_legacy_inbox(w: &Wiki, id: &str) -> Result<(Inbox, Option<StorageWarning>)> {
    load_session(w, id)?;
    let path = inbox_path(w, id)?;
    if !path.exists() {
        return Ok((Inbox::default(), None));
    }
    match read_bounded_regular_utf8(&path, MAX_LEGACY_INBOX_BYTES, "legacy inbox")
        .and_then(|raw| toml::from_str(&raw).map_err(Into::into))
    {
        Ok(inbox) => Ok((inbox, None)),
        Err(error) => Ok((
            Inbox::default(),
            Some(StorageWarning {
                path: path.display().to_string(),
                message: format!("legacy inbox ignored: {error}"),
            }),
        )),
    }
}

fn notification_paths(w: &Wiki) -> NotificationPathScan {
    let mut scan = NotificationPathScan {
        paths: vec![],
        warnings: vec![],
        warnings_total: 0,
        complete: true,
        entries_scanned: 0,
        bytes_scanned: 0,
    };
    let mut budget = StorageScanBudget::session_storage();
    let root = sessions_dir(w);
    if !root.exists() {
        return scan;
    }
    if let Err(error) = ensure_real_directory(&root, "sessions root") {
        push_storage_warning(
            &mut scan.warnings,
            &mut scan.warnings_total,
            StorageWarning {
                path: root.display().to_string(),
                message: error.to_string(),
            },
        );
        scan.complete = false;
        return scan;
    }
    let Ok(entries) = fs::read_dir(&root) else {
        push_storage_warning(
            &mut scan.warnings,
            &mut scan.warnings_total,
            StorageWarning {
                path: root.display().to_string(),
                message: "cannot read sessions root".into(),
            },
        );
        scan.complete = false;
        return scan;
    };
    'sessions: for entry in entries {
        if let Err(error) = budget.charge(1, 0, "notification storage") {
            push_storage_warning(
                &mut scan.warnings,
                &mut scan.warnings_total,
                StorageWarning {
                    path: root.display().to_string(),
                    message: format!("{error}; notification scan is incomplete"),
                },
            );
            scan.complete = false;
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                push_storage_warning(
                    &mut scan.warnings,
                    &mut scan.warnings_total,
                    StorageWarning {
                        path: root.display().to_string(),
                        message: format!("cannot inspect session entry: {error}"),
                    },
                );
                scan.complete = false;
                continue;
            }
        };
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => {}
            Ok(_) => continue,
            Err(error) => {
                push_storage_warning(
                    &mut scan.warnings,
                    &mut scan.warnings_total,
                    StorageWarning {
                        path: entry.path().display().to_string(),
                        message: format!("cannot inspect session entry type: {error}"),
                    },
                );
                scan.complete = false;
                continue;
            }
        }
        let dir = entry.path().join("notifications");
        if let Err(error) = ensure_real_directory(&dir, "notifications directory") {
            push_storage_warning(
                &mut scan.warnings,
                &mut scan.warnings_total,
                StorageWarning {
                    path: dir.display().to_string(),
                    message: error.to_string(),
                },
            );
            scan.complete = false;
            continue;
        }
        let Ok(notifications) = fs::read_dir(&dir) else {
            push_storage_warning(
                &mut scan.warnings,
                &mut scan.warnings_total,
                StorageWarning {
                    path: dir.display().to_string(),
                    message: "cannot read notifications directory".into(),
                },
            );
            scan.complete = false;
            continue;
        };
        for entry in notifications {
            if let Err(error) = budget.charge(1, 0, "notification storage") {
                push_storage_warning(
                    &mut scan.warnings,
                    &mut scan.warnings_total,
                    StorageWarning {
                        path: root.display().to_string(),
                        message: format!("{error}; notification scan is incomplete"),
                    },
                );
                scan.complete = false;
                break 'sessions;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    push_storage_warning(
                        &mut scan.warnings,
                        &mut scan.warnings_total,
                        StorageWarning {
                            path: dir.display().to_string(),
                            message: format!("cannot inspect notification entry: {error}"),
                        },
                    );
                    scan.complete = false;
                    continue;
                }
            };
            let path = entry.path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("md") {
                continue;
            }
            match fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                    if let Err(error) = budget.charge(0, metadata.len(), "notification storage") {
                        push_storage_warning(
                            &mut scan.warnings,
                            &mut scan.warnings_total,
                            StorageWarning {
                                path: root.display().to_string(),
                                message: format!("{error}; notification scan is incomplete"),
                            },
                        );
                        scan.complete = false;
                        break 'sessions;
                    }
                    scan.paths.push(path)
                }
                Ok(_) => push_storage_warning(
                    &mut scan.warnings,
                    &mut scan.warnings_total,
                    StorageWarning {
                        path: path.display().to_string(),
                        message: "notification entry is not a regular file".into(),
                    },
                ),
                Err(error) => push_storage_warning(
                    &mut scan.warnings,
                    &mut scan.warnings_total,
                    StorageWarning {
                        path: path.display().to_string(),
                        message: error.to_string(),
                    },
                ),
            }
        }
    }
    scan.paths.sort();
    scan.entries_scanned = budget.entries;
    scan.bytes_scanned = budget.bytes;
    scan
}

fn render_notification(notification: &Notification) -> Result<String> {
    Ok(format!(
        "+++\n{}+++\n\n{}\n",
        toml::to_string_pretty(&notification.meta)?,
        notification.body.trim_end()
    ))
}

fn notification_file_limit(w: &Wiki) -> u64 {
    let settings = &w.sessions;
    let header = 64_usize
        .saturating_mul(1024)
        .saturating_add(settings.max_summary_bytes)
        .saturating_add(settings.max_idempotency_key_bytes)
        .saturating_add(settings.max_git_branch_bytes)
        .saturating_add(settings.max_git_commit_bytes)
        .saturating_add(settings.max_git_worktree_bytes)
        .saturating_add(settings.max_paths.saturating_mul(settings.max_path_bytes))
        .saturating_add(settings.max_targets.saturating_mul(128))
        .saturating_add(
            settings.max_metadata_entries.saturating_mul(
                settings
                    .max_metadata_key_bytes
                    .saturating_add(settings.max_metadata_value_bytes),
            ),
        )
        .saturating_add(
            settings
                .max_git_dirty_paths
                .saturating_mul(settings.max_path_bytes),
        )
        // TOML escaping can expand caller-controlled strings.
        .saturating_mul(2);
    u64::try_from(
        header
            .saturating_add(settings.max_body_bytes)
            .saturating_add(16),
    )
    .unwrap_or(u64::MAX)
    .min(MAX_NOTIFICATION_FILE_BYTES)
}

fn parse_notification_file(w: &Wiki, path: &Path, include_body: bool) -> Result<Notification> {
    let path_metadata = fs::symlink_metadata(path)?;
    if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
        bail!("notification is not a regular file");
    }
    let limit = notification_file_limit(w);
    let file = fs::File::open(path)?;
    let metadata = file.metadata()?;
    if metadata.len() > limit {
        bail!(
            "notification file is {} bytes, exceeding configured limit {limit}",
            metadata.len()
        );
    }
    let mut reader = BufReader::new(file.take(limit.saturating_add(1)));
    let mut bytes_read = 0_u64;
    let mut line = String::new();
    reader.read_line(&mut line)?;
    bytes_read = bytes_read.saturating_add(u64::try_from(line.len()).unwrap_or(u64::MAX));
    if line.trim_end_matches(['\r', '\n']) != "+++" {
        bail!("notification is missing TOML frontmatter");
    }
    let mut frontmatter = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            bail!("notification frontmatter is not closed");
        }
        bytes_read = bytes_read.saturating_add(u64::try_from(line.len()).unwrap_or(u64::MAX));
        if bytes_read > limit {
            bail!("notification exceeds configured size limits");
        }
        if line.trim_end_matches(['\r', '\n']) == "+++" {
            break;
        }
        frontmatter.push_str(&line);
        if u64::try_from(frontmatter.len()).unwrap_or(u64::MAX) > limit {
            bail!("notification frontmatter exceeds configured size limits");
        }
    }
    let meta = toml::from_str(&frontmatter)?;
    validate_notification_meta(w, &meta)?;
    if path.file_stem().and_then(|stem| stem.to_str()) != Some(meta.id.as_str()) {
        bail!("notification id does not match its filename");
    }
    let source_directory = path
        .parent()
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .context("notification path has no source-session directory")?;
    if source_directory != meta.source_session {
        bail!("notification source session does not match its directory");
    }
    let body = if include_body {
        let mut body = String::new();
        reader.read_to_string(&mut body)?;
        bytes_read = bytes_read.saturating_add(u64::try_from(body.len()).unwrap_or(u64::MAX));
        if bytes_read > limit {
            bail!("notification exceeds configured size limits");
        }
        let body = body.trim_start_matches(['\r', '\n']).trim_end().to_string();
        validate_body(&body)?;
        if body.len() > w.sessions.max_body_bytes {
            bail!(
                "notification body exceeds configured limit {}",
                w.sessions.max_body_bytes
            );
        }
        body
    } else {
        String::new()
    };
    Ok(Notification { meta, body })
}

fn scan_notifications(w: &Wiki) -> NotificationScan {
    let paths = notification_paths(w);
    let mut scan = NotificationScan {
        notifications: vec![],
        warnings: paths.warnings,
        warnings_total: paths.warnings_total,
        complete: paths.complete,
        entries_scanned: paths.entries_scanned,
        bytes_scanned: paths.bytes_scanned,
    };
    for path in paths.paths {
        match parse_notification_file(w, &path, false) {
            Ok(notification) => scan.notifications.push(StoredNotification {
                meta: notification.meta,
                path,
            }),
            Err(error) => push_storage_warning(
                &mut scan.warnings,
                &mut scan.warnings_total,
                StorageWarning {
                    path: path.display().to_string(),
                    message: error.to_string(),
                },
            ),
        }
    }
    scan.notifications.sort_by(|a, b| {
        compare_timestamps(&a.meta.created_at, &b.meta.created_at).then(a.meta.id.cmp(&b.meta.id))
    });
    let mut counts = BTreeMap::new();
    for notification in &scan.notifications {
        *counts
            .entry(notification.meta.id.clone())
            .or_insert(0_usize) += 1;
    }
    let duplicates = counts
        .into_iter()
        .filter_map(|(id, count)| (count > 1).then_some(id))
        .collect::<BTreeSet<_>>();
    for id in &duplicates {
        push_storage_warning(
            &mut scan.warnings,
            &mut scan.warnings_total,
            StorageWarning {
                path: id.clone(),
                message: "duplicate notification id; every copy was ignored".into(),
            },
        );
    }
    scan.notifications
        .retain(|notification| !duplicates.contains(&notification.meta.id));
    scan
}

fn ensure_child_dir(parent: &Path, name: &str) -> Result<PathBuf> {
    create_contained_dir_all(parent, Path::new(name))
}

struct ActivityRecord {
    session: Session,
    history_path: Option<String>,
}

fn commit_session_paths(
    w: &Wiki,
    guard: &crate::publish::MutationGuard,
    message: &str,
    paths: &[String],
) -> Result<()> {
    w.ensure_mutation_guard(guard)?;
    if w.history.commit_sessions {
        w.commit_paths(message, paths)?;
    }
    Ok(())
}

fn record_activity(
    w: &Wiki,
    guard: &crate::publish::MutationGuard,
    id: &str,
    action: &str,
    status: Option<&str>,
    force: bool,
) -> Result<ActivityRecord> {
    w.ensure_mutation_guard(guard)?;
    let mut session = load_session(w, id)?;
    if !force && !session.heartbeat_on_activity {
        return Ok(ActivityRecord {
            session,
            history_path: None,
        });
    }
    let timestamp = now();
    if !force {
        let last = session
            .last_seen_at
            .as_deref()
            .unwrap_or(&session.updated_at);
        if let (Ok(last), Ok(current)) = (
            parse_time("last activity", last),
            parse_time("activity", &timestamp),
        ) {
            let debounce =
                seconds_duration("activity debounce", session.activity_debounce_seconds)?;
            if current.signed_duration_since(last) < debounce {
                return Ok(ActivityRecord {
                    session,
                    history_path: None,
                });
            }
        }
    }

    let action = clean_field("activity", action)?;
    let dir = ensure_child_dir(&session_dir(w, id)?, "activity")?;
    let mut written = None;
    for attempt in 0..100 {
        let event_id = unique_id("activity", attempt);
        let event = ActivityEvent {
            id: event_id.clone(),
            at: timestamp.clone(),
            action: action.clone(),
            status: status.map(str::to_string),
        };
        let path = dir.join(format!("{event_id}.toml"));
        match write_new(&path, &toml::to_string_pretty(&event)?) {
            Ok(()) => {
                written = Some(event);
                break;
            }
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::AlreadyExists) =>
            {
                continue;
            }
            Err(error) => return Err(error),
        }
    }
    let event = written.context("could not allocate a unique activity event id")?;
    session.updated_at = event.at.clone();
    session.last_seen_at = Some(event.at);
    if let Some(status) = event.status.as_ref() {
        session.status = status.clone();
    }
    Ok(ActivityRecord {
        session,
        history_path: Some(format!("sessions/{id}/activity/{}.toml", event.id)),
    })
}

fn bounded_git_value(cwd: &Path, args: &[&str], label: &str, max_bytes: usize) -> Option<String> {
    let capture_limit = max_bytes
        .saturating_add(2)
        .min(MAX_GIT_CONTEXT_CAPTURE_BYTES);
    let mut output = crate::git_paths::bounded_git_stdout(cwd, args, label, capture_limit).ok()?;
    while matches!(output.last(), Some(b'\n' | b'\r')) {
        output.pop();
    }
    if output.is_empty() || output.len() > max_bytes {
        return None;
    }
    let value = String::from_utf8(output).ok()?;
    clean_field(label, &value).ok()
}

/// Return the invocation directory only when it belongs to this wiki's
/// registered project. An explicit `--wiki` from an unrelated repository must
/// not attach that other repository's branch or commit to a notification.
fn git_context_cwd(w: &Wiki, cwd: &Path) -> Option<PathBuf> {
    let invocation = cwd.canonicalize().ok()?;
    let registered = w
        .config
        .project_roots
        .iter()
        .map(PathBuf::from)
        .map(|root| root.canonicalize().unwrap_or(root))
        .collect::<Vec<_>>();
    if registered.iter().any(|root| invocation.starts_with(root)) {
        return Some(invocation);
    }

    // Linked worktrees intentionally resolve to the main checkout's wiki.
    // Resolve the common directory with the same bounded process helper used
    // for notification metadata instead of falling back to `Command::output`.
    let common = bounded_git_value(
        &invocation,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        "Git common directory",
        w.sessions.max_git_worktree_bytes,
    )?;
    let common = PathBuf::from(common);
    let main = (common.file_name()?.to_str()? == ".git")
        .then(|| common.parent().map(Path::to_path_buf))
        .flatten()?
        .canonicalize()
        .ok()?;
    registered
        .iter()
        .any(|root| root == &main)
        .then_some(invocation)
}

fn git_status_capture_limit(w: &Wiki) -> usize {
    w.sessions
        .max_git_dirty_paths
        .saturating_mul(w.sessions.max_path_bytes.saturating_add(4))
        .min(MAX_GIT_CONTEXT_CAPTURE_BYTES)
}

/// Best-effort, bounded Git metadata for notification routing. Outside a
/// registered project worktree this returns `None`; an unavailable or
/// over-limit optional field is omitted, while an unsafe/oversized status
/// inventory becomes an empty dirty-path list rather than failing `notify`.
pub fn capture_git_context(w: &Wiki, cwd: &Path) -> Option<GitContext> {
    let cwd = git_context_cwd(w, cwd)?;
    let worktree = bounded_git_value(
        &cwd,
        &["rev-parse", "--show-toplevel"],
        "Git worktree",
        w.sessions.max_git_worktree_bytes,
    )?;
    let branch = bounded_git_value(
        &cwd,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
        "Git branch",
        w.sessions.max_git_branch_bytes,
    );
    let commit = bounded_git_value(
        &cwd,
        &["rev-parse", "--verify", "HEAD"],
        "Git commit",
        w.sessions.max_git_commit_bytes,
    );
    let dirty_paths = crate::git_paths::bounded_git_stdout(
        &cwd,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        "Git status for notification context",
        git_status_capture_limit(w),
    )
    .ok()
    .and_then(|output| {
        crate::git_paths::parse_porcelain_v1(&output, "Git notification status")
            .ok()
            .map(|paths| paths.dirty)
    })
    .and_then(|paths| {
        if paths.len() > w.sessions.max_git_dirty_paths {
            return None;
        }
        paths
            .into_iter()
            .map(|path| clean_project_path("Git dirty path", &path, w.sessions.max_path_bytes))
            .collect::<Result<Vec<_>>>()
            .ok()
    })
    .unwrap_or_default();
    Some(GitContext {
        branch,
        commit,
        worktree: Some(worktree),
        dirty_paths,
    })
}

fn stable_hash(value: &str) -> u64 {
    // FNV-1a is intentionally implemented here so idempotent IDs are stable
    // across Rust releases and processes without adding a hashing dependency.
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in value.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn notification_state(
    w: &Wiki,
    session_id: &str,
    notification_id: &str,
    legacy: &Inbox,
) -> Result<Option<String>> {
    validate_generated_id(notification_id, "notify")?;
    let dir = acknowledgements_dir(w, session_id)?;
    if !dir.exists() {
        return Ok(legacy.states.get(notification_id).cloned());
    }
    ensure_real_directory(&dir, "inbox directory")?;
    let dismissed = dir.join(format!("{notification_id}.dismissed"));
    if fs::symlink_metadata(&dismissed)
        .map(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Ok(Some("dismissed".into()));
    }
    let read = dir.join(format!("{notification_id}.read"));
    if fs::symlink_metadata(&read)
        .map(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Ok(Some("read".into()));
    }
    Ok(legacy.states.get(notification_id).cloned())
}

fn write_acknowledgement(
    w: &Wiki,
    guard: &crate::publish::MutationGuard,
    session_id: &str,
    id: &str,
    state: &str,
) -> Result<()> {
    w.ensure_mutation_guard(guard)?;
    load_session(w, session_id)?;
    validate_generated_id(id, "notify")?;
    if !matches!(state, "read" | "dismissed") {
        bail!("invalid notification state '{state}'");
    }
    let dir = ensure_child_dir(&session_dir(w, session_id)?, "inbox")?;
    let path = dir.join(format!("{id}.{state}"));
    let content = format!("state = {state:?}\nat = {:?}\n", now());
    match write_new(&path, &content) {
        Ok(()) => Ok(()),
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::AlreadyExists) =>
        {
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!(
                    "acknowledgement path is not a regular file: {}",
                    path.display()
                );
            }
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn validate_notification_meta(w: &Wiki, meta: &NotificationMeta) -> Result<()> {
    let settings = &w.sessions;
    validate_generated_id(&meta.id, "notify")?;
    validate_generated_id(&meta.source_session, "session")?;
    let summary = clean_field("summary", &meta.summary)?;
    if summary.len() > settings.max_summary_bytes {
        bail!("summary exceeds configured size limit");
    }
    parse_time("notification", &meta.created_at)?;
    if meta.paths.len() > settings.max_paths {
        bail!("notification has too many paths");
    }
    let mut normalized_paths = BTreeSet::new();
    for path in &meta.paths {
        if !normalized_paths.insert(clean_project_path("path", path, settings.max_path_bytes)?) {
            bail!("notification paths must be unique");
        }
    }
    if meta.targets.len() > settings.max_targets {
        bail!("notification has too many targets");
    }
    if meta.targets.iter().collect::<BTreeSet<_>>().len() != meta.targets.len() {
        bail!("notification targets must be unique");
    }
    for target in &meta.targets {
        validate_generated_id(target, "session")?;
        if target == &meta.source_session {
            bail!("notification cannot target its own source session");
        }
    }
    if let Some(key) = &meta.idempotency_key {
        let key = clean_field("idempotency key", key)?;
        if key.len() > settings.max_idempotency_key_bytes {
            bail!("idempotency key exceeds configured size limit");
        }
    }
    if meta.metadata.len() > settings.max_metadata_entries {
        bail!("notification has too many metadata entries");
    }
    for (key, value) in &meta.metadata {
        if clean_field("metadata key", key)?.len() > settings.max_metadata_key_bytes
            || clean_field("metadata value", value)?.len() > settings.max_metadata_value_bytes
        {
            bail!("notification metadata exceeds configured size limits");
        }
    }
    if let Some(git) = &meta.git {
        let mut normalized_dirty_paths = BTreeSet::new();
        for (name, value) in [
            ("git branch", git.branch.as_deref()),
            ("git commit", git.commit.as_deref()),
            ("git worktree", git.worktree.as_deref()),
        ] {
            if let Some(value) = value {
                clean_field(name, value)?;
            }
        }
        for path in &git.dirty_paths {
            if !normalized_dirty_paths.insert(clean_project_path(
                "dirty path",
                path,
                settings.max_path_bytes,
            )?) {
                bail!("notification Git dirty paths must be unique");
            }
        }
        if git
            .branch
            .as_ref()
            .is_some_and(|value| value.len() > settings.max_git_branch_bytes)
            || git
                .commit
                .as_ref()
                .is_some_and(|value| value.len() > settings.max_git_commit_bytes)
            || git
                .worktree
                .as_ref()
                .is_some_and(|value| value.len() > settings.max_git_worktree_bytes)
            || git.dirty_paths.len() > settings.max_git_dirty_paths
            || git
                .dirty_paths
                .iter()
                .any(|path| path.len() > settings.max_path_bytes)
        {
            bail!("notification Git context exceeds configured size limits");
        }
    }
    Ok(())
}

/// Inspect notification storage without allowing one malformed file to block
/// the valid collection. This is suitable for `doctor`, MCP diagnostics, and
/// any caller that wants to surface exact warning paths.
pub fn inspect_notifications(w: &Wiki) -> (Vec<NotificationMeta>, Vec<StorageWarning>) {
    let scan = scan_notifications(w);
    let mut warnings = scan.warnings;
    if scan.warnings_total > warnings.len() {
        if warnings.len() == MAX_STORAGE_WARNINGS {
            warnings.pop();
        }
        warnings.push(StorageWarning {
            path: sessions_dir(w).display().to_string(),
            message: format!(
                "{} additional storage warning(s) omitted",
                scan.warnings_total.saturating_sub(warnings.len())
            ),
        });
    }
    (
        scan.notifications
            .into_iter()
            .map(|notification| notification.meta)
            .collect(),
        warnings,
    )
}

fn find_stored_notification(w: &Wiki, id: &str) -> Result<StoredNotification> {
    validate_generated_id(id, "notify")?;
    let scan = scan_notifications(w);
    if !scan.complete {
        bail!(
            "notification storage scan is incomplete; refusing to resolve '{id}' while a matching or duplicate notice may be hidden"
        );
    }
    let mut matches = scan
        .notifications
        .into_iter()
        .filter(|notification| notification.meta.id == id);
    let notification = matches.next().with_context(|| {
        format!(
            "no usable notification '{id}' ({} corrupt notification(s) skipped)",
            scan.warnings.len()
        )
    })?;
    if matches.next().is_some() {
        bail!("notification id '{id}' is duplicated");
    }
    Ok(notification)
}

fn find_notification(w: &Wiki, id: &str) -> Result<Notification> {
    let notification = find_stored_notification(w, id)?;
    let loaded = parse_notification_file(w, &notification.path, true)?;
    if loaded.meta.id != notification.meta.id {
        bail!("notification changed while it was being read");
    }
    Ok(loaded)
}

fn delivered_to(meta: &NotificationMeta, session_id: &str) -> bool {
    meta.targets.is_empty() || meta.targets.iter().any(|target| target == session_id)
}

fn prepare_notification_filter(filter: &NotificationFilter) -> Result<PreparedNotificationFilter> {
    Ok(PreparedNotificationFilter {
        created_after: filter
            .created_after
            .as_deref()
            .map(|value| parse_time("created-after", value))
            .transpose()?,
        created_before: filter
            .created_before
            .as_deref()
            .map(|value| parse_time("created-before", value))
            .transpose()?,
        max_age_cutoff: filter
            .max_age_seconds
            .map(|seconds| {
                checked_cutoff(
                    "notification max age",
                    seconds_duration("max age", seconds)?,
                )
            })
            .transpose()?,
    })
}

fn matches_filter(
    w: &Wiki,
    notification: &StoredNotification,
    filter: &NotificationFilter,
    prepared: &PreparedNotificationFilter,
) -> Result<bool> {
    let meta = &notification.meta;
    if !filter.source_sessions.is_empty()
        && !filter
            .source_sessions
            .iter()
            .any(|source| source == &meta.source_session)
    {
        return Ok(false);
    }
    if !filter.kinds.is_empty() && !filter.kinds.contains(&meta.kind) {
        return Ok(false);
    }
    if filter
        .min_importance
        .is_some_and(|importance| meta.importance < importance)
    {
        return Ok(false);
    }
    if !filter.path_prefixes.is_empty()
        && !meta.paths.iter().any(|path| {
            filter
                .path_prefixes
                .iter()
                .any(|prefix| path.starts_with(prefix))
        })
    {
        return Ok(false);
    }
    if !filter.branches.is_empty()
        && !meta
            .git
            .as_ref()
            .and_then(|git| git.branch.as_ref())
            .is_some_and(|branch| filter.branches.contains(branch))
    {
        return Ok(false);
    }
    if !filter
        .metadata
        .iter()
        .all(|(key, value)| meta.metadata.get(key) == Some(value))
    {
        return Ok(false);
    }

    let created = parse_time("notification", &meta.created_at)?;
    if prepared.created_after.is_some_and(|after| created < after) {
        return Ok(false);
    }
    if prepared
        .created_before
        .is_some_and(|before| created > before)
    {
        return Ok(false);
    }
    if prepared
        .max_age_cutoff
        .is_some_and(|cutoff| created < cutoff)
    {
        return Ok(false);
    }
    if let Some(text) = &filter.text {
        let needle = text.to_lowercase();
        if !meta.summary.to_lowercase().contains(&needle) {
            let body = parse_notification_file(w, &notification.path, true)?.body;
            if !body.to_lowercase().contains(&needle) {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

pub fn start_with_options(w: &Wiki, options: StartOptions) -> Result<Session> {
    // Keep record publication and its optional history commit inside one
    // shared writer boundary. The guard's recovery checks happen before the
    // first durable session path can be created.
    let guard = w.acquire_mutation_guard()?;
    if options.notification_lookback_seconds > max_session_lookback_seconds() {
        bail!(
            "session notification lookback exceeds the hard ceiling of {} hours",
            crate::config::MAX_SESSION_LOOKBACK_HOURS
        );
    }
    if !(1..=crate::config::MAX_SESSION_ACTIVITY_DEBOUNCE_SECONDS)
        .contains(&options.activity_debounce_seconds)
    {
        bail!(
            "session activity debounce must be between 1 and {} seconds",
            crate::config::MAX_SESSION_ACTIVITY_DEBOUNCE_SECONDS
        );
    }
    let agent = clean_field("agent", options.agent.as_deref().unwrap_or("unknown"))?;
    if agent.len() > options.max_agent_bytes {
        bail!("agent must be at most {} bytes", options.max_agent_bytes);
    }
    let label = options
        .label
        .map(|value| clean_field("label", &value))
        .transpose()?;
    if label
        .as_ref()
        .is_some_and(|label| label.len() > options.max_label_bytes)
    {
        bail!("label must be at most {} bytes", options.max_label_bytes);
    }
    w.ensure_gitignore_guarded(&guard)?;
    ensure_sessions_dir(w)?;

    let mut created = None;
    for attempt in 0..100 {
        let id = unique_id("session", attempt);
        let dir = contained_path(&w.dir, &Path::new("sessions").join(&id))?;
        let mut builder = fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(&dir) {
            Ok(()) => {
                created = Some((id, dir));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    let (id, dir) = created.context("could not allocate a unique session id")?;
    ensure_child_dir(&dir, "notifications")?;
    let timestamp = now();
    let session = Session {
        id: id.clone(),
        agent,
        label,
        created_at: timestamp.clone(),
        updated_at: timestamp.clone(),
        last_seen_at: Some(timestamp),
        notification_lookback_seconds: options.notification_lookback_seconds,
        activity_debounce_seconds: options.activity_debounce_seconds,
        heartbeat_on_activity: options.heartbeat_on_activity,
        status: "active".into(),
    };
    create_session_file(w, &guard, &session)?;
    commit_session_paths(
        w,
        &guard,
        &format!("wookie: start session {id}"),
        &[".gitignore".into(), format!("sessions/{id}/session.toml")],
    )?;
    Ok(session)
}

fn scan_sessions(w: &Wiki, request: &SessionListRequest) -> Result<SessionStorageScan> {
    let mut result = SessionStorageScan {
        sessions: vec![],
        warnings: vec![],
        warnings_total: 0,
        complete: true,
        entries_scanned: 0,
        bytes_scanned: 0,
    };
    let mut budget = StorageScanBudget::session_storage();
    let root = match w.contained_path(Path::new("sessions")) {
        Ok(root) => root,
        Err(error) => {
            push_storage_warning(
                &mut result.warnings,
                &mut result.warnings_total,
                StorageWarning {
                    path: sessions_dir(w).display().to_string(),
                    message: error.to_string(),
                },
            );
            result.complete = false;
            return Ok(result);
        }
    };
    if root.exists() {
        if let Err(error) = ensure_real_directory(&root, "sessions root") {
            push_storage_warning(
                &mut result.warnings,
                &mut result.warnings_total,
                StorageWarning {
                    path: root.display().to_string(),
                    message: error.to_string(),
                },
            );
            result.complete = false;
            return Ok(result);
        }
        let entries = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) => {
                push_storage_warning(
                    &mut result.warnings,
                    &mut result.warnings_total,
                    StorageWarning {
                        path: root.display().to_string(),
                        message: error.to_string(),
                    },
                );
                result.complete = false;
                return Ok(result);
            }
        };
        for entry in entries {
            if let Err(error) = budget.charge(1, 0, "session storage") {
                push_storage_warning(
                    &mut result.warnings,
                    &mut result.warnings_total,
                    StorageWarning {
                        path: root.display().to_string(),
                        message: format!("{error}; session scan is incomplete"),
                    },
                );
                result.complete = false;
                break;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    push_storage_warning(
                        &mut result.warnings,
                        &mut result.warnings_total,
                        StorageWarning {
                            path: root.display().to_string(),
                            message: format!("cannot inspect session entry: {error}"),
                        },
                    );
                    result.complete = false;
                    continue;
                }
            };
            let path = entry.path();
            let Some(id) = entry.file_name().to_str().map(str::to_string) else {
                push_storage_warning(
                    &mut result.warnings,
                    &mut result.warnings_total,
                    StorageWarning {
                        path: path.display().to_string(),
                        message: "session directory name is not valid UTF-8".into(),
                    },
                );
                result.complete = false;
                continue;
            };
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => {}
                Ok(_) => {
                    push_storage_warning(
                        &mut result.warnings,
                        &mut result.warnings_total,
                        StorageWarning {
                            path: path.display().to_string(),
                            message: "session entry is not a real directory".into(),
                        },
                    );
                    continue;
                }
                Err(error) => {
                    push_storage_warning(
                        &mut result.warnings,
                        &mut result.warnings_total,
                        StorageWarning {
                            path: path.display().to_string(),
                            message: format!("cannot inspect session entry type: {error}"),
                        },
                    );
                    result.complete = false;
                    continue;
                }
            }
            match load_session_with_budget(w, &id, Some(&mut budget)) {
                Ok((session, activity_warnings, activity_warnings_total)) => {
                    result.warnings_total = result
                        .warnings_total
                        .saturating_add(activity_warnings_total);
                    let remaining = MAX_STORAGE_WARNINGS.saturating_sub(result.warnings.len());
                    result
                        .warnings
                        .extend(activity_warnings.into_iter().take(remaining));
                    result.sessions.push(session);
                }
                Err(error) => {
                    let scan_limited = scan_limit_error(&error);
                    push_storage_warning(
                        &mut result.warnings,
                        &mut result.warnings_total,
                        StorageWarning {
                            path: path.display().to_string(),
                            message: error.to_string(),
                        },
                    );
                    if scan_limited {
                        result.complete = false;
                        break;
                    }
                }
            }
        }
    }

    let created_after = request
        .created_after
        .as_deref()
        .map(|value| parse_time("created-after", value))
        .transpose()?;
    let active_after = request
        .active_after
        .as_deref()
        .map(|value| parse_time("active-after", value))
        .transpose()?;
    let active_before = request
        .active_before
        .as_deref()
        .map(|value| parse_time("active-before", value))
        .transpose()?;
    let label_contains = request
        .label_contains
        .as_ref()
        .map(|value| value.to_lowercase());
    result.sessions.retain(|session| {
        (request.statuses.is_empty() || request.statuses.contains(&session.status))
            && (request.agents.is_empty() || request.agents.contains(&session.agent))
            && label_contains.as_ref().is_none_or(|needle| {
                session
                    .label
                    .as_ref()
                    .is_some_and(|label| label.to_lowercase().contains(needle))
            })
            && created_after.as_ref().is_none_or(|after| {
                parse_time("session created", &session.created_at)
                    .map(|created| created >= *after)
                    .unwrap_or(false)
            })
            && active_after.as_ref().is_none_or(|after| {
                session
                    .last_seen_at
                    .as_deref()
                    .and_then(|seen| parse_time("session activity", seen).ok())
                    .is_some_and(|seen| seen >= *after)
            })
            && active_before.as_ref().is_none_or(|before| {
                session
                    .last_seen_at
                    .as_deref()
                    .and_then(|seen| parse_time("session activity", seen).ok())
                    .is_some_and(|seen| seen <= *before)
            })
    });
    result
        .sessions
        .sort_by(|a, b| compare_timestamps(&a.created_at, &b.created_at).then(a.id.cmp(&b.id)));
    if request.newest_first {
        result.sessions.reverse();
    }
    result.entries_scanned = budget.entries;
    result.bytes_scanned = budget.bytes;
    Ok(result)
}

pub fn list_with_options(w: &Wiki, request: &SessionListRequest) -> Result<SessionListResult> {
    let limit = request.limit.unwrap_or(DEFAULT_SESSION_LIST_LIMIT);
    if limit == 0 {
        bail!("session list limit must be greater than zero");
    }
    if limit > MAX_SESSION_RESPONSE_LIMIT {
        bail!(
            "session list limit {limit} exceeds the hard response ceiling {MAX_SESSION_RESPONSE_LIMIT}"
        );
    }
    let scan = scan_sessions(w, request)?;
    let total_matches = scan.sessions.len();
    let start = request.cursor.min(total_matches);
    let end = start.saturating_add(limit).min(total_matches);
    let sessions = scan.sessions[start..end].to_vec();
    let returned = sessions.len();
    let omitted = total_matches.saturating_sub(end);
    Ok(SessionListResult {
        sessions,
        total_matches,
        returned,
        omitted,
        cursor: request.cursor,
        continuation: (end < total_matches).then_some(end),
        scan_complete: scan.complete,
        entries_scanned: scan.entries_scanned,
        bytes_scanned: scan.bytes_scanned,
        warnings_total: scan.warnings_total,
        warnings_omitted: scan.warnings_total.saturating_sub(scan.warnings.len()),
        warnings: scan.warnings,
    })
}

pub fn format_session_list(result: &SessionListResult, json: bool) -> Result<String> {
    if json {
        return Ok(serde_json::to_string(result)?);
    }
    let mut output = if result.sessions.is_empty() {
        "No matching sessions. Start one with `wookie session start`.".into()
    } else {
        result
            .sessions
            .iter()
            .map(|session| {
                format!(
                    "{}  {}  {}  {}{}",
                    session.id,
                    session.status,
                    session.agent,
                    session.created_at,
                    session
                        .label
                        .as_ref()
                        .map(|label| format!("  {label}"))
                        .unwrap_or_default()
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    output.push_str(&format!(
        "\n\nShowing {} of {} matching session(s).",
        result.returned, result.total_matches
    ));
    if let Some(cursor) = result.continuation {
        output.push_str(&format!(
            " {} remain; continue with the same filters plus `--cursor {cursor}`.",
            result.omitted
        ));
    }
    if !result.scan_complete {
        output.push_str(&format!(
            "\nStorage scan incomplete after {} entries / {} bytes; totals cover only the scanned prefix.",
            result.entries_scanned, result.bytes_scanned
        ));
    }
    if result.warnings_total > 0 {
        output.push_str(&format!(
            "\nWarnings: {} storage warning{} ({} detail record{} returned, {} omitted).",
            result.warnings_total,
            if result.warnings_total == 1 { "" } else { "s" },
            result.warnings.len(),
            if result.warnings.len() == 1 { "" } else { "s" },
            result.warnings_omitted,
        ));
    }
    Ok(output)
}

fn compact_session_summary(value: &str) -> (String, bool) {
    if value.len() <= MAX_SESSION_SHOW_SUMMARY_BYTES {
        return (value.to_string(), false);
    }
    let suffix = "…";
    let mut end = MAX_SESSION_SHOW_SUMMARY_BYTES.saturating_sub(suffix.len());
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    (format!("{}{}", &value[..end], suffix), true)
}

pub fn show_with_options(
    w: &Wiki,
    id: &str,
    request: &SessionShowRequest,
    json: bool,
) -> Result<String> {
    let limit = request.limit.unwrap_or(DEFAULT_SESSION_SHOW_LIMIT);
    if limit == 0 {
        bail!("session show limit must be greater than zero");
    }
    if limit > MAX_SESSION_RESPONSE_LIMIT {
        bail!(
            "session show limit {limit} exceeds the hard response ceiling {MAX_SESSION_RESPONSE_LIMIT}"
        );
    }
    let session = load_session(w, id)?;
    let scan = scan_notifications(w);
    let mut sent: Vec<StoredNotification> = scan
        .notifications
        .into_iter()
        .filter(|notification| notification.meta.source_session == id)
        .collect();
    // Recent work is the useful default for a compact operational summary.
    sent.reverse();
    let total_notifications_sent = sent.len();
    let start = request.cursor.min(total_notifications_sent);
    let end = start.saturating_add(limit).min(total_notifications_sent);
    let notifications_sent = sent[start..end]
        .iter()
        .map(|notification| {
            let meta = &notification.meta;
            let (summary, summary_truncated) = compact_session_summary(&meta.summary);
            SessionNotificationSummary {
                id: meta.id.clone(),
                summary,
                summary_truncated,
                kind: meta.kind,
                importance: meta.importance,
                created_at: meta.created_at.clone(),
                affected_path_count: meta.paths.len(),
                target_count: meta.targets.len(),
            }
        })
        .collect::<Vec<_>>();
    let result = SessionShowResult {
        session,
        notifications_returned: notifications_sent.len(),
        notifications_omitted: total_notifications_sent.saturating_sub(end),
        notifications_sent,
        total_notifications_sent,
        cursor: request.cursor,
        continuation: (end < total_notifications_sent).then_some(end),
        scan_complete: scan.complete,
        entries_scanned: scan.entries_scanned,
        bytes_scanned: scan.bytes_scanned,
        warnings_total: scan.warnings_total,
        warnings_omitted: scan.warnings_total.saturating_sub(scan.warnings.len()),
        warnings: scan.warnings,
    };
    if json {
        return Ok(serde_json::to_string(&result)?);
    }
    let mut output = format!(
        "Session: {}\nAgent: {}\nLabel: {}\nStatus: {}\nCreated: {}\nUpdated: {}\nLast seen: {}\nNotifications sent: {} total; {} returned",
        result.session.id,
        result.session.agent,
        result.session.label.as_deref().unwrap_or("-"),
        result.session.status,
        result.session.created_at,
        result.session.updated_at,
        result.session.last_seen_at.as_deref().unwrap_or("-"),
        result.total_notifications_sent,
        result.notifications_returned,
    );
    for notification in &result.notifications_sent {
        output.push_str(&format!(
            "\n  {}  [{} / {}] {}",
            notification.id, notification.kind, notification.importance, notification.summary
        ));
    }
    if let Some(cursor) = result.continuation {
        output.push_str(&format!(
            "\n{} more known notification(s); continue with `--cursor {cursor}`.",
            result.notifications_omitted
        ));
    }
    if !result.scan_complete {
        output.push_str(&format!(
            "\nNotification storage scan incomplete after {} entries / {} bytes; totals cover only the scanned prefix.",
            result.entries_scanned, result.bytes_scanned
        ));
    }
    if result.warnings_total > 0 {
        output.push_str(&format!(
            "\nWarnings: {} storage warning(s) ({} detail record(s) returned, {} omitted).",
            result.warnings_total,
            result.warnings.len(),
            result.warnings_omitted,
        ));
    }
    Ok(output)
}

pub fn heartbeat(w: &Wiki, id: &str, force: bool) -> Result<Session> {
    let guard = w.acquire_mutation_guard()?;
    let activity = record_activity(w, &guard, id, "heartbeat", None, force)?;
    if let Some(path) = activity.history_path.as_ref() {
        commit_session_paths(
            w,
            &guard,
            &format!("wookie: heartbeat session {id}"),
            std::slice::from_ref(path),
        )?;
    }
    Ok(activity.session)
}

pub fn close(w: &Wiki, id: &str, json: bool) -> Result<String> {
    let guard = w.acquire_mutation_guard()?;
    let activity = record_activity(w, &guard, id, "close", Some("closed"), true)?;
    if let Some(path) = activity.history_path.as_ref() {
        commit_session_paths(
            w,
            &guard,
            &format!("wookie: close session {id}"),
            std::slice::from_ref(path),
        )?;
    }
    let session = activity.session;
    if json {
        Ok(serde_json::json!({"session": session}).to_string())
    } else {
        Ok(format!("Closed session '{id}'."))
    }
}

fn same_idempotent_payload(existing: &Notification, request: &NotifyRequest, body: &str) -> bool {
    existing.meta.source_session == request.source_session
        && existing.meta.idempotency_key == request.idempotency_key
        && existing.meta.summary == request.summary
        && existing.meta.kind == request.kind
        && existing.meta.importance == request.importance
        && existing.meta.paths == request.paths
        && existing.meta.targets == request.targets
        && existing.meta.metadata == request.metadata
        && existing.body == body
}

pub fn notify_with_request(w: &Wiki, mut request: NotifyRequest) -> Result<Notification> {
    let guard = w.acquire_mutation_guard()?;
    let session = load_session(w, &request.source_session)?;
    if session.status != "active" {
        bail!(
            "session '{}' is closed; start a new session before notifying",
            request.source_session
        );
    }
    request.summary = clean_field("summary", &request.summary)?;
    if request.summary.len() > request.limits.max_summary_bytes {
        bail!(
            "summary must be at most {} bytes",
            request.limits.max_summary_bytes
        );
    }
    if request.paths.len() > request.limits.max_paths {
        bail!("at most {} paths are allowed", request.limits.max_paths);
    }
    request.paths = request
        .paths
        .into_iter()
        .map(|path| clean_project_path("path", &path, request.limits.max_path_bytes))
        .collect::<Result<BTreeSet<_>>>()?
        .into_iter()
        .collect();
    if request.paths.len() > request.limits.max_paths {
        bail!("at most {} paths are allowed", request.limits.max_paths);
    }
    if request.targets.len() > request.limits.max_targets {
        bail!("at most {} targets are allowed", request.limits.max_targets);
    }
    request.targets = request
        .targets
        .into_iter()
        .map(|target| {
            validate_generated_id(&target, "session")?;
            if target == request.source_session {
                bail!("a notification cannot target its own source session");
            }
            let target_session = load_session(w, &target)?;
            if target_session.status != "active" {
                bail!("target session '{target}' is not active");
            }
            Ok(target)
        })
        .collect::<Result<BTreeSet<_>>>()?
        .into_iter()
        .collect();
    request.idempotency_key = clean_optional_field(
        "idempotency key",
        request.idempotency_key,
        request.limits.max_idempotency_key_bytes,
    )?;
    if request.metadata.len() > request.limits.max_metadata_entries {
        bail!(
            "at most {} metadata entries are allowed",
            request.limits.max_metadata_entries
        );
    }
    request.metadata = request
        .metadata
        .into_iter()
        .map(|(key, value)| {
            let key = clean_field("metadata key", &key)?;
            if key.len() > request.limits.max_metadata_key_bytes {
                bail!(
                    "metadata key must be at most {} bytes",
                    request.limits.max_metadata_key_bytes
                );
            }
            let value = clean_field("metadata value", &value)?;
            if value.len() > request.limits.max_metadata_value_bytes {
                bail!(
                    "metadata value must be at most {} bytes",
                    request.limits.max_metadata_value_bytes
                );
            }
            Ok((key, value))
        })
        .collect::<Result<_>>()?;
    if let Some(git) = &mut request.git {
        git.branch = clean_optional_field(
            "git branch",
            git.branch.take(),
            request.limits.max_git_branch_bytes,
        )?;
        git.commit = clean_optional_field(
            "git commit",
            git.commit.take(),
            request.limits.max_git_commit_bytes,
        )?;
        git.worktree = clean_optional_field(
            "git worktree",
            git.worktree.take(),
            request.limits.max_git_worktree_bytes,
        )?;
        if git.dirty_paths.len() > request.limits.max_git_dirty_paths {
            bail!(
                "at most {} dirty Git paths are allowed",
                request.limits.max_git_dirty_paths
            );
        }
        git.dirty_paths = git
            .dirty_paths
            .drain(..)
            .map(|path| clean_project_path("dirty path", &path, request.limits.max_path_bytes))
            .collect::<Result<BTreeSet<_>>>()?
            .into_iter()
            .collect();
        if git.dirty_paths.len() > request.limits.max_git_dirty_paths {
            bail!(
                "at most {} dirty Git paths are allowed",
                request.limits.max_git_dirty_paths
            );
        }
    }

    let body = request
        .body
        .as_deref()
        .filter(|body| !body.trim().is_empty())
        .unwrap_or(&request.summary)
        .trim()
        .to_string();
    validate_body(&body)?;
    if body.len() > request.limits.max_body_bytes {
        bail!(
            "body must be at most {} bytes",
            request.limits.max_body_bytes
        );
    }

    if let Some(key) = &request.idempotency_key {
        let scan = scan_notifications(w);
        if !scan.complete {
            bail!(
                "notification storage scan is incomplete; refusing an idempotent publish while the existing key may be hidden"
            );
        }
        if let Some(stored) = scan.notifications.into_iter().find(|notification| {
            notification.meta.source_session == request.source_session
                && notification.meta.idempotency_key.as_ref() == Some(key)
        }) {
            let existing = parse_notification_file(w, &stored.path, true)?;
            if same_idempotent_payload(&existing, &request, &body) {
                return Ok(existing);
            }
            bail!("idempotency key '{key}' was already used with a different notification payload");
        }
    }

    let dir = ensure_child_dir(&session_dir(w, &request.source_session)?, "notifications")?;
    let mut created = None;
    for attempt in 0..100 {
        let id = request.idempotency_key.as_ref().map_or_else(
            || unique_id("notify", attempt),
            |key| {
                format!(
                    "notify-idem-{:016x}",
                    stable_hash(&format!("{}\0{key}", request.source_session))
                )
            },
        );
        let notification = Notification {
            meta: NotificationMeta {
                id: id.clone(),
                source_session: request.source_session.clone(),
                summary: request.summary.clone(),
                kind: request.kind,
                importance: request.importance,
                created_at: now(),
                paths: request.paths.clone(),
                targets: request.targets.clone(),
                idempotency_key: request.idempotency_key.clone(),
                git: request.git.clone(),
                metadata: request.metadata.clone(),
            },
            body: body.clone(),
        };
        let path = dir.join(format!("{id}.md"));
        let rendered = render_notification(&notification)?;
        let file_limit = notification_file_limit(w);
        if u64::try_from(rendered.len()).unwrap_or(u64::MAX) > file_limit {
            bail!("notification payload exceeds the {file_limit}-byte storage safety limit");
        }
        match write_new(&path, &rendered) {
            Ok(()) => {
                created = Some(notification);
                break;
            }
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::AlreadyExists) =>
            {
                if request.idempotency_key.is_some() {
                    let existing = parse_notification_file(w, &path, true).with_context(|| {
                        format!(
                            "idempotent notification file {} is unusable",
                            path.display()
                        )
                    })?;
                    if same_idempotent_payload(&existing, &request, &body) {
                        return Ok(existing);
                    }
                    bail!(
                        "idempotency key collision for notification '{id}': existing payload differs"
                    );
                }
                continue;
            }
            Err(error) => return Err(error),
        }
    }
    let notification = created.context("could not allocate a unique notification id")?;
    let activity = record_activity(w, &guard, &request.source_session, "notify", None, false)?;
    let mut paths = vec![format!(
        "sessions/{}/notifications/{}.md",
        request.source_session, notification.meta.id
    )];
    if let Some(path) = activity.history_path {
        paths.push(path);
    }
    commit_session_paths(
        w,
        &guard,
        &format!("wookie: notify {}", notification.meta.id),
        &paths,
    )?;
    Ok(notification)
}

pub fn inbox_with_request(w: &Wiki, request: &InboxRequest) -> Result<InboxResult> {
    // Polling may publish a debounced activity event, so the whole operation
    // uses the same boundary as explicit session mutations.
    let guard = w.acquire_mutation_guard()?;
    let limit = request.limit.unwrap_or(w.sessions.poll_limit);
    if !(1..=w.sessions.poll_limit).contains(&limit) {
        bail!(
            "notification limit {limit} must be between 1 and configured sessions.poll_limit {}",
            w.sessions.poll_limit
        );
    }
    let session = load_session(w, &request.session_id)?;
    let (legacy, legacy_warning) = load_legacy_inbox(w, &request.session_id)?;
    let prepared_filter = prepare_notification_filter(&request.filter)?;
    let mut scan = scan_notifications(w);
    if !scan.complete {
        bail!(
            "notification storage scan is incomplete; refusing to poll an inbox while notices may be hidden"
        );
    }
    if let Some(warning) = legacy_warning {
        push_storage_warning(&mut scan.warnings, &mut scan.warnings_total, warning);
    }

    let lookback_seconds = request
        .lookback_seconds
        .unwrap_or(session.notification_lookback_seconds);
    if lookback_seconds > max_session_lookback_seconds() {
        bail!(
            "notification lookback exceeds the hard ceiling of {} hours",
            crate::config::MAX_SESSION_LOOKBACK_HOURS
        );
    }
    let default_cutoff = parse_time("session created", &session.created_at)?
        .checked_sub_signed(seconds_duration("notification lookback", lookback_seconds)?)
        .context("notification lookback produces an out-of-range timestamp")?;
    let mut listings = vec![];
    for notification in scan.notifications {
        if notification.meta.source_session == request.session_id
            || !delivered_to(&notification.meta, &request.session_id)
        {
            continue;
        }
        if !request.include_acknowledged
            && parse_time("notification", &notification.meta.created_at)? < default_cutoff
        {
            continue;
        }
        match matches_filter(w, &notification, &request.filter, &prepared_filter) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(error) => {
                push_storage_warning(
                    &mut scan.warnings,
                    &mut scan.warnings_total,
                    StorageWarning {
                        path: notification.path.display().to_string(),
                        message: format!("notification content ignored while filtering: {error:#}"),
                    },
                );
                continue;
            }
        }
        let state = notification_state(w, &request.session_id, &notification.meta.id, &legacy)?;
        if !request.include_acknowledged && state.is_some() {
            continue;
        }
        listings.push(NotificationListing {
            notification: notification.meta,
            state,
        });
    }
    listings.sort_by(|a, b| {
        compare_timestamps(&a.notification.created_at, &b.notification.created_at)
            .then(a.notification.id.cmp(&b.notification.id))
    });
    if request.newest_first {
        listings.reverse();
    }
    let total = listings.len();
    if request.offset > total {
        bail!(
            "notification offset {} exceeds the {total} matching notification(s)",
            request.offset
        );
    }
    let end = request.offset.saturating_add(limit).min(total);
    let returned = end - request.offset;
    let omitted = total - returned;
    let continuation = (end < total).then_some(InboxContinuation {
        offset: end,
        limit,
        remaining: total - end,
    });
    let listings = listings
        .into_iter()
        .skip(request.offset)
        .take(returned)
        .collect();
    let warnings_omitted = scan.warnings_total.saturating_sub(scan.warnings.len());
    let activity = record_activity(w, &guard, &request.session_id, "poll-inbox", None, false)?;
    if let Some(path) = activity.history_path {
        commit_session_paths(
            w,
            &guard,
            &format!("wookie: session activity {}", request.session_id),
            &[path],
        )?;
    }
    Ok(InboxResult {
        session: request.session_id.clone(),
        unread_only: !request.include_acknowledged,
        newest_first: request.newest_first,
        offset: request.offset,
        total,
        returned,
        omitted,
        continuation,
        notifications: listings,
        warnings_total: scan.warnings_total,
        warnings_omitted,
        warnings: scan.warnings,
    })
}

pub fn format_inbox(result: &InboxResult, json: bool) -> Result<String> {
    if json {
        return Ok(serde_json::to_string(result)?);
    }
    if result.notifications.is_empty() {
        let mut output = if result.total > 0 {
            format!(
                "No notifications at offset {} ({} matching notification(s); {} omitted).",
                result.offset, result.total, result.omitted
            )
        } else if !result.unread_only {
            "No notifications from other sessions.".to_string()
        } else {
            "No unread notifications.".to_string()
        };
        if result.warnings_total > 0 {
            output.push_str(&format!(
                "\nWarnings: {} corrupt storage item(s) skipped ({} detail record(s) returned, {} omitted).",
                result.warnings_total,
                result.warnings.len(),
                result.warnings_omitted
            ));
        }
        return Ok(output);
    }
    let mut output = result
        .notifications
        .iter()
        .map(|listing| {
            let notification = &listing.notification;
            let paths = if notification.paths.is_empty() {
                String::new()
            } else {
                format!("\n  Paths: {}", notification.paths.join(", "))
            };
            let targets = if notification.targets.is_empty() {
                String::new()
            } else {
                format!("\n  Targets: {}", notification.targets.join(", "))
            };
            let state = listing
                .state
                .as_ref()
                .map(|state| format!("\n  State: {state}"))
                .unwrap_or_default();
            format!(
                "{}\n  From: {}\n  Summary: {}\n  Kind: {}\n  Importance: {}{}{}{}",
                notification.id,
                notification.source_session,
                notification.summary,
                notification.kind,
                notification.importance,
                paths,
                targets,
                state
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let order = if result.newest_first {
        "newest first"
    } else {
        "oldest first"
    };
    output.push_str(&format!(
        "\n\nShowing {} of {} matching notification(s) ({order}, offset {}; {} omitted).",
        result.returned, result.total, result.offset, result.omitted
    ));
    if let Some(continuation) = &result.continuation {
        output.push_str(&format!(
            " Continue with the same filters, order, and limit plus --offset {} ({} remaining).",
            continuation.offset, continuation.remaining
        ));
    }
    if result.warnings_total > 0 {
        output.push_str(&format!(
            "\n\nWarnings: {} corrupt storage item(s) skipped ({} detail record(s) returned, {} omitted).",
            result.warnings_total,
            result.warnings.len(),
            result.warnings_omitted
        ));
    }
    Ok(output)
}

pub fn read_notification(w: &Wiki, session_id: &str, id: &str, json: bool) -> Result<String> {
    let guard = w.acquire_mutation_guard()?;
    load_session(w, session_id)?;
    let notification = find_notification(w, id)?;
    if !delivered_to(&notification.meta, session_id) {
        bail!("notification '{id}' is targeted to other sessions");
    }
    write_acknowledgement(w, &guard, session_id, id, "read")?;
    let activity = record_activity(w, &guard, session_id, "read-notification", None, false)?;
    if let Some(path) = activity.history_path {
        commit_session_paths(
            w,
            &guard,
            &format!("wookie: session activity {session_id}"),
            &[path],
        )?;
    }
    if json {
        return Ok(serde_json::json!({
            "notification": notification.meta,
            "body": notification.body,
            "state": "read"
        })
        .to_string());
    }
    Ok(format!(
        "Notification: {}\nFrom: {}\nKind: {}\nImportance: {}\nSummary: {}\nPaths: {}\n\n{}",
        notification.meta.id,
        notification.meta.source_session,
        notification.meta.kind,
        notification.meta.importance,
        notification.meta.summary,
        if notification.meta.paths.is_empty() {
            "-".into()
        } else {
            notification.meta.paths.join(", ")
        },
        notification.body
    ))
}

pub fn dismiss_notification(w: &Wiki, session_id: &str, id: &str, json: bool) -> Result<String> {
    let guard = w.acquire_mutation_guard()?;
    load_session(w, session_id)?;
    let notification = find_stored_notification(w, id)?;
    if !delivered_to(&notification.meta, session_id) {
        bail!("notification '{id}' is targeted to other sessions");
    }
    write_acknowledgement(w, &guard, session_id, id, "dismissed")?;
    let activity = record_activity(w, &guard, session_id, "dismiss-notification", None, false)?;
    if let Some(path) = activity.history_path {
        commit_session_paths(
            w,
            &guard,
            &format!("wookie: session activity {session_id}"),
            &[path],
        )?;
    }
    if json {
        Ok(serde_json::json!({"notification": id, "state": "dismissed"}).to_string())
    } else {
        Ok(format!(
            "Dismissed notification '{id}' for session '{session_id}'."
        ))
    }
}

pub fn prune_sessions(w: &Wiki, request: &PruneRequest) -> Result<PruneResult> {
    if !request.closed_only
        && request.older_than_seconds.is_none()
        && request.inactive_before.is_none()
        && request.keep_latest == 0
    {
        bail!("refusing unbounded session prune; configure at least one retention constraint");
    }
    // A dry run is genuinely read-only. An applying prune acquires before it
    // enumerates candidates so selection, deletion, and history all observe a
    // single serialized state.
    let guard = if request.dry_run {
        None
    } else {
        Some(w.acquire_mutation_guard()?)
    };
    let listed = scan_sessions(
        w,
        &SessionListRequest {
            newest_first: true,
            ..SessionListRequest::default()
        },
    )?;
    if !listed.complete {
        bail!("session storage scan is incomplete; refusing to compute a prune set");
    }
    let inactive_before = request
        .inactive_before
        .as_deref()
        .map(|value| parse_time("inactive-before", value))
        .transpose()?;
    let age_cutoff = request
        .older_than_seconds
        .map(|seconds| {
            checked_cutoff(
                "session retention",
                seconds_duration("session retention", seconds)?,
            )
        })
        .transpose()?;
    let mut result = PruneResult {
        dry_run: request.dry_run,
        sessions: vec![],
        warnings: listed.warnings,
    };
    for (index, session) in listed.sessions.into_iter().enumerate() {
        if index < request.keep_latest || (request.closed_only && session.status != "closed") {
            continue;
        }
        let last_seen = session
            .last_seen_at
            .as_deref()
            .unwrap_or(&session.updated_at);
        let Ok(last_seen) = parse_time("session activity", last_seen) else {
            result.warnings.push(StorageWarning {
                path: session.id,
                message: "invalid session activity timestamp".into(),
            });
            continue;
        };
        if age_cutoff.is_some_and(|cutoff| last_seen > cutoff)
            || inactive_before.is_some_and(|cutoff| last_seen > cutoff)
        {
            continue;
        }
        let dir = session_dir(w, &session.id)?;
        ensure_real_directory(&dir, "session directory")?;
        result.sessions.push(session.id);
        if !request.dry_run {
            fs::remove_dir_all(&dir)
                .with_context(|| format!("pruning session directory {}", dir.display()))?;
        }
    }
    if !request.dry_run && !result.sessions.is_empty() {
        let paths = result
            .sessions
            .iter()
            .map(|id| format!("sessions/{id}"))
            .collect::<Vec<_>>();
        commit_session_paths(
            w,
            guard
                .as_ref()
                .context("applying a session prune requires the wiki mutation guard")?,
            &format!("wookie: prune {} sessions", result.sessions.len()),
            &paths,
        )?;
    }
    Ok(result)
}

pub fn format_prune(result: &PruneResult, json: bool) -> Result<String> {
    if json {
        return Ok(serde_json::to_string(result)?);
    }
    let action = if result.dry_run {
        "Would prune"
    } else {
        "Pruned"
    };
    let mut output = if result.sessions.is_empty() {
        format!("{action} no sessions.")
    } else {
        format!(
            "{action} {} session(s):\n{}",
            result.sessions.len(),
            result.sessions.join("\n")
        )
    };
    if !result.warnings.is_empty() {
        output.push_str(&format!(
            "\n\nWarnings: {} corrupt session entr{} skipped.",
            result.warnings.len(),
            if result.warnings.len() == 1 {
                "y"
            } else {
                "ies"
            }
        ));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    #[cfg(unix)]
    use std::sync::mpsc;
    use std::thread;

    struct Fixture {
        home: PathBuf,
        wiki: Wiki,
    }

    impl Fixture {
        fn new() -> Self {
            let home = std::env::temp_dir().join(unique_id("wookie-session-test", 0));
            let wiki_dir = home.join("test");
            fs::create_dir_all(wiki_dir.join("pages")).unwrap();
            fs::write(
                wiki_dir.join("wookie.toml"),
                "name = \"test\"\nauto_commit = false\nproject_roots = []\n",
            )
            .unwrap();
            let wiki = crate::wiki::open(&home, "test").unwrap();
            Self { home, wiki }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.home);
        }
    }

    fn start_test_session(wiki: &Wiki, agent: &str, lookback: u64) -> Session {
        start_with_options(
            wiki,
            StartOptions {
                agent: Some(agent.into()),
                notification_lookback_seconds: lookback,
                activity_debounce_seconds: 60,
                ..StartOptions::default()
            },
        )
        .unwrap()
    }

    fn git(root: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
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
    }

    fn git_project(root: &Path) {
        fs::create_dir_all(root).unwrap();
        git(root, &["init", "-q"]);
        fs::write(root.join("old name.txt"), "tracked\n").unwrap();
        git(root, &["add", "old name.txt"]);
        git(root, &["commit", "-q", "-m", "initial"]);
    }

    fn test_notification(wiki: &Wiki, sender: &Session, summary: &str) -> Notification {
        notify_with_request(
            wiki,
            NotifyRequest {
                source_session: sender.id.clone(),
                summary: summary.into(),
                ..NotifyRequest::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn git_context_is_project_bound_and_parses_both_sides_of_renames() {
        let mut fixture = Fixture::new();
        let project = fixture.home.join("project");
        let unrelated = fixture.home.join("unrelated");
        git_project(&project);
        git_project(&unrelated);
        fixture.wiki.config.project_roots = vec![project.to_string_lossy().into_owned()];

        git(&project, &["mv", "old name.txt", "new name.txt"]);
        let context = capture_git_context(&fixture.wiki, &project).unwrap();
        assert_eq!(
            context.dirty_paths,
            vec!["new name.txt".to_string(), "old name.txt".to_string()]
        );
        assert!(context.commit.is_some());
        assert_eq!(
            PathBuf::from(context.worktree.unwrap())
                .canonicalize()
                .unwrap(),
            project.canonicalize().unwrap()
        );

        assert!(
            capture_git_context(&fixture.wiki, &unrelated).is_none(),
            "an explicit wiki must never inherit Git metadata from another repository"
        );
    }

    #[test]
    fn git_context_overflow_is_best_effort_and_never_unbounded() {
        let mut fixture = Fixture::new();
        let project = fixture.home.join("overflow-project");
        git_project(&project);
        fixture.wiki.config.project_roots = vec![project.to_string_lossy().into_owned()];
        fixture.wiki.sessions.max_git_dirty_paths = 1;
        fs::write(project.join("one.txt"), "one").unwrap();
        fs::write(project.join("two.txt"), "two").unwrap();

        let context = capture_git_context(&fixture.wiki, &project).unwrap();
        assert!(
            context.dirty_paths.is_empty(),
            "an over-limit status must not publish a misleading partial inventory"
        );

        fixture.wiki.sessions.max_git_worktree_bytes = 1;
        assert!(
            capture_git_context(&fixture.wiki, &project).is_none(),
            "an over-limit required worktree field must omit Git context"
        );
    }

    #[test]
    fn notification_read_limit_cannot_saturate_past_the_absolute_ceiling() {
        let mut fixture = Fixture::new();
        fixture.wiki.sessions.max_summary_bytes = usize::MAX;
        fixture.wiki.sessions.max_body_bytes = usize::MAX;
        fixture.wiki.sessions.max_paths = usize::MAX;
        fixture.wiki.sessions.max_path_bytes = usize::MAX;
        fixture.wiki.sessions.max_targets = usize::MAX;
        fixture.wiki.sessions.max_idempotency_key_bytes = usize::MAX;
        fixture.wiki.sessions.max_metadata_entries = usize::MAX;
        fixture.wiki.sessions.max_metadata_key_bytes = usize::MAX;
        fixture.wiki.sessions.max_metadata_value_bytes = usize::MAX;
        fixture.wiki.sessions.max_git_dirty_paths = usize::MAX;
        fixture.wiki.sessions.max_git_branch_bytes = usize::MAX;
        fixture.wiki.sessions.max_git_commit_bytes = usize::MAX;
        fixture.wiki.sessions.max_git_worktree_bytes = usize::MAX;
        assert_eq!(
            notification_file_limit(&fixture.wiki),
            MAX_NOTIFICATION_FILE_BYTES
        );
    }

    fn storage_snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
        fn visit(root: &Path, path: &Path, snapshot: &mut BTreeMap<String, Vec<u8>>) {
            if !path.exists() {
                return;
            }
            let mut entries = fs::read_dir(path)
                .unwrap()
                .map(|entry| entry.unwrap())
                .collect::<Vec<_>>();
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let path = entry.path();
                let relative = path.strip_prefix(root).unwrap().to_string_lossy();
                let metadata = fs::symlink_metadata(&path).unwrap();
                if metadata.is_dir() {
                    snapshot.insert(format!("{relative}/"), vec![]);
                    visit(root, &path, snapshot);
                } else if metadata.is_file() {
                    snapshot.insert(relative.into_owned(), fs::read(&path).unwrap());
                }
            }
        }

        let mut snapshot = BTreeMap::new();
        visit(root, root, &mut snapshot);
        snapshot
    }

    fn assert_recovery_blocked<T>(result: Result<T>) {
        let error = match result {
            Ok(_) => panic!("recovery state must block session mutation"),
            Err(error) => error,
        };
        let message = format!("{error:#}");
        assert!(
            message.contains("interrupted publication journal")
                || message.contains("unresolved ingest reconciliation marker"),
            "unexpected recovery error: {message}"
        );
    }

    #[cfg(unix)]
    fn unix_mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[cfg(unix)]
    #[test]
    fn created_session_directories_and_immutable_records_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let fixture = Fixture::new();
        // A pre-existing wiki directory belongs to the user. Session startup
        // must not silently tighten it while creating private descendants.
        fs::set_permissions(&fixture.wiki.dir, fs::Permissions::from_mode(0o755)).unwrap();

        let sender = start_test_session(&fixture.wiki, "sender", 0);
        let receiver = start_test_session(&fixture.wiki, "receiver", 60);
        heartbeat(&fixture.wiki, &sender.id, true).unwrap();
        let notification = test_notification(&fixture.wiki, &sender, "private state");
        read_notification(&fixture.wiki, &receiver.id, &notification.meta.id, false).unwrap();

        assert_eq!(unix_mode(&fixture.wiki.dir), 0o755);
        for directory in [
            sessions_dir(&fixture.wiki),
            session_dir(&fixture.wiki, &sender.id).unwrap(),
            session_dir(&fixture.wiki, &receiver.id).unwrap(),
            activity_dir(&fixture.wiki, &sender.id).unwrap(),
            session_dir(&fixture.wiki, &sender.id)
                .unwrap()
                .join("notifications"),
            acknowledgements_dir(&fixture.wiki, &receiver.id).unwrap(),
        ] {
            assert_eq!(unix_mode(&directory), 0o700, "{}", directory.display());
        }

        let activity = fs::read_dir(activity_dir(&fixture.wiki, &sender.id).unwrap())
            .unwrap()
            .find_map(|entry| {
                let path = entry.ok()?.path();
                (path.extension().and_then(|value| value.to_str()) == Some("toml")).then_some(path)
            })
            .expect("forced heartbeat writes an activity record");
        for file in [
            session_path(&fixture.wiki, &sender.id).unwrap(),
            activity,
            session_dir(&fixture.wiki, &sender.id)
                .unwrap()
                .join(format!("notifications/{}.md", notification.meta.id)),
            acknowledgements_dir(&fixture.wiki, &receiver.id)
                .unwrap()
                .join(format!("{}.read", notification.meta.id)),
        ] {
            assert_eq!(unix_mode(&file), 0o600, "{}", file.display());
        }
    }

    #[test]
    fn concurrent_acknowledgements_do_not_lose_state() {
        let fixture = Fixture::new();
        let sender = start_test_session(&fixture.wiki, "sender", 0);
        let receiver = start_test_session(&fixture.wiki, "receiver", 0);
        let notifications: Vec<String> = (0..16)
            .map(|index| test_notification(&fixture.wiki, &sender, &format!("change {index}")))
            .map(|notification| notification.meta.id)
            .collect();

        let handles: Vec<_> = notifications
            .into_iter()
            .map(|notification_id| {
                let home = fixture.home.clone();
                let receiver_id = receiver.id.clone();
                thread::spawn(move || {
                    let wiki = crate::wiki::open(&home, "test").unwrap();
                    read_notification(&wiki, &receiver_id, &notification_id, false).unwrap();
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }

        let inbox = inbox_with_request(
            &fixture.wiki,
            &InboxRequest {
                session_id: receiver.id,
                ..InboxRequest::default()
            },
        )
        .unwrap();
        assert!(inbox.notifications.is_empty());
    }

    #[test]
    fn start_uses_timestamp_lookback_instead_of_history_snapshot() {
        let fixture = Fixture::new();
        let sender = start_test_session(&fixture.wiki, "sender", 0);
        test_notification(&fixture.wiki, &sender, "existing change");

        let caught_up = start_test_session(&fixture.wiki, "caught-up", 0);
        let caught_up_inbox = inbox_with_request(
            &fixture.wiki,
            &InboxRequest {
                session_id: caught_up.id.clone(),
                ..InboxRequest::default()
            },
        )
        .unwrap();
        assert!(caught_up_inbox.notifications.is_empty());
        assert!(!inbox_path(&fixture.wiki, &caught_up.id).unwrap().exists());

        let with_lookback = start_test_session(&fixture.wiki, "lookback", 60);
        let lookback_inbox = inbox_with_request(
            &fixture.wiki,
            &InboxRequest {
                session_id: with_lookback.id,
                ..InboxRequest::default()
            },
        )
        .unwrap();
        assert_eq!(lookback_inbox.notifications.len(), 1);
    }

    #[test]
    fn session_time_overrides_cannot_exceed_absolute_ceilings() {
        let fixture = Fixture::new();
        let error = start_with_options(
            &fixture.wiki,
            StartOptions {
                notification_lookback_seconds: max_session_lookback_seconds() + 1,
                ..StartOptions::default()
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("lookback exceeds"), "{error:#}");

        let error = start_with_options(
            &fixture.wiki,
            StartOptions {
                activity_debounce_seconds: crate::config::MAX_SESSION_ACTIVITY_DEBOUNCE_SECONDS + 1,
                ..StartOptions::default()
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("activity debounce"), "{error:#}");
        assert!(!sessions_dir(&fixture.wiki).exists());

        let receiver = start_test_session(&fixture.wiki, "receiver", 0);
        let error = inbox_with_request(
            &fixture.wiki,
            &InboxRequest {
                session_id: receiver.id,
                lookback_seconds: Some(max_session_lookback_seconds() + 1),
                ..InboxRequest::default()
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("lookback exceeds"), "{error:#}");
    }

    #[test]
    fn legacy_inbox_states_remain_acknowledged() {
        let fixture = Fixture::new();
        let sender = start_test_session(&fixture.wiki, "sender", 0);
        let receiver = start_test_session(&fixture.wiki, "receiver", 0);
        let notification = test_notification(&fixture.wiki, &sender, "legacy read");
        let legacy = Inbox {
            states: BTreeMap::from([(notification.meta.id, "read".into())]),
        };
        fs::write(
            inbox_path(&fixture.wiki, &receiver.id).unwrap(),
            toml::to_string_pretty(&legacy).unwrap(),
        )
        .unwrap();

        let result = inbox_with_request(
            &fixture.wiki,
            &InboxRequest {
                session_id: receiver.id,
                ..InboxRequest::default()
            },
        )
        .unwrap();
        assert!(result.notifications.is_empty());
    }

    #[test]
    fn corrupt_notification_is_a_warning_not_an_outage() {
        let fixture = Fixture::new();
        let sender = start_test_session(&fixture.wiki, "sender", 0);
        let receiver = start_test_session(&fixture.wiki, "receiver", 0);
        let valid = test_notification(&fixture.wiki, &sender, "valid");
        let corrupt_path = session_dir(&fixture.wiki, &sender.id)
            .unwrap()
            .join("notifications/notify-20260721-deadbeef.md");
        fs::write(&corrupt_path, "not frontmatter").unwrap();

        let result = inbox_with_request(
            &fixture.wiki,
            &InboxRequest {
                session_id: receiver.id,
                ..InboxRequest::default()
            },
        )
        .unwrap();
        assert_eq!(result.notifications.len(), 1);
        assert_eq!(result.notifications[0].notification.id, valid.meta.id);
        assert_eq!(result.warnings.len(), 1);
        assert_eq!(result.warnings[0].path, corrupt_path.display().to_string());
    }

    #[test]
    fn idempotency_targets_and_filters_are_enforced() {
        let fixture = Fixture::new();
        let sender = start_test_session(&fixture.wiki, "sender", 0);
        let target = start_test_session(&fixture.wiki, "target", 60);
        let bystander = start_test_session(&fixture.wiki, "bystander", 60);
        let request = || NotifyRequest {
            source_session: sender.id.clone(),
            summary: "targeted source change".into(),
            kind: NotificationKind::CodeChange,
            importance: Importance::High,
            paths: vec!["src/sessions.rs".into()],
            targets: vec![target.id.clone()],
            idempotency_key: Some("operation-42".into()),
            git: Some(GitContext {
                branch: Some("main".into()),
                ..GitContext::default()
            }),
            ..NotifyRequest::default()
        };
        let first = notify_with_request(&fixture.wiki, request()).unwrap();
        let second = notify_with_request(&fixture.wiki, request()).unwrap();
        assert_eq!(first.meta.id, second.meta.id);

        let target_result = inbox_with_request(
            &fixture.wiki,
            &InboxRequest {
                session_id: target.id,
                filter: NotificationFilter {
                    kinds: vec![NotificationKind::CodeChange],
                    min_importance: Some(Importance::High),
                    path_prefixes: vec!["src/".into()],
                    branches: vec!["main".into()],
                    ..NotificationFilter::default()
                },
                ..InboxRequest::default()
            },
        )
        .unwrap();
        assert_eq!(target_result.notifications.len(), 1);

        let bystander_result = inbox_with_request(
            &fixture.wiki,
            &InboxRequest {
                session_id: bystander.id,
                ..InboxRequest::default()
            },
        )
        .unwrap();
        assert!(bystander_result.notifications.is_empty());
    }

    #[test]
    fn polling_reads_metadata_without_loading_the_body() {
        let fixture = Fixture::new();
        let sender = start_test_session(&fixture.wiki, "sender", 0);
        let receiver = start_test_session(&fixture.wiki, "receiver", 0);
        let notification = test_notification(&fixture.wiki, &sender, "valid metadata");
        let path = session_dir(&fixture.wiki, &sender.id)
            .unwrap()
            .join(format!("notifications/{}.md", notification.meta.id));
        let mut raw = format!(
            "+++\n{}+++\n\n",
            toml::to_string_pretty(&notification.meta).unwrap()
        )
        .into_bytes();
        raw.extend_from_slice(&[0xff, 0xfe]);
        fs::write(&path, raw).unwrap();

        let inbox = inbox_with_request(
            &fixture.wiki,
            &InboxRequest {
                session_id: receiver.id.clone(),
                ..InboxRequest::default()
            },
        )
        .unwrap();
        assert_eq!(inbox.notifications.len(), 1);
        assert!(
            read_notification(&fixture.wiki, &receiver.id, &notification.meta.id, false).is_err()
        );
    }

    #[test]
    fn session_list_is_bounded_and_has_a_continuation() {
        let fixture = Fixture::new();
        let expected = (0..3)
            .map(|index| start_test_session(&fixture.wiki, &format!("agent-{index}"), 0).id)
            .collect::<BTreeSet<_>>();

        let first = list_with_options(
            &fixture.wiki,
            &SessionListRequest {
                limit: Some(1),
                newest_first: true,
                ..SessionListRequest::default()
            },
        )
        .unwrap();
        assert!(first.scan_complete);
        assert_eq!(first.total_matches, 3);
        assert_eq!(first.returned, 1);
        assert_eq!(first.omitted, 2);
        assert_eq!(first.continuation, Some(1));

        let second = list_with_options(
            &fixture.wiki,
            &SessionListRequest {
                limit: Some(2),
                cursor: first.continuation.unwrap(),
                newest_first: true,
                ..SessionListRequest::default()
            },
        )
        .unwrap();
        assert_eq!(second.returned, 2);
        assert_eq!(second.omitted, 0);
        assert_eq!(second.continuation, None);
        let returned = first
            .sessions
            .into_iter()
            .chain(second.sessions)
            .map(|session| session.id)
            .collect::<BTreeSet<_>>();
        assert_eq!(returned, expected);
    }

    #[test]
    fn session_show_returns_compact_bounded_notification_summaries() {
        let mut fixture = Fixture::new();
        fixture.wiki.sessions.max_summary_bytes = MAX_SESSION_SHOW_SUMMARY_BYTES * 2;
        let sender = start_test_session(&fixture.wiki, "sender", 0);
        for index in 0..3 {
            test_notification(
                &fixture.wiki,
                &sender,
                &format!(
                    "{index}-{}",
                    "x".repeat(MAX_SESSION_SHOW_SUMMARY_BYTES + 64)
                ),
            );
        }

        let first: serde_json::Value = serde_json::from_str(
            &show_with_options(
                &fixture.wiki,
                &sender.id,
                &SessionShowRequest {
                    limit: Some(1),
                    cursor: 0,
                },
                true,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(first["total_notifications_sent"], 3, "{first}");
        assert_eq!(first["notifications_returned"], 1);
        assert_eq!(first["notifications_omitted"], 2);
        assert_eq!(first["continuation"], 1);
        let summary = first["notifications_sent"][0]["summary"].as_str().unwrap();
        assert!(summary.len() <= MAX_SESSION_SHOW_SUMMARY_BYTES);
        assert_eq!(first["notifications_sent"][0]["summary_truncated"], true);
        assert!(first["notifications_sent"][0].get("paths").is_none());
        assert!(first["notifications_sent"][0].get("metadata").is_none());

        let second: serde_json::Value = serde_json::from_str(
            &show_with_options(
                &fixture.wiki,
                &sender.id,
                &SessionShowRequest {
                    limit: Some(2),
                    cursor: 1,
                },
                true,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(second["notifications_returned"], 2);
        assert_eq!(second["notifications_omitted"], 0);
        assert!(second.get("continuation").is_none());
    }

    #[test]
    fn oversized_activity_scan_fails_closed() {
        let fixture = Fixture::new();
        let session = start_test_session(&fixture.wiki, "busy", 0);
        let dir = activity_dir(&fixture.wiki, &session.id).unwrap();
        fs::create_dir(&dir).unwrap();
        let path = dir.join("activity-20000101-000000-deadbeef.toml");
        let file = fs::File::create(path).unwrap();
        file.set_len(MAX_ACTIVITY_BYTES_PER_SESSION + 1).unwrap();

        let error = load_session(&fixture.wiki, &session.id).unwrap_err();
        assert!(error.to_string().contains("hard scan ceiling"));
        let listed = list_with_options(&fixture.wiki, &SessionListRequest::default()).unwrap();
        assert!(!listed.scan_complete);
        assert!(listed.sessions.is_empty());
    }

    #[test]
    fn incomplete_notification_scan_is_visible_and_blocks_resolution_and_polling() {
        let fixture = Fixture::new();
        let sender = start_test_session(&fixture.wiki, "sender", 0);
        let receiver = start_test_session(&fixture.wiki, "receiver", 60);
        let valid = test_notification(&fixture.wiki, &sender, "valid");
        let oversized = session_dir(&fixture.wiki, &sender.id)
            .unwrap()
            .join("notifications/notify-oversized.md");
        let file = fs::File::create(oversized).unwrap();
        file.set_len(MAX_SESSION_SCAN_BYTES + 1).unwrap();

        let shown: serde_json::Value = serde_json::from_str(
            &show_with_options(
                &fixture.wiki,
                &sender.id,
                &SessionShowRequest::default(),
                true,
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(shown["scan_complete"], false);
        assert!(shown["warnings_total"].as_u64().unwrap() >= 1);
        assert!(find_stored_notification(&fixture.wiki, &valid.meta.id)
            .unwrap_err()
            .to_string()
            .contains("scan is incomplete"));
        assert!(inbox_with_request(
            &fixture.wiki,
            &InboxRequest {
                session_id: receiver.id,
                ..InboxRequest::default()
            },
        )
        .unwrap_err()
        .to_string()
        .contains("scan is incomplete"));
    }

    #[test]
    fn recovery_markers_block_every_session_mutation_before_publication() {
        let fixture = Fixture::new();
        let sender = start_test_session(&fixture.wiki, "sender", 0);
        let receiver = start_test_session(&fixture.wiki, "receiver", 60);
        let prunable = start_test_session(&fixture.wiki, "prunable", 0);
        let notification = test_notification(&fixture.wiki, &sender, "existing notification");
        close(&fixture.wiki, &prunable.id, false).unwrap();

        let baseline = storage_snapshot(&sessions_dir(&fixture.wiki));
        for marker in [
            ".publish-journal.json",
            ".ingest-reconciliation-recovery.json",
        ] {
            let marker_path = fixture.wiki.dir.join(marker);
            fs::write(&marker_path, "{}").unwrap();

            assert_recovery_blocked(start_with_options(
                &fixture.wiki,
                StartOptions {
                    agent: Some("blocked-start".into()),
                    ..StartOptions::default()
                },
            ));
            assert_recovery_blocked(heartbeat(&fixture.wiki, &sender.id, true));
            assert_recovery_blocked(close(&fixture.wiki, &sender.id, false));
            assert_recovery_blocked(notify_with_request(
                &fixture.wiki,
                NotifyRequest {
                    source_session: sender.id.clone(),
                    summary: "must not publish".into(),
                    ..NotifyRequest::default()
                },
            ));
            assert_recovery_blocked(inbox_with_request(
                &fixture.wiki,
                &InboxRequest {
                    session_id: receiver.id.clone(),
                    ..InboxRequest::default()
                },
            ));
            assert_recovery_blocked(read_notification(
                &fixture.wiki,
                &receiver.id,
                &notification.meta.id,
                false,
            ));
            assert_recovery_blocked(dismiss_notification(
                &fixture.wiki,
                &receiver.id,
                &notification.meta.id,
                false,
            ));
            assert_recovery_blocked(prune_sessions(
                &fixture.wiki,
                &PruneRequest {
                    closed_only: true,
                    older_than_seconds: Some(0),
                    dry_run: false,
                    ..PruneRequest::default()
                },
            ));

            // Read-only surfaces and a genuine prune preview remain usable
            // while operators inspect an interrupted transaction.
            list_with_options(&fixture.wiki, &SessionListRequest::default()).unwrap();
            show_with_options(
                &fixture.wiki,
                &sender.id,
                &SessionShowRequest::default(),
                false,
            )
            .unwrap();
            inspect_notifications(&fixture.wiki);
            prune_sessions(
                &fixture.wiki,
                &PruneRequest {
                    closed_only: true,
                    older_than_seconds: Some(0),
                    dry_run: true,
                    ..PruneRequest::default()
                },
            )
            .unwrap();

            assert_eq!(
                storage_snapshot(&sessions_dir(&fixture.wiki)),
                baseline,
                "{marker} allowed a session record to escape before recovery"
            );
            fs::remove_file(marker_path).unwrap();
        }

        heartbeat(&fixture.wiki, &sender.id, true)
            .expect("session mutation should resume after recovery state is cleared");
    }

    #[cfg(unix)]
    #[test]
    fn shared_mutation_guard_remains_held_through_session_history_commit() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration as StdDuration, Instant};

        let mut fixture = Fixture::new();
        let sender = start_test_session(&fixture.wiki, "sender", 0);

        // Reopen with automatic session history enabled, then establish a
        // clean baseline before installing a deliberately blocking hook.
        fs::write(
            fixture.wiki.dir.join("wookie.toml"),
            "name = \"test\"\nauto_commit = true\nproject_roots = []\n",
        )
        .unwrap();
        fixture.wiki.init_git();
        let add = Command::new("git")
            .arg("-C")
            .arg(&fixture.wiki.dir)
            .args(["add", "-A"])
            .status()
            .unwrap();
        assert!(add.success());
        let baseline_commit = Command::new("git")
            .arg("-C")
            .arg(&fixture.wiki.dir)
            .args([
                "-c",
                "user.name=wookie",
                "-c",
                "user.email=wookie@localhost",
                "commit",
                "-q",
                "-m",
                "baseline",
            ])
            .status()
            .unwrap();
        assert!(baseline_commit.success());
        fixture.wiki = crate::wiki::open(&fixture.home, "test").unwrap();

        let git_dir = fixture.wiki.dir.join(".git");
        let hook_entered = git_dir.join("wookie-session-hook-entered");
        let hook_release = git_dir.join("wookie-session-hook-release");
        let hook = git_dir.join("hooks/pre-commit");
        fs::write(
            &hook,
            "#!/bin/sh\n: > .git/wookie-session-hook-entered\nwhile [ ! -f .git/wookie-session-hook-release ]; do sleep 0.01; done\n",
        )
        .unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();

        let notify_home = fixture.home.clone();
        let sender_id = sender.id.clone();
        let notify_thread = thread::spawn(move || {
            let wiki = crate::wiki::open(&notify_home, "test").unwrap();
            notify_with_request(
                &wiki,
                NotifyRequest {
                    source_session: sender_id,
                    summary: "commit boundary".into(),
                    ..NotifyRequest::default()
                },
            )
        });

        let deadline = Instant::now() + StdDuration::from_secs(5);
        while !hook_entered.exists() && Instant::now() < deadline {
            thread::sleep(StdDuration::from_millis(10));
        }
        if !hook_entered.exists() {
            fs::write(&hook_release, "").unwrap();
            let result = notify_thread.join().unwrap();
            panic!("session history hook was not reached; notify result: {result:?}");
        }

        let (attempted_tx, attempted_rx) = mpsc::channel();
        let start_home = fixture.home.clone();
        let start_thread = thread::spawn(move || {
            let wiki = crate::wiki::open(&start_home, "test").unwrap();
            attempted_tx.send(()).unwrap();
            start_with_options(
                &wiki,
                StartOptions {
                    agent: Some("waiting-writer".into()),
                    ..StartOptions::default()
                },
            )
        });
        attempted_rx.recv().unwrap();
        thread::sleep(StdDuration::from_millis(100));

        let session_count = fs::read_dir(sessions_dir(&fixture.wiki))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .count();
        let competing_guard_was_blocked = fixture.wiki.try_acquire_mutation_guard().is_none();

        // Always unblock and reap both Git writers before asserting, so a
        // regression fails cleanly instead of leaving hook processes behind.
        fs::write(&hook_release, "").unwrap();
        notify_thread.join().unwrap().unwrap();
        start_thread.join().unwrap().unwrap();

        assert_eq!(
            session_count, 1,
            "a second session record published before the first history commit completed"
        );
        assert!(
            competing_guard_was_blocked,
            "shared writer guard was released while session history was still committing"
        );

        let session_count = fs::read_dir(sessions_dir(&fixture.wiki))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .count();
        assert_eq!(session_count, 2);
        let status = Command::new("git")
            .arg("-C")
            .arg(&fixture.wiki.dir)
            .args(["status", "--porcelain"])
            .output()
            .unwrap();
        assert!(status.status.success());
        assert!(
            status.stdout.is_empty(),
            "session transaction left wiki history dirty: {}",
            String::from_utf8_lossy(&status.stdout)
        );
    }

    #[cfg(unix)]
    #[test]
    fn session_symlinks_cannot_redirect_notifications_or_acknowledgements() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        let sender = start_test_session(&fixture.wiki, "sender", 0);
        let receiver = start_test_session(&fixture.wiki, "receiver", 0);
        let outside_notifications = fixture.home.join("outside-notifications");
        fs::create_dir_all(&outside_notifications).unwrap();
        let notifications = session_dir(&fixture.wiki, &sender.id)
            .unwrap()
            .join("notifications");
        fs::remove_dir(&notifications).unwrap();
        symlink(&outside_notifications, &notifications).unwrap();
        let publish = notify_with_request(
            &fixture.wiki,
            NotifyRequest {
                source_session: sender.id.clone(),
                summary: "must not escape".into(),
                ..NotifyRequest::default()
            },
        );
        assert!(publish.unwrap_err().to_string().contains("symlink"));
        assert_eq!(fs::read_dir(&outside_notifications).unwrap().count(), 0);

        fs::remove_file(&notifications).unwrap();
        fs::create_dir(&notifications).unwrap();
        let notification = test_notification(&fixture.wiki, &sender, "ack containment");
        let outside_inbox = fixture.home.join("outside-inbox");
        fs::create_dir_all(&outside_inbox).unwrap();
        let inbox = session_dir(&fixture.wiki, &receiver.id)
            .unwrap()
            .join("inbox");
        symlink(&outside_inbox, &inbox).unwrap();
        let read = read_notification(&fixture.wiki, &receiver.id, &notification.meta.id, false);
        assert!(read.unwrap_err().to_string().contains("symlink"));
        assert_eq!(fs::read_dir(&outside_inbox).unwrap().count(), 0);
    }
}
