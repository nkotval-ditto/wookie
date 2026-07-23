//! Global config: the `~/.wookie/config.toml` registry mapping project roots
//! to wikis, plus defaults. `WOOKIE_HOME` overrides the home for testing.

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::fs::{self, Metadata, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Persistent configuration is intentionally small. Bounding it before TOML
/// parsing prevents an accidental or hostile file from driving unbounded
/// allocation during every command startup.
const MAX_PERSISTED_CONFIG_BYTES: usize = 1024 * 1024;
const MAX_REGISTERED_WIKIS: usize = 1024;
const MAX_PROJECT_ROOTS_PER_WIKI: usize = 128;
const MAX_PROJECT_ROOT_BYTES: usize = 4 * 1024;
const MAX_SESSION_ENUM_BYTES: usize = 32;

// Session limits remain configurable inside immutable resource ceilings. The
// ceilings are deliberately generous for large local wikis, but prevent a
// typo such as `usize::MAX` from disabling bounded reads, output windows, or
// Git-context capture through saturating arithmetic.
pub const MAX_SESSION_POLL_LIMIT: usize = 10_000;
pub const MAX_SESSION_SUMMARY_BYTES: usize = 64 * 1024;
pub const MAX_SESSION_AGENT_BYTES: usize = 4 * 1024;
pub const MAX_SESSION_LABEL_BYTES: usize = 64 * 1024;
pub const MAX_SESSION_BODY_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_SESSION_PATHS: usize = 10_000;
pub const MAX_SESSION_PATH_BYTES: usize = 32 * 1024;
pub const MAX_SESSION_TARGETS: usize = 10_000;
pub const MAX_SESSION_IDEMPOTENCY_KEY_BYTES: usize = 4 * 1024;
pub const MAX_SESSION_METADATA_ENTRIES: usize = 1_000;
pub const MAX_SESSION_METADATA_KEY_BYTES: usize = 4 * 1024;
pub const MAX_SESSION_METADATA_VALUE_BYTES: usize = 64 * 1024;
pub const MAX_SESSION_GIT_DIRTY_PATHS: usize = 100_000;
pub const MAX_SESSION_GIT_BRANCH_BYTES: usize = 4 * 1024;
pub const MAX_SESSION_GIT_COMMIT_BYTES: usize = 4 * 1024;
pub const MAX_SESSION_GIT_WORKTREE_BYTES: usize = 32 * 1024;
pub const MAX_SESSION_LOOKBACK_HOURS: u64 = 24 * 366 * 100;
pub const MAX_SESSION_STALE_AFTER_MINUTES: u64 = 60 * 24 * 366 * 10;
pub const MAX_SESSION_ACTIVITY_DEBOUNCE_SECONDS: u64 = 7 * 24 * 60 * 60;
pub const MAX_SESSION_RETENTION_DAYS: u64 = 366 * 100;

fn contains_terminal_control(value: &str) -> bool {
    value.chars().any(|character| {
        character.is_control()
            || matches!(
                character,
                '\u{061c}'
                    | '\u{200e}'
                    | '\u{200f}'
                    | '\u{202a}'..='\u{202e}'
                    | '\u{2066}'..='\u{2069}'
            )
    })
}

fn validate_persistent_string(
    label: &str,
    value: &str,
    max_bytes: usize,
    allow_empty: bool,
) -> Result<()> {
    if (!allow_empty && value.is_empty()) || value.len() > max_bytes {
        bail!(
            "{label} must be {} and at most {max_bytes} bytes",
            if allow_empty {
                "valid UTF-8"
            } else {
                "non-empty"
            }
        );
    }
    if contains_terminal_control(value) {
        bail!("{label} must not contain control or terminal-direction characters");
    }
    Ok(())
}

fn configure_no_follow(options: &mut OpenOptions) {
    // `std` does not expose these flags by name. Keep the platform constants
    // local so configuration reads do not need a new native dependency.
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
fn same_file(left: &Metadata, right: &Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file(left: &Metadata, right: &Metadata) -> bool {
    // Windows opens reparse points without following them above. These fields
    // add a best-effort replacement check on platforms without Unix inode IDs.
    left.len() == right.len()
        && left.modified().ok() == right.modified().ok()
        && left.created().ok() == right.created().ok()
}

/// Read an optional configuration snapshot through exactly one file handle.
///
/// The pre-open metadata check avoids blocking on devices/FIFOs, no-follow
/// flags reject link substitution at open time on supported platforms, and
/// the identity comparison closes the lstat/open replacement window. Reading
/// `limit + 1` bytes also catches a regular file that grows after metadata was
/// inspected.
pub(crate) fn read_optional_bounded_regular_utf8(
    path: &Path,
    max_bytes: usize,
    label: &str,
) -> Result<Option<String>> {
    let max_bytes_u64 = u64::try_from(max_bytes)
        .ok()
        .and_then(|limit| limit.checked_add(1).map(|_| limit))
        .context("persistent configuration byte limit is too large")?;
    let before = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("inspecting {label} {}", path.display()));
        }
    };
    if before.file_type().is_symlink() {
        bail!("{label} must not be a symlink: {}", path.display());
    }
    if !before.is_file() {
        bail!("{label} must be a regular file: {}", path.display());
    }
    if before.len() > max_bytes_u64 {
        bail!(
            "{label} exceeds the {max_bytes} byte limit: {}",
            path.display()
        );
    }

    let mut options = OpenOptions::new();
    options.read(true);
    configure_no_follow(&mut options);
    let file = options
        .open(path)
        .with_context(|| format!("opening {label} {}", path.display()))?;
    let opened = file
        .metadata()
        .with_context(|| format!("inspecting opened {label} {}", path.display()))?;
    if !opened.is_file() {
        bail!("{label} must be a regular file: {}", path.display());
    }
    let after = fs::symlink_metadata(path)
        .with_context(|| format!("rechecking {label} {}", path.display()))?;
    if after.file_type().is_symlink() || !after.is_file() {
        bail!(
            "{label} must remain a regular non-symlink file: {}",
            path.display()
        );
    }
    if !same_file(&before, &opened) || !same_file(&after, &opened) {
        bail!(
            "{label} changed while it was being opened: {}",
            path.display()
        );
    }
    if opened.len() > max_bytes_u64 {
        bail!(
            "{label} exceeds the {max_bytes} byte limit: {}",
            path.display()
        );
    }

    let mut bytes = Vec::with_capacity(opened.len() as usize);
    file.take(max_bytes_u64 + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("reading {label} {}", path.display()))?;
    if bytes.len() > max_bytes {
        bail!(
            "{label} exceeds the {max_bytes} byte limit: {}",
            path.display()
        );
    }
    let text = String::from_utf8(bytes)
        .with_context(|| format!("{label} is not valid UTF-8: {}", path.display()))?;
    Ok(Some(text))
}

pub fn wookie_home() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("WOOKIE_HOME") {
        return Ok(PathBuf::from(home));
    }
    Ok(user_home()?.join(".wookie"))
}

pub fn user_home() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        return Ok(PathBuf::from(home));
    }
    if let (Some(drive), Some(path)) = (std::env::var_os("HOMEDRIVE"), std::env::var_os("HOMEPATH"))
    {
        let mut home = PathBuf::from(drive);
        home.push(path);
        return Ok(home);
    }
    bail!("cannot locate the user home directory; set HOME, USERPROFILE, or WOOKIE_HOME")
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GlobalConfig {
    #[serde(default)]
    pub wikis: BTreeMap<String, WikiEntry>,
    #[serde(default)]
    pub defaults: Defaults,
}

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WikiEntry {
    #[serde(default)]
    pub project_roots: Vec<String>,
}

impl WikiEntry {
    fn validate(&self, wiki: &str) -> Result<()> {
        if self.project_roots.len() > MAX_PROJECT_ROOTS_PER_WIKI {
            bail!(
                "wiki '{wiki}' has too many project roots (maximum {MAX_PROJECT_ROOTS_PER_WIKI})"
            );
        }
        for (index, root) in self.project_roots.iter().enumerate() {
            validate_persistent_string(
                &format!("project root {index} for wiki '{wiki}'"),
                root,
                MAX_PROJECT_ROOT_BYTES,
                false,
            )?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    #[serde(default = "default_true")]
    pub auto_commit: bool,
    #[serde(default)]
    pub sessions: SessionSettings,
    #[serde(default)]
    pub history: HistorySettings,
    #[serde(default)]
    pub retrieval: RetrievalSettings,
    #[serde(default)]
    pub audit: AuditSettings,
    #[serde(default)]
    pub publish: PublishSettings,
}

fn default_true() -> bool {
    true
}

impl Default for Defaults {
    fn default() -> Self {
        Defaults {
            auto_commit: true,
            sessions: SessionSettings::default(),
            history: HistorySettings::default(),
            retrieval: RetrievalSettings::default(),
            audit: AuditSettings::default(),
            publish: PublishSettings::default(),
        }
    }
}

/// Hard ceiling on one bounded retrieval result window.
pub const MAX_SEARCH_LIMIT: usize = 1_000;
/// Hard ceiling on matching body lines materialized per search result.
pub const MAX_EXCERPT_LINES: usize = 20;
/// Immutable ceiling for any bounded retrieval response budget. This is high
/// enough for explicit large audits while preventing `usize::MAX`-style
/// configuration or CLI inputs from defeating bounded materialization.
pub const MAX_RETRIEVAL_TOKENS: usize = 1_000_000;

/// Bounded retrieval defaults shared by `prime` and `search`.
///
/// Defaults remain intentionally small while the hard ceilings prevent a
/// one-off override from causing disproportionate allocation or output work.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetrievalSettings {
    #[serde(default = "default_prime_tokens")]
    pub prime_tokens: usize,
    #[serde(default = "default_instruction_tokens")]
    pub instruction_tokens: usize,
    #[serde(default = "default_search_limit")]
    pub search_limit: usize,
    #[serde(default = "default_search_tokens")]
    pub search_tokens: usize,
    #[serde(default = "default_excerpt_lines")]
    pub excerpt_lines: usize,
    #[serde(default = "default_max_per_section")]
    pub max_per_section: usize,
}

impl Default for RetrievalSettings {
    fn default() -> Self {
        Self {
            prime_tokens: default_prime_tokens(),
            instruction_tokens: default_instruction_tokens(),
            search_limit: default_search_limit(),
            search_tokens: default_search_tokens(),
            excerpt_lines: default_excerpt_lines(),
            max_per_section: default_max_per_section(),
        }
    }
}

impl RetrievalSettings {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.prime_tokens > 0,
            "retrieval.prime_tokens must be greater than zero"
        );
        anyhow::ensure!(
            self.prime_tokens <= MAX_RETRIEVAL_TOKENS,
            "retrieval.prime_tokens must not exceed {MAX_RETRIEVAL_TOKENS}"
        );
        anyhow::ensure!(
            self.instruction_tokens > 0,
            "retrieval.instruction_tokens must be greater than zero"
        );
        anyhow::ensure!(
            self.instruction_tokens <= MAX_RETRIEVAL_TOKENS,
            "retrieval.instruction_tokens must not exceed {MAX_RETRIEVAL_TOKENS}"
        );
        anyhow::ensure!(
            self.instruction_tokens <= self.prime_tokens,
            "retrieval.instruction_tokens must not exceed retrieval.prime_tokens"
        );
        anyhow::ensure!(
            self.search_limit > 0,
            "retrieval.search_limit must be greater than zero"
        );
        anyhow::ensure!(
            self.search_limit <= MAX_SEARCH_LIMIT,
            "retrieval.search_limit must not exceed {MAX_SEARCH_LIMIT}"
        );
        anyhow::ensure!(
            self.search_tokens > 0,
            "retrieval.search_tokens must be greater than zero"
        );
        anyhow::ensure!(
            self.search_tokens <= MAX_RETRIEVAL_TOKENS,
            "retrieval.search_tokens must not exceed {MAX_RETRIEVAL_TOKENS}"
        );
        anyhow::ensure!(
            self.excerpt_lines > 0,
            "retrieval.excerpt_lines must be greater than zero"
        );
        anyhow::ensure!(
            self.excerpt_lines <= MAX_EXCERPT_LINES,
            "retrieval.excerpt_lines must not exceed {MAX_EXCERPT_LINES}"
        );
        anyhow::ensure!(
            self.max_per_section > 0,
            "retrieval.max_per_section must be greater than zero"
        );
        anyhow::ensure!(
            self.max_per_section <= MAX_SEARCH_LIMIT,
            "retrieval.max_per_section must not exceed {MAX_SEARCH_LIMIT}"
        );
        Ok(())
    }
}

/// Sparse per-wiki retrieval overrides.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetrievalOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prime_tokens: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction_tokens: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_limit: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_tokens: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excerpt_lines: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_per_section: Option<usize>,
}

impl RetrievalOverrides {
    pub fn is_empty(&self) -> bool {
        self.prime_tokens.is_none()
            && self.instruction_tokens.is_none()
            && self.search_limit.is_none()
            && self.search_tokens.is_none()
            && self.excerpt_lines.is_none()
            && self.max_per_section.is_none()
    }

    pub fn apply(&self, base: &RetrievalSettings) -> Result<RetrievalSettings> {
        let mut effective = base.clone();
        macro_rules! inherit {
            ($($field:ident),+ $(,)?) => {
                $(if let Some(value) = self.$field {
                    effective.$field = value;
                })+
            };
        }
        inherit!(
            prime_tokens,
            instruction_tokens,
            search_limit,
            search_tokens,
            excerpt_lines,
            max_per_section,
        );
        effective.validate()?;
        Ok(effective)
    }
}

/// Health-check and provenance defaults.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditSettings {
    #[serde(default = "default_true")]
    pub source_provenance: bool,
    /// Maximum estimated tokens returned by the compact critique briefing.
    #[serde(default = "default_critique_tokens")]
    pub critique_tokens: usize,
}

pub const MIN_CRITIQUE_TOKENS: usize = 256;
pub const MAX_CRITIQUE_TOKENS: usize = 1_000_000;

const fn default_critique_tokens() -> usize {
    4_000
}

impl Default for AuditSettings {
    fn default() -> Self {
        Self {
            source_provenance: true,
            critique_tokens: default_critique_tokens(),
        }
    }
}

impl AuditSettings {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            (MIN_CRITIQUE_TOKENS..=MAX_CRITIQUE_TOKENS).contains(&self.critique_tokens),
            "audit.critique_tokens must be between {MIN_CRITIQUE_TOKENS} and {MAX_CRITIQUE_TOKENS}"
        );
        Ok(())
    }
}

/// Sparse per-wiki audit overrides.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_provenance: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub critique_tokens: Option<usize>,
}

impl AuditOverrides {
    pub fn is_empty(&self) -> bool {
        self.source_provenance.is_none() && self.critique_tokens.is_none()
    }

    pub fn apply(&self, base: &AuditSettings) -> Result<AuditSettings> {
        let mut effective = base.clone();
        if let Some(value) = self.source_provenance {
            effective.source_provenance = value;
        }
        if let Some(value) = self.critique_tokens {
            effective.critique_tokens = value;
        }
        effective.validate()?;
        Ok(effective)
    }
}

/// How transactional publish treats pages left without inbound links.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OrphanPolicy {
    #[default]
    Warn,
    Error,
}

/// Change-control defaults for transactional publishing.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishSettings {
    #[serde(default = "default_true")]
    pub require_base_revision: bool,
    #[serde(default)]
    pub orphan_policy: OrphanPolicy,
    /// Maximum estimated tokens returned by publish checks and rule reviews.
    #[serde(default = "default_publish_output_tokens")]
    pub output_tokens: usize,
}

const fn default_publish_output_tokens() -> usize {
    4_000
}

impl Default for PublishSettings {
    fn default() -> Self {
        Self {
            require_base_revision: true,
            orphan_policy: OrphanPolicy::Warn,
            output_tokens: default_publish_output_tokens(),
        }
    }
}

impl PublishSettings {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.output_tokens >= 256,
            "publish.output_tokens must be at least 256"
        );
        Ok(())
    }
}

/// Sparse per-wiki publish overrides.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_base_revision: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub orphan_policy: Option<OrphanPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens: Option<usize>,
}

impl PublishOverrides {
    pub fn is_empty(&self) -> bool {
        self.require_base_revision.is_none()
            && self.orphan_policy.is_none()
            && self.output_tokens.is_none()
    }

    pub fn apply(&self, base: &PublishSettings) -> Result<PublishSettings> {
        let mut effective = base.clone();
        if let Some(value) = self.require_base_revision {
            effective.require_base_revision = value;
        }
        if let Some(value) = self.orphan_policy {
            effective.orphan_policy = value;
        }
        if let Some(value) = self.output_tokens {
            effective.output_tokens = value;
        }
        effective.validate()?;
        Ok(effective)
    }
}

/// Effective defaults for cross-session coordination. A wiki may override
/// individual fields in `wookie.toml`; every omitted field inherits globally.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSettings {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// How far before session creation unread polling should look. Zero keeps
    /// the original "start caught up" behavior.
    #[serde(default)]
    pub initial_lookback_hours: u64,
    /// Sessions without activity for this long are considered stale in lists.
    #[serde(default = "default_stale_after_minutes")]
    pub stale_after_minutes: u64,
    /// Minimum interval between append-only activity events generated by
    /// ordinary session commands.
    #[serde(default = "default_activity_debounce_seconds")]
    pub activity_debounce_seconds: u64,
    /// Optional age threshold used by `session prune` and auto-pruning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_days: Option<u64>,
    #[serde(default)]
    pub auto_prune_on_start: bool,
    #[serde(default = "default_poll_limit")]
    pub poll_limit: usize,
    #[serde(default = "default_summary_bytes")]
    pub max_summary_bytes: usize,
    #[serde(default = "default_agent_bytes")]
    pub max_agent_bytes: usize,
    #[serde(default = "default_label_bytes")]
    pub max_label_bytes: usize,
    #[serde(default = "default_body_bytes")]
    pub max_body_bytes: usize,
    #[serde(default = "default_paths")]
    pub max_paths: usize,
    #[serde(default = "default_path_bytes")]
    pub max_path_bytes: usize,
    #[serde(default = "default_targets")]
    pub max_targets: usize,
    #[serde(default = "default_idempotency_key_bytes")]
    pub max_idempotency_key_bytes: usize,
    #[serde(default = "default_metadata_entries")]
    pub max_metadata_entries: usize,
    #[serde(default = "default_metadata_key_bytes")]
    pub max_metadata_key_bytes: usize,
    #[serde(default = "default_metadata_value_bytes")]
    pub max_metadata_value_bytes: usize,
    #[serde(default = "default_git_dirty_paths")]
    pub max_git_dirty_paths: usize,
    #[serde(default = "default_git_branch_bytes")]
    pub max_git_branch_bytes: usize,
    #[serde(default = "default_git_commit_bytes")]
    pub max_git_commit_bytes: usize,
    #[serde(default = "default_git_worktree_bytes")]
    pub max_git_worktree_bytes: usize,
    #[serde(default = "default_true")]
    pub include_git_context: bool,
    #[serde(default = "default_true")]
    pub heartbeat_on_activity: bool,
    #[serde(default = "default_kind")]
    pub default_kind: String,
    #[serde(default = "default_importance")]
    pub default_importance: String,
}

impl Default for SessionSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            initial_lookback_hours: 0,
            stale_after_minutes: default_stale_after_minutes(),
            activity_debounce_seconds: default_activity_debounce_seconds(),
            retention_days: None,
            auto_prune_on_start: false,
            poll_limit: default_poll_limit(),
            max_summary_bytes: default_summary_bytes(),
            max_agent_bytes: default_agent_bytes(),
            max_label_bytes: default_label_bytes(),
            max_body_bytes: default_body_bytes(),
            max_paths: default_paths(),
            max_path_bytes: default_path_bytes(),
            max_targets: default_targets(),
            max_idempotency_key_bytes: default_idempotency_key_bytes(),
            max_metadata_entries: default_metadata_entries(),
            max_metadata_key_bytes: default_metadata_key_bytes(),
            max_metadata_value_bytes: default_metadata_value_bytes(),
            max_git_dirty_paths: default_git_dirty_paths(),
            max_git_branch_bytes: default_git_branch_bytes(),
            max_git_commit_bytes: default_git_commit_bytes(),
            max_git_worktree_bytes: default_git_worktree_bytes(),
            include_git_context: true,
            heartbeat_on_activity: true,
            default_kind: default_kind(),
            default_importance: default_importance(),
        }
    }
}

impl SessionSettings {
    pub fn validate(&self) -> Result<()> {
        let bounded = |key: &str, value: usize, maximum: usize| -> Result<()> {
            anyhow::ensure!(
                (1..=maximum).contains(&value),
                "{key} must be greater than zero and no greater than {maximum}"
            );
            Ok(())
        };
        anyhow::ensure!(
            self.initial_lookback_hours <= MAX_SESSION_LOOKBACK_HOURS,
            "sessions.initial_lookback_hours is too large (maximum {MAX_SESSION_LOOKBACK_HOURS})"
        );
        anyhow::ensure!(
            (1..=MAX_SESSION_STALE_AFTER_MINUTES).contains(&self.stale_after_minutes),
            "sessions.stale_after_minutes must be positive and no greater than {MAX_SESSION_STALE_AFTER_MINUTES}"
        );
        anyhow::ensure!(
            (1..=MAX_SESSION_ACTIVITY_DEBOUNCE_SECONDS)
                .contains(&self.activity_debounce_seconds),
            "sessions.activity_debounce_seconds must be positive and no greater than {MAX_SESSION_ACTIVITY_DEBOUNCE_SECONDS}"
        );
        anyhow::ensure!(
            self.retention_days
                .is_none_or(|days| (1..=MAX_SESSION_RETENTION_DAYS).contains(&days)),
            "sessions.retention_days must be positive and no greater than {MAX_SESSION_RETENTION_DAYS}, or omitted"
        );
        bounded(
            "sessions.poll_limit",
            self.poll_limit,
            MAX_SESSION_POLL_LIMIT,
        )?;
        bounded(
            "sessions.max_summary_bytes",
            self.max_summary_bytes,
            MAX_SESSION_SUMMARY_BYTES,
        )?;
        bounded(
            "sessions.max_agent_bytes",
            self.max_agent_bytes,
            MAX_SESSION_AGENT_BYTES,
        )?;
        bounded(
            "sessions.max_label_bytes",
            self.max_label_bytes,
            MAX_SESSION_LABEL_BYTES,
        )?;
        bounded(
            "sessions.max_body_bytes",
            self.max_body_bytes,
            MAX_SESSION_BODY_BYTES,
        )?;
        anyhow::ensure!(
            self.max_body_bytes >= self.max_summary_bytes,
            "sessions.max_body_bytes must be at least sessions.max_summary_bytes"
        );
        bounded("sessions.max_paths", self.max_paths, MAX_SESSION_PATHS)?;
        bounded(
            "sessions.max_path_bytes",
            self.max_path_bytes,
            MAX_SESSION_PATH_BYTES,
        )?;
        bounded(
            "sessions.max_targets",
            self.max_targets,
            MAX_SESSION_TARGETS,
        )?;
        bounded(
            "sessions.max_idempotency_key_bytes",
            self.max_idempotency_key_bytes,
            MAX_SESSION_IDEMPOTENCY_KEY_BYTES,
        )?;
        bounded(
            "sessions.max_metadata_entries",
            self.max_metadata_entries,
            MAX_SESSION_METADATA_ENTRIES,
        )?;
        bounded(
            "sessions.max_metadata_key_bytes",
            self.max_metadata_key_bytes,
            MAX_SESSION_METADATA_KEY_BYTES,
        )?;
        bounded(
            "sessions.max_metadata_value_bytes",
            self.max_metadata_value_bytes,
            MAX_SESSION_METADATA_VALUE_BYTES,
        )?;
        bounded(
            "sessions.max_git_dirty_paths",
            self.max_git_dirty_paths,
            MAX_SESSION_GIT_DIRTY_PATHS,
        )?;
        bounded(
            "sessions.max_git_branch_bytes",
            self.max_git_branch_bytes,
            MAX_SESSION_GIT_BRANCH_BYTES,
        )?;
        bounded(
            "sessions.max_git_commit_bytes",
            self.max_git_commit_bytes,
            MAX_SESSION_GIT_COMMIT_BYTES,
        )?;
        bounded(
            "sessions.max_git_worktree_bytes",
            self.max_git_worktree_bytes,
            MAX_SESSION_GIT_WORKTREE_BYTES,
        )?;
        validate_persistent_string(
            "sessions.default_kind",
            &self.default_kind,
            MAX_SESSION_ENUM_BYTES,
            false,
        )?;
        anyhow::ensure!(
            matches!(
                self.default_kind.as_str(),
                "code-change" | "decision" | "blocker" | "handoff" | "warning" | "note"
            ),
            "sessions.default_kind is invalid"
        );
        validate_persistent_string(
            "sessions.default_importance",
            &self.default_importance,
            MAX_SESSION_ENUM_BYTES,
            false,
        )?;
        anyhow::ensure!(
            matches!(self.default_importance.as_str(), "low" | "normal" | "high"),
            "sessions.default_importance is invalid"
        );
        Ok(())
    }
}

/// Sparse per-wiki overrides. Keeping every field optional means changing one
/// wiki setting continues to inherit all other values from global defaults.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_lookback_hours: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_after_minutes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity_debounce_seconds: Option<u64>,
    /// Zero explicitly disables a globally configured retention period.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retention_days: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_prune_on_start: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_limit: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_summary_bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_agent_bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_label_bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_body_bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_paths: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_path_bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_targets: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_idempotency_key_bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_metadata_entries: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_metadata_key_bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_metadata_value_bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_git_dirty_paths: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_git_branch_bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_git_commit_bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_git_worktree_bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_git_context: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_on_activity: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_importance: Option<String>,
}

impl SessionOverrides {
    pub fn is_empty(&self) -> bool {
        toml::Value::try_from(self)
            .ok()
            .and_then(|value| value.as_table().map(toml::map::Map::is_empty))
            .unwrap_or(true)
    }

    pub fn apply(&self, base: &SessionSettings) -> Result<SessionSettings> {
        let mut effective = base.clone();
        macro_rules! inherit {
            ($($field:ident),+ $(,)?) => {
                $(if let Some(value) = &self.$field {
                    effective.$field = value.clone();
                })+
            };
        }
        inherit!(
            enabled,
            initial_lookback_hours,
            stale_after_minutes,
            activity_debounce_seconds,
            auto_prune_on_start,
            poll_limit,
            max_summary_bytes,
            max_agent_bytes,
            max_label_bytes,
            max_body_bytes,
            max_paths,
            max_path_bytes,
            max_targets,
            max_idempotency_key_bytes,
            max_metadata_entries,
            max_metadata_key_bytes,
            max_metadata_value_bytes,
            max_git_dirty_paths,
            max_git_branch_bytes,
            max_git_commit_bytes,
            max_git_worktree_bytes,
            include_git_context,
            heartbeat_on_activity,
            default_kind,
            default_importance,
        );
        if let Some(days) = self.retention_days {
            effective.retention_days = (days > 0).then_some(days);
        }
        effective.validate()?;
        Ok(effective)
    }
}

/// Git-history behavior for a wiki. The lock settings serialize the `git add`
/// + `git commit` pair across concurrent agent processes.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistorySettings {
    #[serde(default = "default_git_lock_timeout_ms")]
    pub lock_timeout_ms: u64,
    #[serde(default = "default_git_lock_stale_seconds")]
    pub lock_stale_seconds: u64,
    #[serde(default = "default_true")]
    pub commit_sessions: bool,
    #[serde(default)]
    pub fail_on_commit_error: bool,
}

impl Default for HistorySettings {
    fn default() -> Self {
        Self {
            lock_timeout_ms: default_git_lock_timeout_ms(),
            lock_stale_seconds: default_git_lock_stale_seconds(),
            commit_sessions: true,
            fail_on_commit_error: false,
        }
    }
}

impl HistorySettings {
    pub fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.lock_timeout_ms > 0,
            "history.lock_timeout_ms must be positive"
        );
        anyhow::ensure!(
            self.lock_stale_seconds > 0,
            "history.lock_stale_seconds must be positive"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock_stale_seconds: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit_sessions: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fail_on_commit_error: Option<bool>,
}

impl HistoryOverrides {
    pub fn is_empty(&self) -> bool {
        self.lock_timeout_ms.is_none()
            && self.lock_stale_seconds.is_none()
            && self.commit_sessions.is_none()
            && self.fail_on_commit_error.is_none()
    }

    pub fn apply(&self, base: &HistorySettings) -> Result<HistorySettings> {
        let mut effective = base.clone();
        if let Some(value) = self.lock_timeout_ms {
            effective.lock_timeout_ms = value;
        }
        if let Some(value) = self.lock_stale_seconds {
            effective.lock_stale_seconds = value;
        }
        if let Some(value) = self.commit_sessions {
            effective.commit_sessions = value;
        }
        if let Some(value) = self.fail_on_commit_error {
            effective.fail_on_commit_error = value;
        }
        effective.validate()?;
        Ok(effective)
    }
}

fn default_stale_after_minutes() -> u64 {
    120
}

fn default_activity_debounce_seconds() -> u64 {
    30
}

fn default_poll_limit() -> usize {
    100
}

fn default_summary_bytes() -> usize {
    512
}

fn default_agent_bytes() -> usize {
    128
}

fn default_label_bytes() -> usize {
    1024
}

fn default_body_bytes() -> usize {
    64 * 1024
}

fn default_paths() -> usize {
    64
}

fn default_path_bytes() -> usize {
    4096
}

fn default_targets() -> usize {
    32
}

fn default_idempotency_key_bytes() -> usize {
    256
}

fn default_metadata_entries() -> usize {
    32
}

fn default_metadata_key_bytes() -> usize {
    64
}

fn default_metadata_value_bytes() -> usize {
    1024
}

fn default_git_dirty_paths() -> usize {
    256
}

fn default_git_branch_bytes() -> usize {
    512
}

fn default_git_commit_bytes() -> usize {
    128
}

fn default_git_worktree_bytes() -> usize {
    4096
}

fn default_kind() -> String {
    "note".into()
}

fn default_importance() -> String {
    "normal".into()
}

fn default_git_lock_timeout_ms() -> u64 {
    30_000
}

fn default_git_lock_stale_seconds() -> u64 {
    60
}

fn default_prime_tokens() -> usize {
    1_500
}

fn default_instruction_tokens() -> usize {
    700
}

fn default_search_limit() -> usize {
    10
}

fn default_search_tokens() -> usize {
    2_000
}

fn default_excerpt_lines() -> usize {
    2
}

fn default_max_per_section() -> usize {
    5
}

impl GlobalConfig {
    const UPDATE_LOCK: &'static str = ".config.lock";

    fn validate(&self) -> Result<()> {
        if self.wikis.len() > MAX_REGISTERED_WIKIS {
            bail!("global config has too many wikis (maximum {MAX_REGISTERED_WIKIS})");
        }
        for (name, entry) in &self.wikis {
            crate::wiki::validate_slug(name)
                .with_context(|| format!("invalid registered wiki name '{name}'"))?;
            entry.validate(name)?;
        }
        self.defaults.sessions.validate()?;
        self.defaults.history.validate()?;
        self.defaults.retrieval.validate()?;
        self.defaults.audit.validate()?;
        self.defaults.publish.validate()
    }

    pub fn load(home: &Path) -> Result<GlobalConfig> {
        let path = home.join("config.toml");
        let Some(raw) =
            read_optional_bounded_regular_utf8(&path, MAX_PERSISTED_CONFIG_BYTES, "global config")?
        else {
            return Ok(GlobalConfig::default());
        };
        let config: GlobalConfig =
            toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    fn save_guarded(&self, home: &Path, guard: &crate::history::ExclusiveLock) -> Result<()> {
        if !guard.is_for(home, Self::UPDATE_LOCK) {
            bail!("global configuration lock belongs to a different Wookie home");
        }
        self.validate()?;
        fs::create_dir_all(home)?;
        let path = home.join("config.toml");
        let raw = toml::to_string_pretty(self)?;
        crate::wiki::atomic_write(&path, raw).with_context(|| format!("writing {}", path.display()))
    }

    pub(crate) fn with_home_lock<T>(
        home: &Path,
        operation: impl FnOnce(&crate::history::ExclusiveLock) -> Result<T>,
    ) -> Result<T> {
        fs::create_dir_all(home)
            .with_context(|| format!("creating wookie home {}", home.display()))?;
        // Lock timing is advisory only. Protected state is reloaded after
        // acquisition, so using the prior timing values cannot lose data.
        let lock_settings = Self::load(home)?.defaults.history;
        let guard = crate::history::acquire_named_lock(home, Self::UPDATE_LOCK, &lock_settings)?;
        operation(&guard)
    }

    /// Serialize a read-modify-write of global defaults. The latest file is
    /// loaded only after the lock is held, so two agents updating independent
    /// keys cannot overwrite one another with stale snapshots.
    pub(crate) fn update<T>(
        home: &Path,
        edit: impl FnOnce(&mut GlobalConfig) -> Result<T>,
    ) -> Result<T> {
        Self::with_home_lock(home, |guard| {
            let mut config = Self::load(home)?;
            let result = edit(&mut config)?;
            config.save_guarded(home, guard)?;
            Ok(result)
        })
    }

    /// Create the global config exactly once without rewriting an existing
    /// file from a snapshot loaded before another process updated it.
    pub(crate) fn ensure_exists_guarded(
        home: &Path,
        guard: &crate::history::ExclusiveLock,
    ) -> Result<()> {
        if !guard.is_for(home, Self::UPDATE_LOCK) {
            bail!("global configuration lock belongs to a different Wookie home");
        }
        let path = home.join("config.toml");
        match fs::symlink_metadata(&path) {
            Ok(_) => {
                Self::load(home)?;
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Self::default().save_guarded(home, guard)
            }
            Err(error) => Err(error).with_context(|| format!("inspecting {}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_test_home(label: &str) -> PathBuf {
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let home = std::env::temp_dir().join(format!(
            "wookie-config-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&home).unwrap();
        home
    }

    #[test]
    fn session_resource_settings_have_generous_hard_ceilings() {
        macro_rules! accepts_and_rejects {
            ($field:ident, $maximum:expr) => {{
                let mut settings = SessionSettings::default();
                settings.$field = $maximum;
                settings.validate().unwrap();
                settings.$field = $maximum + 1;
                let error = settings.validate().unwrap_err().to_string();
                assert!(
                    error.contains(stringify!($field)),
                    "unexpected {} error: {error}",
                    stringify!($field)
                );
            }};
        }

        accepts_and_rejects!(poll_limit, MAX_SESSION_POLL_LIMIT);
        accepts_and_rejects!(max_summary_bytes, MAX_SESSION_SUMMARY_BYTES);
        accepts_and_rejects!(max_agent_bytes, MAX_SESSION_AGENT_BYTES);
        accepts_and_rejects!(max_label_bytes, MAX_SESSION_LABEL_BYTES);
        accepts_and_rejects!(max_body_bytes, MAX_SESSION_BODY_BYTES);
        accepts_and_rejects!(max_paths, MAX_SESSION_PATHS);
        accepts_and_rejects!(max_path_bytes, MAX_SESSION_PATH_BYTES);
        accepts_and_rejects!(max_targets, MAX_SESSION_TARGETS);
        accepts_and_rejects!(max_idempotency_key_bytes, MAX_SESSION_IDEMPOTENCY_KEY_BYTES);
        accepts_and_rejects!(max_metadata_entries, MAX_SESSION_METADATA_ENTRIES);
        accepts_and_rejects!(max_metadata_key_bytes, MAX_SESSION_METADATA_KEY_BYTES);
        accepts_and_rejects!(max_metadata_value_bytes, MAX_SESSION_METADATA_VALUE_BYTES);
        accepts_and_rejects!(max_git_dirty_paths, MAX_SESSION_GIT_DIRTY_PATHS);
        accepts_and_rejects!(max_git_branch_bytes, MAX_SESSION_GIT_BRANCH_BYTES);
        accepts_and_rejects!(max_git_commit_bytes, MAX_SESSION_GIT_COMMIT_BYTES);
        accepts_and_rejects!(max_git_worktree_bytes, MAX_SESSION_GIT_WORKTREE_BYTES);

        let mut settings = SessionSettings {
            initial_lookback_hours: MAX_SESSION_LOOKBACK_HOURS,
            stale_after_minutes: MAX_SESSION_STALE_AFTER_MINUTES,
            activity_debounce_seconds: MAX_SESSION_ACTIVITY_DEBOUNCE_SECONDS,
            retention_days: Some(MAX_SESSION_RETENTION_DAYS),
            ..SessionSettings::default()
        };
        settings.validate().unwrap();
        settings.retention_days = Some(MAX_SESSION_RETENTION_DAYS + 1);
        assert!(settings.validate().is_err());
    }

    #[test]
    fn global_config_rejects_oversized_files() {
        let home = config_test_home("oversized");
        fs::write(
            home.join("config.toml"),
            vec![b' '; MAX_PERSISTED_CONFIG_BYTES + 1],
        )
        .unwrap();

        let error = GlobalConfig::load(&home).unwrap_err().to_string();
        assert!(error.contains("exceeds the"), "unexpected error: {error}");
        assert!(error.contains("byte limit"), "unexpected error: {error}");
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn global_config_rejects_invalid_utf8() {
        let home = config_test_home("invalid-utf8");
        fs::write(home.join("config.toml"), [0xff, 0xfe]).unwrap();

        let error = GlobalConfig::load(&home).unwrap_err().to_string();
        assert!(
            error.contains("not valid UTF-8"),
            "unexpected error: {error}"
        );
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn global_config_rejects_non_regular_files() {
        let home = config_test_home("directory");
        fs::create_dir(home.join("config.toml")).unwrap();

        let error = GlobalConfig::load(&home).unwrap_err().to_string();
        assert!(error.contains("regular file"), "unexpected error: {error}");
        fs::remove_dir_all(home).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn global_config_rejects_symlinks() {
        use std::os::unix::fs::symlink;

        let home = config_test_home("symlink");
        let target = home.join("real-config.toml");
        fs::write(&target, "[defaults]\nauto_commit = false\n").unwrap();
        symlink(&target, home.join("config.toml")).unwrap();

        let error = GlobalConfig::load(&home).unwrap_err().to_string();
        assert!(error.contains("symlink"), "unexpected error: {error}");
        fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn global_config_accepts_unicode_and_spaces_in_project_roots() {
        let mut config = GlobalConfig::default();
        config.wikis.insert(
            "example".into(),
            WikiEntry {
                project_roots: vec!["/tmp/Project Folder/资料".into()],
            },
        );
        config.validate().unwrap();
    }

    #[test]
    fn global_config_bounds_registry_counts_and_project_roots() {
        let mut too_many_wikis = GlobalConfig::default();
        for index in 0..=MAX_REGISTERED_WIKIS {
            too_many_wikis
                .wikis
                .insert(format!("wiki-{index}"), WikiEntry::default());
        }
        let error = too_many_wikis.validate().unwrap_err().to_string();
        assert!(
            error.contains("too many wikis"),
            "unexpected error: {error}"
        );

        let mut too_many_roots = GlobalConfig::default();
        too_many_roots.wikis.insert(
            "example".into(),
            WikiEntry {
                project_roots: vec!["/tmp/project".into(); MAX_PROJECT_ROOTS_PER_WIKI + 1],
            },
        );
        let error = too_many_roots.validate().unwrap_err().to_string();
        assert!(
            error.contains("too many project roots"),
            "unexpected error: {error}"
        );

        let mut long_root = GlobalConfig::default();
        long_root.wikis.insert(
            "example".into(),
            WikiEntry {
                project_roots: vec!["x".repeat(MAX_PROJECT_ROOT_BYTES + 1)],
            },
        );
        let error = long_root.validate().unwrap_err().to_string();
        assert!(error.contains("at most"), "unexpected error: {error}");
    }

    #[test]
    fn global_config_rejects_control_characters_in_visible_strings() {
        let mut path_control = GlobalConfig::default();
        path_control.wikis.insert(
            "example".into(),
            WikiEntry {
                project_roots: vec!["/tmp/project\nspoof".into()],
            },
        );
        let error = path_control.validate().unwrap_err().to_string();
        assert!(error.contains("control"), "unexpected error: {error}");

        let mut kind_control = GlobalConfig::default();
        kind_control.defaults.sessions.default_kind = "note\u{202e}".into();
        let error = kind_control.validate().unwrap_err().to_string();
        assert!(error.contains("control"), "unexpected error: {error}");
    }

    #[test]
    fn global_config_rejects_invalid_registered_wiki_names() {
        let mut config = GlobalConfig::default();
        config
            .wikis
            .insert("../outside".into(), WikiEntry::default());
        let error = config.validate().unwrap_err().to_string();
        assert!(
            error.contains("invalid registered wiki name"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn new_feature_defaults_are_bounded_and_safe() {
        let defaults = Defaults::default();
        assert_eq!(defaults.retrieval.prime_tokens, 1_500);
        assert_eq!(defaults.retrieval.instruction_tokens, 700);
        assert_eq!(defaults.retrieval.search_limit, 10);
        assert_eq!(defaults.retrieval.search_tokens, 2_000);
        assert_eq!(defaults.retrieval.excerpt_lines, 2);
        assert_eq!(defaults.retrieval.max_per_section, 5);
        assert!(defaults.audit.source_provenance);
        assert_eq!(defaults.audit.critique_tokens, 4_000);
        assert!(defaults.publish.require_base_revision);
        assert_eq!(defaults.publish.orphan_policy, OrphanPolicy::Warn);
        assert_eq!(defaults.publish.output_tokens, 4_000);
    }

    #[test]
    fn old_global_config_inherits_new_defaults() {
        let config: GlobalConfig = toml::from_str(
            r#"
                [defaults]
                auto_commit = false
            "#,
        )
        .unwrap();
        assert!(!config.defaults.auto_commit);
        assert_eq!(config.defaults.retrieval, RetrievalSettings::default());
        assert_eq!(config.defaults.audit, AuditSettings::default());
        assert_eq!(config.defaults.publish, PublishSettings::default());
    }

    #[test]
    fn sparse_feature_overrides_inherit_other_values() {
        let retrieval = RetrievalOverrides {
            search_limit: Some(25),
            ..Default::default()
        }
        .apply(&RetrievalSettings::default())
        .unwrap();
        assert_eq!(retrieval.search_limit, 25);
        assert_eq!(retrieval.prime_tokens, 1_500);

        let audit = AuditOverrides {
            source_provenance: Some(false),
            ..Default::default()
        }
        .apply(&AuditSettings::default())
        .unwrap();
        assert!(!audit.source_provenance);
        assert_eq!(audit.critique_tokens, 4_000);

        let publish = PublishOverrides {
            orphan_policy: Some(OrphanPolicy::Error),
            ..Default::default()
        }
        .apply(&PublishSettings::default())
        .unwrap();
        assert!(publish.require_base_revision);
        assert_eq!(publish.orphan_policy, OrphanPolicy::Error);
        assert_eq!(publish.output_tokens, 4_000);
    }

    #[test]
    fn invalid_retrieval_budgets_are_rejected() {
        let mut retrieval = RetrievalSettings::default();
        retrieval.instruction_tokens = retrieval.prime_tokens + 1;
        assert!(retrieval.validate().is_err());

        let excessive_results = RetrievalSettings {
            search_limit: MAX_SEARCH_LIMIT + 1,
            ..Default::default()
        };
        assert!(excessive_results.validate().is_err());
        let excessive_excerpts = RetrievalSettings {
            excerpt_lines: MAX_EXCERPT_LINES + 1,
            ..Default::default()
        };
        assert!(excessive_excerpts.validate().is_err());

        for excessive_tokens in [MAX_RETRIEVAL_TOKENS + 1, usize::MAX] {
            let excessive_prime = RetrievalSettings {
                prime_tokens: excessive_tokens,
                ..Default::default()
            };
            assert!(excessive_prime.validate().is_err());
            let excessive_search = RetrievalSettings {
                search_tokens: excessive_tokens,
                ..Default::default()
            };
            assert!(excessive_search.validate().is_err());
        }
        let excessive_section_window = RetrievalSettings {
            max_per_section: usize::MAX,
            ..Default::default()
        };
        assert!(excessive_section_window.validate().is_err());
        let boundary = RetrievalSettings {
            prime_tokens: MAX_RETRIEVAL_TOKENS,
            instruction_tokens: MAX_RETRIEVAL_TOKENS,
            search_tokens: MAX_RETRIEVAL_TOKENS,
            max_per_section: MAX_SEARCH_LIMIT,
            ..Default::default()
        };
        assert!(boundary.validate().is_ok());
    }

    #[test]
    fn invalid_publish_output_budget_is_rejected() {
        let publish = PublishSettings {
            output_tokens: 255,
            ..Default::default()
        };
        assert!(publish.validate().is_err());
    }

    #[test]
    fn global_updates_reload_after_waiting_for_the_config_lock() {
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "wookie-global-config-lock-{}-{sequence}",
            std::process::id()
        ));
        let home = base.join("home");

        std::thread::scope(|scope| {
            let thread_home = home.clone();
            let (started_tx, started_rx) = std::sync::mpsc::channel();
            let (done_tx, done_rx) = std::sync::mpsc::channel();
            let handle = GlobalConfig::with_home_lock(&home, |guard| {
                let handle = scope.spawn(move || {
                    started_tx.send(()).unwrap();
                    GlobalConfig::update(&thread_home, |config| {
                        config.defaults.retrieval.search_tokens = 3_100;
                        Ok(())
                    })
                    .unwrap();
                    done_tx.send(()).unwrap();
                });
                started_rx.recv().unwrap();
                assert!(done_rx
                    .recv_timeout(std::time::Duration::from_millis(100))
                    .is_err());

                let mut current = GlobalConfig::load(&home).unwrap();
                current.defaults.retrieval.search_limit = 17;
                current.save_guarded(&home, guard).unwrap();
                Ok(handle)
            })
            .unwrap();
            handle.join().unwrap();
        });

        let stored = GlobalConfig::load(&home).unwrap();
        assert_eq!(stored.defaults.retrieval.search_limit, 17);
        assert_eq!(stored.defaults.retrieval.search_tokens, 3_100);
        std::fs::remove_dir_all(base).unwrap();
    }
}
