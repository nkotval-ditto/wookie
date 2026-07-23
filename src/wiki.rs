//! Wiki storage and resolution. A wiki lives at `<WOOKIE_HOME>/<slug>/` with a
//! `wookie.toml` and a `pages/` tree. Resolution order: explicit slug, cwd
//! prefix match against registered project roots, then the git main-worktree
//! fallback so linked worktrees land on the same wiki.

use crate::config::{
    read_optional_bounded_regular_utf8, AuditOverrides, AuditSettings, GlobalConfig,
    HistoryOverrides, HistorySettings, PublishOverrides, PublishSettings, RetrievalOverrides,
    RetrievalSettings, SessionOverrides, SessionSettings,
};
use crate::page::Page;
use crate::snapshot;
use anyhow::{bail, Context, Result};
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WikiConfig {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub project_roots: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_commit: Option<bool>,
    /// Sparse per-wiki session overrides; omitted values inherit the global
    /// defaults in `~/.wookie/config.toml`.
    #[serde(default, skip_serializing_if = "SessionOverrides::is_empty")]
    pub sessions: SessionOverrides,
    /// Sparse per-wiki Git-history overrides.
    #[serde(default, skip_serializing_if = "HistoryOverrides::is_empty")]
    pub history: HistoryOverrides,
    /// Sparse per-wiki retrieval overrides.
    #[serde(default, skip_serializing_if = "RetrievalOverrides::is_empty")]
    pub retrieval: RetrievalOverrides,
    /// Sparse per-wiki audit/provenance overrides.
    #[serde(default, skip_serializing_if = "AuditOverrides::is_empty")]
    pub audit: AuditOverrides,
    /// Sparse per-wiki publish-safety overrides.
    #[serde(default, skip_serializing_if = "PublishOverrides::is_empty")]
    pub publish: PublishOverrides,
    /// Project commit the wiki was last synced to (set by `wookie ingest --mark`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ingest_commit: Option<String>,
    /// Top-level namespaces pages are filed under. Empty means the built-in
    /// defaults apply (kept last: TOML wants tables after plain values).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub sections: std::collections::BTreeMap<String, SectionConfig>,
}

const MAX_WIKI_DESCRIPTION_BYTES: usize = 8 * 1024;
const MAX_PROJECT_ROOT_BYTES: usize = 4 * 1024;
const MAX_PROJECT_ROOTS: usize = 128;
const MAX_SECTIONS: usize = 256;
const MAX_SECTION_DESCRIPTION_BYTES: usize = 4 * 1024;
const MAX_REQUIRED_PAGES_PER_SECTION: usize = 256;
pub(crate) const MAX_PAGE_CATALOG_ENTRIES: usize = 100_000;

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

fn validate_config_string(label: &str, value: &str, max_bytes: usize, empty: bool) -> Result<()> {
    if (!empty && value.is_empty()) || value.len() > max_bytes {
        bail!(
            "{label} must be {} and at most {max_bytes} bytes",
            if empty { "valid UTF-8" } else { "non-empty" }
        );
    }
    if contains_terminal_control(value) {
        bail!("{label} must not contain control or terminal-direction characters");
    }
    Ok(())
}

impl WikiConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        validate_slug(&self.name).context("invalid wiki config name")?;
        validate_config_string(
            "wiki description",
            &self.description,
            MAX_WIKI_DESCRIPTION_BYTES,
            true,
        )?;
        if self.project_roots.len() > MAX_PROJECT_ROOTS {
            bail!("wiki configuration has too many project roots (maximum {MAX_PROJECT_ROOTS})");
        }
        for (index, root) in self.project_roots.iter().enumerate() {
            validate_config_string(
                &format!("project root {index}"),
                root,
                MAX_PROJECT_ROOT_BYTES,
                false,
            )?;
        }
        if self.sections.len() > MAX_SECTIONS {
            bail!("wiki configuration has too many sections (maximum {MAX_SECTIONS})");
        }
        for (section, config) in &self.sections {
            validate_id(section).with_context(|| format!("invalid section name '{section}'"))?;
            if section.contains('/') {
                bail!("section name '{section}' must be one page-id segment");
            }
            validate_config_string(
                &format!("description for section '{section}'"),
                &config.description,
                MAX_SECTION_DESCRIPTION_BYTES,
                true,
            )?;
            if config.required.len() > MAX_REQUIRED_PAGES_PER_SECTION {
                bail!(
                    "section '{section}' has too many required pages (maximum {MAX_REQUIRED_PAGES_PER_SECTION})"
                );
            }
            for required in &config.required {
                validate_id(&format!("{section}/{required}")).with_context(|| {
                    format!("invalid required page '{required}' in section '{section}'")
                })?;
            }
        }
        if let Some(revision) = &self.last_ingest_commit {
            validate_config_string("last ingest revision", revision, 256, false)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SectionKind {
    /// Descriptive knowledge: architecture, code reference, decisions.
    #[default]
    Info,
    /// Normative content: checkable via `wookie critique`, locked by default.
    Rules,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SectionConfig {
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub kind: SectionKind,
    /// Override the default lock (rules sections are locked unless told otherwise).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locked: Option<bool>,
    /// Page names (relative to the section) doctor insists on, e.g. "overview".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required: Vec<String>,
}

impl SectionConfig {
    pub fn is_locked(&self) -> bool {
        self.locked.unwrap_or(self.kind == SectionKind::Rules)
    }
}

pub fn default_sections() -> std::collections::BTreeMap<String, SectionConfig> {
    let s = |description: &str, kind: SectionKind, required: &[&str]| SectionConfig {
        description: description.into(),
        kind,
        locked: None,
        required: required.iter().map(|r| r.to_string()).collect(),
    };
    use SectionKind::{Info, Rules};
    std::collections::BTreeMap::from([
        (
            "architecture".to_string(),
            s(
                "System structure, boundaries, how subsystems interact",
                Info,
                &["overview"],
            ),
        ),
        (
            "code".to_string(),
            s("Module-by-module reference (ingest seeds these)", Info, &[]),
        ),
        (
            "decisions".to_string(),
            s(
                "Why things are the way they are, one page per decision",
                Info,
                &[],
            ),
        ),
        (
            "guides".to_string(),
            s(
                "How to do common tasks: build, test, release, debug",
                Info,
                &[],
            ),
        ),
        (
            "findings".to_string(),
            s(
                "Review findings, remediation state and verification evidence",
                Info,
                &[],
            ),
        ),
        (
            "style".to_string(),
            s("Code style, naming, idioms, review conventions", Rules, &[]),
        ),
        (
            "workflow".to_string(),
            s(
                "How to commit, branch, PR, review and release; team process rules",
                Rules,
                &[],
            ),
        ),
    ])
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct UnlockState {
    #[serde(default)]
    unlocks: std::collections::BTreeMap<String, String>,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SectionUnlockState {
    #[serde(default)]
    locked: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    expires_at: Option<String>,
}

pub struct Wiki {
    pub slug: String,
    pub dir: PathBuf,
    pub config: WikiConfig,
    pub auto_commit: bool,
    pub sessions: SessionSettings,
    pub history: HistorySettings,
    pub retrieval: RetrievalSettings,
    pub audit: AuditSettings,
    pub publish: PublishSettings,
}

/// If `cwd` is inside a git repo, return the main worktree's root, so any
/// linked worktree resolves to the same project. None outside git or for
/// bare repos.
pub fn git_main_worktree(cwd: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .args(["-C"])
        .arg(cwd)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let common = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    if common.file_name()?.to_str()? == ".git" {
        common.parent().map(Path::to_path_buf)
    } else {
        None
    }
}

fn canon(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// Resolve a relative path beneath `root` without following symlinks in the
/// managed portion of the path. Missing final components are allowed so the
/// same helper can preflight both reads and writes.
///
/// `root` is the trust boundary: symlinks in its ancestors are intentionally
/// irrelevant, but `root` itself and every existing descendant must be real.
pub(crate) fn contained_path(root: &Path, relative: &Path) -> Result<PathBuf> {
    let root_meta = fs::symlink_metadata(root)
        .with_context(|| format!("inspecting storage root {}", root.display()))?;
    if root_meta.file_type().is_symlink() || !root_meta.is_dir() {
        bail!(
            "storage root {} must be a real directory, not a symlink",
            root.display()
        );
    }
    if relative.is_absolute() {
        bail!("managed path '{}' must be relative", relative.display());
    }

    let mut path = root.to_path_buf();
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let std::path::Component::Normal(segment) = component else {
            bail!(
                "managed path '{}' contains an invalid path component",
                relative.display()
            );
        };
        path.push(segment);
        match fs::symlink_metadata(&path) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    bail!(
                        "refusing managed path {} because it contains a symlink",
                        path.display()
                    );
                }
                if components.peek().is_some() && !meta.is_dir() {
                    bail!(
                        "refusing managed path {} because an ancestor is not a directory",
                        path.display()
                    );
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // A missing ancestor also means all descendants are missing.
                // Component validation above is enough for the remaining path.
                for component in components {
                    let std::path::Component::Normal(segment) = component else {
                        bail!(
                            "managed path '{}' contains an invalid path component",
                            relative.display()
                        );
                    };
                    path.push(segment);
                }
                break;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspecting managed path {}", path.display()));
            }
        }
    }
    Ok(path)
}

/// Create a relative directory tree one component at a time, checking after
/// every step that a concurrent/pre-existing symlink was not followed.
pub(crate) fn create_contained_dir_all(root: &Path, relative: &Path) -> Result<PathBuf> {
    // Preflight the complete path first, catching existing symlinks early.
    contained_path(root, relative)?;
    let mut path = root.to_path_buf();
    for component in relative.components() {
        let std::path::Component::Normal(segment) = component else {
            bail!(
                "managed directory '{}' contains an invalid path component",
                relative.display()
            );
        };
        path.push(segment);
        let mut builder = fs::DirBuilder::new();
        // Wiki directories contain project knowledge and session state. Set
        // the restrictive mode at creation time; existing directories are
        // deliberately only validated, never chmodded behind the user's back.
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("creating managed directory {}", path.display()));
            }
        }
        let meta = fs::symlink_metadata(&path)
            .with_context(|| format!("inspecting managed directory {}", path.display()))?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            bail!(
                "managed directory {} must be a real directory, not a symlink",
                path.display()
            );
        }
    }
    Ok(path)
}

static ATOMIC_WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);
const MAX_PERSISTENT_CONFIG_BYTES: usize = 1024 * 1024;

#[cfg(windows)]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    #[link(name = "Kernel32")]
    extern "system" {
        fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
    }

    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: both buffers are NUL-terminated and live for the duration of
    // the call. The paths share a parent, so this remains an atomic rename.
    let moved = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::rename(from, to)
}

/// Atomically replace a file using a same-directory temporary. Windows uses
/// `MoveFileExW(REPLACE_EXISTING)` because `std::fs::rename` cannot replace an
/// existing file there. Callers handling untrusted relative paths should run
/// them through [`contained_path`] first.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AtomicWritePermissions {
    pub readonly: bool,
    pub unix_mode: Option<u32>,
}

pub(crate) fn atomic_write(path: &Path, content: impl AsRef<[u8]>) -> Result<()> {
    atomic_write_with_permissions(path, content, None)
}

/// Atomically replace a file after applying requested permissions to the
/// same-directory temporary. This prevents a crash between rename and chmod
/// from exposing new bytes with old or default metadata.
pub(crate) fn atomic_write_with_permissions(
    path: &Path,
    content: impl AsRef<[u8]>,
    desired_permissions: Option<AtomicWritePermissions>,
) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        anyhow::anyhow!(
            "cannot atomically write {} without a parent",
            path.display()
        )
    })?;
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("cannot atomically write path without a file name"))?
        .to_string_lossy();

    let existing_permissions = match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            bail!("refusing to replace symlink {}", path.display())
        }
        Ok(meta) => Some(meta.permissions()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error).with_context(|| format!("inspecting {}", path.display())),
    };

    let (temp_path, mut temp_file) = (0..128)
        .find_map(|_| {
            let sequence = ATOMIC_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
            let candidate = parent.join(format!(
                ".{file_name}.tmp-{}-{sequence}",
                std::process::id()
            ));
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            // New knowledge, configuration, and recovery files can contain
            // private project context. Restrict the temporary before the
            // first byte is written; an existing target's permissions are
            // restored below before publication.
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&candidate) {
                Ok(file) => Some(Ok((candidate, file))),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => None,
                Err(error) => Some(Err(error)),
            }
        })
        .transpose()
        .with_context(|| format!("creating atomic temporary beside {}", path.display()))?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "could not allocate an atomic temporary for {}",
                path.display()
            )
        })?;

    let cleanup_path = temp_path.clone();
    let result = (move || -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temp_file
                .set_permissions(fs::Permissions::from_mode(0o600))
                .with_context(|| format!("securing atomic temporary for {}", path.display()))?;
        }
        temp_file
            .write_all(content.as_ref())
            .with_context(|| format!("writing atomic temporary for {}", path.display()))?;
        if let Some(desired) = desired_permissions {
            let mut permissions = temp_file.metadata()?.permissions();
            #[cfg(unix)]
            if let Some(mode) = desired.unix_mode {
                use std::os::unix::fs::PermissionsExt;
                permissions.set_mode(mode);
            } else {
                permissions.set_readonly(desired.readonly);
            }
            #[cfg(not(unix))]
            permissions.set_readonly(desired.readonly);
            temp_file.set_permissions(permissions).with_context(|| {
                format!("setting final permissions while writing {}", path.display())
            })?;
        } else if let Some(permissions) = existing_permissions {
            temp_file.set_permissions(permissions).with_context(|| {
                format!("preserving permissions while writing {}", path.display())
            })?;
        }
        temp_file
            .sync_all()
            .with_context(|| format!("syncing atomic temporary for {}", path.display()))?;
        drop(temp_file);
        replace_file(&temp_path, path)
            .with_context(|| format!("atomically replacing {}", path.display()))?;
        #[cfg(unix)]
        if let Ok(parent_dir) = fs::File::open(parent) {
            // Some filesystems do not support directory fsync. The rename is
            // already complete, so durability here is deliberately best effort.
            let _ = parent_dir.sync_all();
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&cleanup_path);
    }
    result
}

pub fn open(home: &Path, slug: &str) -> Result<Wiki> {
    validate_slug(slug)?;
    let dir = home.join(slug);
    if !dir.join("wookie.toml").exists() {
        bail!(
            "no wiki '{slug}' at {} (run `wookie list` to see known wikis)",
            dir.display()
        );
    }
    // A wiki must be a real, direct child of WOOKIE_HOME. Besides rejecting
    // `..` and absolute paths above, this prevents a symlink named like a wiki
    // from redirecting reads, writes, or `remove-wiki` outside the home.
    let canonical_home = home
        .canonicalize()
        .with_context(|| format!("resolving wookie home {}", home.display()))?;
    let dir_meta = fs::symlink_metadata(&dir)
        .with_context(|| format!("inspecting wiki directory {}", dir.display()))?;
    if dir_meta.file_type().is_symlink() || !dir_meta.is_dir() {
        bail!(
            "wiki '{slug}' must be a real, direct directory under {}",
            home.display()
        );
    }
    let canonical_dir = dir
        .canonicalize()
        .with_context(|| format!("resolving wiki directory {}", dir.display()))?;
    if canonical_dir.parent() != Some(canonical_home.as_path()) {
        bail!(
            "wiki '{slug}' must be a direct directory under {}",
            home.display()
        );
    }
    let cfg_path = contained_path(&canonical_dir, Path::new("wookie.toml"))?;

    let raw = read_optional_bounded_regular_utf8(
        &cfg_path,
        MAX_PERSISTENT_CONFIG_BYTES,
        "wiki configuration",
    )?
    .context("wiki configuration disappeared while opening it")?;
    let config: WikiConfig =
        toml::from_str(&raw).with_context(|| format!("parsing {}", cfg_path.display()))?;
    config.validate()?;
    if config.name != slug {
        bail!(
            "wiki config name '{}' does not match directory slug '{slug}'",
            config.name
        );
    }
    let global = GlobalConfig::load(home)?;
    let auto_commit = config.auto_commit.unwrap_or(global.defaults.auto_commit);
    let sessions = config.sessions.apply(&global.defaults.sessions)?;
    let history = config.history.apply(&global.defaults.history)?;
    let retrieval = config.retrieval.apply(&global.defaults.retrieval)?;
    let audit = config.audit.apply(&global.defaults.audit)?;
    let publish = config.publish.apply(&global.defaults.publish)?;
    Ok(Wiki {
        slug: slug.to_string(),
        dir: canonical_dir,
        config,
        auto_commit,
        sessions,
        history,
        retrieval,
        audit,
        publish,
    })
}

/// Wiki slugs are directory names, never paths. Keep this validation in the
/// storage layer so every CLI and MCP operation gets the same containment.
pub fn validate_slug(slug: &str) -> Result<()> {
    if slug.len() > 255
        || slug.is_empty()
        || !slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_'))
    {
        bail!("invalid wiki slug '{slug}' — use lowercase letters, digits, '-' or '_' (no paths)");
    }
    validate_portable_segment(slug).with_context(|| format!("invalid wiki slug '{slug}'"))?;
    Ok(())
}

/// Reject path spellings that alias another name or fail on common filesystems.
/// Callers still own their more specific alphabet and whole-path limits.
pub(crate) fn validate_portable_segment(segment: &str) -> Result<()> {
    if segment.is_empty() || segment.len() > 255 || segment.ends_with('.') {
        bail!("path segment is empty, too long, or ends in a dot");
    }
    let lowercase = segment.to_ascii_lowercase();
    let stem = lowercase.split('.').next().unwrap_or(&lowercase);
    let reserved_device = matches!(stem, "con" | "prn" | "aux" | "nul")
        || (stem.len() == 4
            && (stem.starts_with("com") || stem.starts_with("lpt"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'));
    if reserved_device {
        bail!("path segment uses a reserved Windows device name");
    }
    Ok(())
}

/// Every wiki under home (a dir containing wookie.toml). This, not the
/// global config, is the source of truth for what exists.
pub fn all_wikis(home: &Path) -> Vec<String> {
    let mut slugs: Vec<String> = std::fs::read_dir(home)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().join("wookie.toml").exists())
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    slugs.sort();
    slugs
}

pub fn resolve(home: &Path, flag: Option<&str>, cwd: &Path) -> Result<Wiki> {
    if let Some(slug) = flag {
        return open(home, slug);
    }
    // Each wiki's own wookie.toml project_roots decide resolution, so
    // editing roots (or `wookie roots --add`) takes effect immediately.
    let mut wikis = vec![];
    for slug in all_wikis(home) {
        if let Ok(w) = open(home, &slug) {
            wikis.push(w);
        }
    }

    let match_path = |path: &Path| -> Option<usize> {
        let path = canon(path);
        let mut best: Option<(usize, usize)> = None; // (wiki idx, depth)
        for (i, w) in wikis.iter().enumerate() {
            for root in &w.config.project_roots {
                let root = canon(Path::new(root));
                if path.starts_with(&root) {
                    let depth = root.components().count();
                    if best.is_none_or(|(_, d)| depth > d) {
                        best = Some((i, depth));
                    }
                }
            }
        }
        best.map(|(i, _)| i)
    };

    let hit = match_path(cwd).or_else(|| git_main_worktree(cwd).and_then(|m| match_path(&m)));
    if let Some(i) = hit {
        return Ok(wikis.swap_remove(i));
    }

    let known: Vec<&str> = wikis.iter().map(|w| w.slug.as_str()).collect();
    if known.is_empty() {
        bail!("no wikis exist yet. Create one with `wookie init` from your project directory.");
    }
    bail!(
        "no wiki matches {} — pass --wiki <slug> or register this project with `wookie init`.\nKnown wikis: {}",
        cwd.display(),
        known.join(", ")
    );
}

/// Page ids are relative paths under pages/ without the .md extension.
pub fn validate_id(id: &str) -> Result<()> {
    const MAX_ID_BYTES: usize = 1_024;
    const MAX_SEGMENT_BYTES: usize = 255;

    if id.is_empty() {
        bail!("page id is empty");
    }
    if id.len() > MAX_ID_BYTES {
        bail!("page id is too long (maximum {MAX_ID_BYTES} bytes)");
    }
    if id.starts_with('/') || id.ends_with('/') {
        bail!("page id '{id}' must not start or end with '/'");
    }
    for seg in id.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." {
            bail!("page id '{id}' has an invalid path segment");
        }
        if seg.starts_with('.') {
            bail!("page id '{id}' must not contain hidden segments");
        }
        if seg.len() > MAX_SEGMENT_BYTES {
            bail!("page id '{id}' contains a segment longer than {MAX_SEGMENT_BYTES} bytes");
        }
        // Windows strips trailing dots and aliases reserved DOS device names,
        // even when a suffix is present (for example `con.md`). Reject those
        // spellings everywhere so an id has one portable filesystem identity.
        if validate_portable_segment(seg).is_err() {
            bail!("page id '{id}' contains a path segment that is not portable");
        }
        if !seg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            bail!("page id '{id}' may only contain letters, digits, '-', '_', '.' and '/'");
        }
        // Lowercase-only, hard rule: on case-insensitive filesystems (macOS)
        // 'STYLE/checks' aliases 'style/checks' and would bypass section locks.
        if seg.chars().any(|c| c.is_ascii_uppercase()) {
            bail!(
                "page id '{id}' must be lowercase (did you mean '{}'?)",
                id.to_lowercase()
            );
        }
    }
    Ok(())
}

impl Wiki {
    pub(crate) fn acquire_mutation_guard(&self) -> Result<crate::publish::MutationGuard> {
        crate::publish::acquire_mutation_guard(self)
    }

    /// Best-effort, non-blocking mutation guard for disposable derived state.
    /// Failure means the caller must skip persistence, not fail a read.
    pub(crate) fn try_acquire_mutation_guard(&self) -> Option<crate::publish::MutationGuard> {
        crate::publish::try_acquire_mutation_guard(self)
    }

    /// Re-read global defaults and recompute the effective settings after a
    /// config mutation. Per-wiki overrides remain sparse.
    pub fn refresh_effective_settings(&mut self) -> Result<()> {
        self.config.validate()?;
        let home = self
            .dir
            .parent()
            .context("wiki directory has no wookie home")?;
        let global = GlobalConfig::load(home)?;
        self.auto_commit = self
            .config
            .auto_commit
            .unwrap_or(global.defaults.auto_commit);
        self.sessions = self.config.sessions.apply(&global.defaults.sessions)?;
        self.history = self.config.history.apply(&global.defaults.history)?;
        self.retrieval = self.config.retrieval.apply(&global.defaults.retrieval)?;
        self.audit = self.config.audit.apply(&global.defaults.audit)?;
        self.publish = self.config.publish.apply(&global.defaults.publish)?;
        Ok(())
    }

    fn load_config_file(&self) -> Result<WikiConfig> {
        let path = self.contained_path(Path::new("wookie.toml"))?;
        let raw = read_optional_bounded_regular_utf8(
            &path,
            MAX_PERSISTENT_CONFIG_BYTES,
            "wiki configuration",
        )?
        .context("wiki configuration disappeared while opening it")?;
        let config: WikiConfig =
            toml::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
        config.validate()?;
        if config.name != self.slug {
            bail!(
                "wiki config name '{}' does not match directory slug '{}'",
                config.name,
                self.slug
            );
        }
        Ok(config)
    }

    pub(crate) fn save_config_guarded(&self, guard: &crate::publish::MutationGuard) -> Result<()> {
        self.ensure_mutation_guard(guard)?;
        self.config.validate()?;
        let path = self.contained_path(Path::new("wookie.toml"))?;
        atomic_write(&path, toml::to_string_pretty(&self.config)?)
            .with_context(|| format!("writing {}", path.display()))
    }

    pub(crate) fn reload_config_guarded(
        &mut self,
        guard: &crate::publish::MutationGuard,
    ) -> Result<()> {
        self.ensure_mutation_guard(guard)?;
        let config = self.load_config_file()?;
        let previous = std::mem::replace(&mut self.config, config);
        if let Err(error) = self.refresh_effective_settings() {
            self.config = previous;
            let _ = self.refresh_effective_settings();
            return Err(error);
        }
        Ok(())
    }

    /// Serialize a wiki-config read-modify-write with page publications and
    /// other config writers. The latest on-disk config is loaded only after
    /// acquiring the mutation guard, and the guard remains held through the
    /// path-scoped history commit.
    pub(crate) fn update_config<T>(
        &mut self,
        message: &str,
        edit: impl FnOnce(&mut WikiConfig) -> Result<T>,
    ) -> Result<T> {
        let guard = self.acquire_mutation_guard()?;
        let mut config = self.load_config_file()?;
        let result = edit(&mut config)?;
        config.validate()?;

        let previous_config = std::mem::replace(&mut self.config, config);
        if let Err(error) = self.refresh_effective_settings() {
            self.config = previous_config;
            let _ = self.refresh_effective_settings();
            return Err(error);
        }
        if let Err(error) = self.save_config_guarded(&guard) {
            self.config = previous_config;
            let _ = self.refresh_effective_settings();
            return Err(error);
        }
        self.commit_paths(message, &["wookie.toml".into()])?;
        Ok(result)
    }

    #[cfg(test)]
    pub fn pages_dir(&self) -> PathBuf {
        self.dir.join("pages")
    }

    pub(crate) fn contained_path(&self, relative: &Path) -> Result<PathBuf> {
        contained_path(&self.dir, relative)
    }

    fn page_relative_path(id: &str) -> Result<PathBuf> {
        validate_id(id)?;
        Ok(Path::new("pages").join(format!("{id}.md")))
    }

    pub fn page_path(&self, id: &str) -> Result<PathBuf> {
        self.contained_path(&Self::page_relative_path(id)?)
    }

    pub fn exists(&self, id: &str) -> bool {
        self.page_path(id).map(|p| p.exists()).unwrap_or(false)
    }

    pub fn page_ids(&self) -> Vec<String> {
        let root = match self.contained_path(Path::new("pages")) {
            Ok(root)
                if fs::symlink_metadata(&root)
                    .map(|meta| meta.is_dir() && !meta.file_type().is_symlink())
                    .unwrap_or(false) =>
            {
                root
            }
            _ => return vec![],
        };
        let mut ids: Vec<String> = walkdir::WalkDir::new(&root)
            .into_iter()
            // Hidden dirs (e.g. pages/.obsidian) are never pages.
            .filter_entry(|e| {
                !e.file_name()
                    .to_str()
                    .map(|n| n.starts_with('.'))
                    .unwrap_or(false)
                    || e.depth() == 0
            })
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .filter_map(|e| {
                let rel = e.path().strip_prefix(&root).ok()?;
                let s = rel.to_str()?;
                s.strip_suffix(".md").map(|s| s.replace('\\', "/"))
            })
            .collect();
        ids.sort();
        ids
    }

    /// Enumerate the complete Markdown catalog without suppressing filesystem
    /// errors. Security- and identity-sensitive callers use this instead of
    /// the forgiving interactive `page_ids` helper so an unreadable, invalid,
    /// or symlinked entry can never disappear from a snapshot silently.
    pub fn page_files_strict(&self) -> Result<Vec<(String, PathBuf)>> {
        let root = self.contained_path(Path::new("pages"))?;
        let metadata = fs::symlink_metadata(&root)
            .with_context(|| format!("inspecting page catalog {}", root.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!("page catalog {} must be a real directory", root.display());
        }

        let mut pages = Vec::new();
        let walker = walkdir::WalkDir::new(&root).follow_links(false).into_iter();
        for entry in walker.filter_entry(|entry| {
            entry.depth() == 0
                || !entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with('.'))
        }) {
            let entry =
                entry.with_context(|| format!("walking page catalog {}", root.display()))?;
            if entry.depth() == 0 {
                continue;
            }
            let kind = entry.file_type();
            if kind.is_symlink() {
                bail!(
                    "page catalog entry {} must not be a symlink",
                    entry.path().display()
                );
            }
            if kind.is_dir() {
                continue;
            }
            let markdown = entry.path().extension().and_then(|value| value.to_str()) == Some("md");
            if !markdown {
                continue;
            }
            if !kind.is_file() {
                bail!(
                    "Markdown page {} must be a regular file",
                    entry.path().display()
                );
            }
            let relative = entry
                .path()
                .strip_prefix(&root)
                .context("page catalog entry escaped its root")?;
            let relative = relative
                .to_str()
                .context("page path is not valid UTF-8")?
                .replace('\\', "/");
            let id = relative
                .strip_suffix(".md")
                .context("Markdown page path lacks .md suffix")?
                .to_string();
            validate_id(&id)?;
            pages.push((id, entry.path().to_path_buf()));
            if pages.len() > MAX_PAGE_CATALOG_ENTRIES {
                bail!("page catalog exceeds the {MAX_PAGE_CATALOG_ENTRIES}-entry safety limit");
            }
        }
        pages.sort_by(|left, right| left.0.cmp(&right.0));
        if pages.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            bail!("page catalog contains duplicate page ids");
        }
        Ok(pages)
    }

    pub fn load_page(&self, id: &str) -> Result<Page> {
        let path = self.page_path(id)?;
        let raw = snapshot::read_raw_page(&path)
            .with_context(|| format!("no page '{id}' (looked at {})", path.display()))?;
        let raw = std::str::from_utf8(&raw)
            .with_context(|| format!("page '{id}' at {} is not valid UTF-8", path.display()))?;
        Ok(Page::parse(id, raw))
    }

    pub fn all_pages(&self) -> Vec<Page> {
        self.page_ids()
            .iter()
            .filter_map(|id| self.load_page(id).ok())
            .collect()
    }

    /// Checked save: refuses pages in locked sections. This is THE
    /// enforcement point; command-level checks only improve error timing.
    #[cfg(test)]
    pub fn save_page(&self, page: &mut Page, bump_updated: bool) -> Result<()> {
        let guard = self.acquire_mutation_guard()?;
        self.assert_writable(&page.id)?;
        self.ensure_mutation_guard(&guard)?;
        self.save_page_unlocked(page, bump_updated)
    }

    /// Unchecked single-page save for test fixtures. Production mechanical
    /// operations use `save_page_raw_guarded` so a logical multi-page change
    /// retains one shared guard throughout.
    #[cfg(test)]
    pub fn save_page_raw(&self, page: &mut Page, bump_updated: bool) -> Result<()> {
        let guard = self.acquire_mutation_guard()?;
        self.ensure_mutation_guard(&guard)?;
        self.save_page_unlocked(page, bump_updated)
    }

    pub(crate) fn save_page_raw_guarded(
        &self,
        guard: &crate::publish::MutationGuard,
        page: &mut Page,
        bump_updated: bool,
    ) -> Result<()> {
        self.ensure_mutation_guard(guard)?;
        self.save_page_unlocked(page, bump_updated)
    }

    pub(crate) fn ensure_mutation_guard(
        &self,
        guard: &crate::publish::MutationGuard,
    ) -> Result<()> {
        if !guard.is_for(self) {
            bail!("mutation guard belongs to a different wiki");
        }
        Ok(())
    }

    fn save_page_unlocked(&self, page: &mut Page, bump_updated: bool) -> Result<()> {
        if bump_updated {
            page.fm.updated = crate::page::today();
        }
        if page.fm.created.is_empty() {
            page.fm.created = crate::page::today();
        }
        page.validate_frontmatter()?;
        let relative = Self::page_relative_path(&page.id)?;
        if let Some(parent) = relative.parent() {
            create_contained_dir_all(&self.dir, parent)?;
        }
        let path = self.contained_path(&relative)?;
        let rendered = page.render();
        if u64::try_from(rendered.len()).unwrap_or(u64::MAX)
            > crate::snapshot::MAX_SNAPSHOT_PAGE_BYTES
        {
            bail!(
                "rendered page '{}' exceeds the {}-byte page safety limit",
                page.id,
                crate::snapshot::MAX_SNAPSHOT_PAGE_BYTES
            );
        }
        atomic_write(&path, rendered)
            .with_context(|| format!("writing page '{}' at {}", page.id, path.display()))
    }

    #[cfg(test)]
    pub fn delete_page(&self, id: &str) -> Result<()> {
        let guard = self.acquire_mutation_guard()?;
        self.assert_writable(id)?;
        self.delete_page_raw_guarded(&guard, id)
    }

    pub(crate) fn delete_page_raw_guarded(
        &self,
        guard: &crate::publish::MutationGuard,
        id: &str,
    ) -> Result<()> {
        self.ensure_mutation_guard(guard)?;
        let path = self.page_path(id)?;
        fs::remove_file(&path).with_context(|| format!("no page '{id}'"))
    }

    /// Pages whose bodies link to `id`.
    pub fn backlinks(&self, id: &str) -> Vec<String> {
        self.all_pages()
            .iter()
            .filter(|p| p.id != id && p.links().iter().any(|l| l == id))
            .map(|p| p.id.clone())
            .collect()
    }

    /// Effective sections: built-in defaults overlaid by per-wiki entries, so
    /// adding one custom section cannot accidentally remove the default rules.
    pub fn sections(&self) -> std::collections::BTreeMap<String, SectionConfig> {
        let mut sections = default_sections();
        sections.extend(self.config.sections.clone());
        sections
    }

    fn unlocks_path(&self) -> Result<PathBuf> {
        self.contained_path(Path::new(".unlocks.toml"))
    }

    fn load_unlocks(&self) -> std::collections::BTreeMap<String, String> {
        self.unlocks_path()
            .ok()
            .and_then(|path| {
                read_optional_bounded_regular_utf8(
                    &path,
                    MAX_PERSISTENT_CONFIG_BYTES,
                    "legacy unlock configuration",
                )
                .ok()
                .flatten()
            })
            .and_then(|raw| toml::from_str::<UnlockState>(&raw).ok())
            .map(|st| st.unlocks)
            .unwrap_or_default()
    }

    fn section_unlock_path(&self, section: &str) -> Result<PathBuf> {
        validate_id(section)?;
        if section.contains('/') {
            bail!("section name must be one page-id segment");
        }
        self.contained_path(&Path::new(".unlocks").join(format!("{section}.toml")))
    }

    /// Some(true/false) means a per-section marker exists and is authoritative;
    /// None falls back to the legacy shared-map format for compatibility.
    fn section_unlock_status(&self, section: &str) -> Option<bool> {
        let path = self.section_unlock_path(section).ok()?;
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => Some(false),
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => Some(false),
            Ok(_) => {
                let state = read_optional_bounded_regular_utf8(
                    &path,
                    MAX_PERSISTENT_CONFIG_BYTES,
                    "section unlock configuration",
                )
                .ok()
                .flatten()
                .and_then(|raw| toml::from_str::<SectionUnlockState>(&raw).ok());
                Some(state.is_some_and(|state| {
                    !state.locked
                        && state
                            .expires_at
                            .as_deref()
                            .and_then(|timestamp| {
                                chrono::DateTime::parse_from_rfc3339(timestamp).ok()
                            })
                            .is_some_and(|expiry| chrono::Utc::now() < expiry)
                }))
            }
        }
    }

    fn save_section_unlock(
        &self,
        guard: &crate::publish::MutationGuard,
        section: &str,
        state: &SectionUnlockState,
    ) -> Result<()> {
        self.ensure_mutation_guard(guard)?;
        create_contained_dir_all(&self.dir, Path::new(".unlocks"))?;
        let path = self.section_unlock_path(section)?;
        atomic_write(&path, toml::to_string_pretty(state)?)
            .with_context(|| format!("writing {}", path.display()))
    }

    /// Make sure transient/local files stay out of the wiki's git history.
    pub fn ensure_gitignore(&self) -> Result<()> {
        let guard = self.acquire_mutation_guard()?;
        self.ensure_gitignore_guarded(&guard)
    }

    pub(crate) fn ensure_gitignore_guarded(
        &self,
        guard: &crate::publish::MutationGuard,
    ) -> Result<()> {
        self.ensure_mutation_guard(guard)?;
        let path = self.contained_path(Path::new(".gitignore"))?;
        let mut cur = read_optional_bounded_regular_utf8(
            &path,
            MAX_PERSISTENT_CONFIG_BYTES,
            "wiki ignore configuration",
        )?
        .unwrap_or_default();
        let mut changed = false;
        for entry in [
            ".history.lock",
            ".unlocks.toml",
            ".unlocks/",
            ".publish.lock",
            ".publish-journal.json",
            ".ingest-reconciliation-recovery.json",
            ".cache/",
            "proposals/rules/",
            "pages/.obsidian/",
            "sessions/*/inbox.toml",
            "sessions/*/inbox/",
            "sessions/**/.*.tmp-*",
        ] {
            if !cur.lines().any(|l| l.trim() == entry) {
                if !cur.is_empty() && !cur.ends_with('\n') {
                    cur.push('\n');
                }
                cur.push_str(entry);
                cur.push('\n');
                changed = true;
            }
        }
        if changed {
            atomic_write(&path, cur).with_context(|| format!("writing {}", path.display()))?;
        }
        Ok(())
    }

    /// A locked section is temporarily writable while an unlock is active.
    pub fn is_unlocked(&self, section: &str) -> bool {
        self.section_unlock_status(section).unwrap_or_else(|| {
            self.load_unlocks()
                .get(section)
                .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
                .map(|exp| chrono::Utc::now() < exp)
                .unwrap_or(false)
        })
    }

    /// Error unless the page's section is writable. The message doubles as
    /// agent instructions: get user permission before unlocking.
    pub fn assert_writable(&self, id: &str) -> Result<()> {
        let Some((section, _)) = id.split_once('/') else {
            return Ok(());
        };
        let sections = self.sections();
        let Some(cfg) = sections.get(section) else {
            return Ok(());
        };
        if cfg.is_locked() && !self.is_unlocked(section) {
            bail!(
                "section '{section}' is locked (it holds this project's rules). \
                 Do NOT unlock it on your own: ask the user for explicit permission first, \
                 then run `wookie unlock {section}` (auto-relocks in 15 min) and retry."
            );
        }
        Ok(())
    }

    pub fn unlock(&self, section: &str, minutes: u64) -> Result<String> {
        let minutes = minutes.clamp(1, 24 * 60);
        let sections = self.sections();
        let Some(cfg) = sections.get(section) else {
            bail!(
                "unknown section '{section}'. Sections: {}",
                sections.keys().cloned().collect::<Vec<_>>().join(", ")
            );
        };
        if !cfg.is_locked() {
            return Ok(format!("Section '{section}' is not locked."));
        }
        // Serialize the marker transition with page writes and publications.
        // In particular, a page writer that observed an unlocked marker must
        // finish before relock can publish its locked marker, while writers
        // starting after unlock completes observe the new marker.
        let guard = self.acquire_mutation_guard()?;
        let now = chrono::Utc::now();
        let expiry = now + chrono::Duration::minutes(minutes as i64);
        self.ensure_gitignore_guarded(&guard)?;
        self.save_section_unlock(
            &guard,
            section,
            &SectionUnlockState {
                locked: false,
                expires_at: Some(expiry.to_rfc3339()),
            },
        )?;
        Ok(format!(
            "Unlocked section '{section}' for {minutes} min (relock early with `wookie lock {section}`)."
        ))
    }

    pub fn relock(&self, section: &str) -> Result<String> {
        if !self.sections().contains_key(section) {
            bail!("unknown section '{section}'");
        }
        let guard = self.acquire_mutation_guard()?;
        self.ensure_gitignore_guarded(&guard)?;
        self.save_section_unlock(
            &guard,
            section,
            &SectionUnlockState {
                locked: true,
                expires_at: None,
            },
        )?;
        Ok(format!("Locked section '{section}'."))
    }

    fn git(&self, args: &[&str]) {
        let _ = Command::new("git")
            .arg("-C")
            .arg(&self.dir)
            .args(args)
            .output();
    }

    pub fn init_git(&self) {
        self.git(&["init", "-q"]);
    }

    /// Serialize and commit only the named wiki-relative paths. Failures can
    /// either warn or fail the mutation according to `history` configuration.
    pub fn commit_paths(&self, msg: &str, paths: &[String]) -> Result<()> {
        if paths.is_empty() {
            bail!("wiki history commit requires at least one path");
        }
        if !self.auto_commit {
            return Ok(());
        }
        if let Err(error) = crate::history::commit_paths(&self.dir, msg, paths, &self.history) {
            if self.history.fail_on_commit_error {
                return Err(error);
            }
            eprintln!("warning: wiki history commit failed: {error:#}");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let sequence = ATOMIC_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wookie-wiki-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn wiki_fixture(label: &str) -> (PathBuf, Wiki) {
        let base = temp_dir(label);
        let home = base.join("home");
        let wiki_dir = home.join("test");
        fs::create_dir_all(wiki_dir.join("pages")).unwrap();
        fs::write(
            wiki_dir.join("wookie.toml"),
            "name = \"test\"\nproject_roots = []\nauto_commit = false\n\n[history]\nlock_timeout_ms = 50\n",
        )
        .unwrap();
        let wiki = open(&home, "test").unwrap();
        (base, wiki)
    }

    #[test]
    fn atomic_write_replaces_existing_file() {
        let dir = temp_dir("atomic");
        let path = dir.join("page.md");
        fs::write(&path, "old").unwrap();

        atomic_write(&path, "new contents").unwrap();

        assert_eq!(fs::read_to_string(&path).unwrap(), "new contents");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn page_save_rejects_a_rendered_image_above_the_canonical_read_limit() {
        let (base, wiki) = wiki_fixture("oversized-render");
        let mut page = Page::parse(
            "oversized",
            &"x".repeat(crate::snapshot::MAX_SNAPSHOT_PAGE_BYTES as usize),
        );
        let error = wiki
            .save_page_raw(&mut page, false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("page safety limit"), "{error}");
        assert!(!wiki.page_path("oversized").unwrap().exists());
        fs::remove_dir_all(base).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn atomic_rename_and_new_managed_directories_use_private_modes() {
        use std::os::unix::fs::PermissionsExt;

        let root = temp_dir("private-modes");
        let existing = root.join("existing");
        fs::create_dir(&existing).unwrap();
        fs::set_permissions(&existing, fs::Permissions::from_mode(0o755)).unwrap();

        let created = create_contained_dir_all(&root, Path::new("existing/private/child")).unwrap();
        let file = created.join("state.toml");
        atomic_write(&file, "private = true\n").unwrap();

        let mode = |path: &Path| fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&existing), 0o755, "existing directory was modified");
        assert_eq!(mode(&existing.join("private")), 0o700);
        assert_eq!(mode(&created), 0o700);
        // `atomic_write` publishes via rename on Unix; the final name must
        // inherit the temporary's private mode.
        assert_eq!(mode(&file), 0o600);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn contained_path_rejects_parent_components() {
        let dir = temp_dir("parent");
        let error = contained_path(&dir, Path::new("pages/../outside"))
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid path component"), "{error}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn wiki_configuration_is_bounded_and_rejects_terminal_controls() {
        let (base, wiki) = wiki_fixture("config-bounds");
        let path = wiki.dir.join("wookie.toml");
        let file = fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_len(MAX_PERSISTENT_CONFIG_BYTES as u64 + 1)
            .unwrap();
        drop(file);
        let error = open(base.join("home").as_path(), "test")
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("exceeds"), "{error}");

        let mut valid: WikiConfig = toml::from_str(
            "name = \"test\"\ndescription = \"Unicode ✓\"\nproject_roots = [\"/tmp/Project Folder/资料\"]\n",
        )
        .unwrap();
        valid.validate().unwrap();
        valid.description = "unsafe\u{1b}[31m".into();
        let error = valid.validate().unwrap_err().to_string();
        assert!(error.contains("control"), "{error}");
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn nonregular_and_invalid_utf8_wiki_configuration_is_rejected() {
        let base = temp_dir("nonregular-config");
        let home = base.join("home");
        let wiki_dir = home.join("test");
        fs::create_dir_all(wiki_dir.join("pages")).unwrap();
        fs::create_dir(wiki_dir.join("wookie.toml")).unwrap();
        let error = open(&home, "test").err().unwrap().to_string();
        assert!(error.contains("regular"), "{error}");

        fs::remove_dir(wiki_dir.join("wookie.toml")).unwrap();
        fs::write(wiki_dir.join("wookie.toml"), [0xff, 0xfe]).unwrap();
        let error = open(&home, "test").err().unwrap().to_string();
        assert!(error.contains("UTF-8"), "{error}");
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn oversized_section_unlock_marker_fails_closed() {
        let (base, wiki) = wiki_fixture("unlock-config-bound");
        let unlocks = wiki.dir.join(".unlocks");
        fs::create_dir(&unlocks).unwrap();
        let marker = unlocks.join("style.toml");
        let file = fs::File::create(marker).unwrap();
        file.set_len(MAX_PERSISTENT_CONFIG_BYTES as u64 + 1)
            .unwrap();
        drop(file);

        assert!(!wiki.is_unlocked("style"));
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn oversized_pages_and_ignore_control_files_are_rejected() {
        let (base, wiki) = wiki_fixture("bounded-page-and-ignore");
        let page = wiki.dir.join("pages/large.md");
        let file = fs::File::create(page).unwrap();
        file.set_len(snapshot::MAX_SNAPSHOT_PAGE_BYTES + 1).unwrap();
        drop(file);
        let error = format!("{:#}", wiki.load_page("large").unwrap_err());
        assert!(error.contains("safety limit"), "{error}");

        let ignore = wiki.dir.join(".gitignore");
        let file = fs::File::create(ignore).unwrap();
        file.set_len(MAX_PERSISTENT_CONFIG_BYTES as u64 + 1)
            .unwrap();
        drop(file);
        let error = wiki.ensure_gitignore().unwrap_err().to_string();
        assert!(error.contains("limit"), "{error}");
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn page_ids_reject_nonportable_windows_segments_and_excessive_lengths() {
        for id in [
            "con",
            "code/con.md",
            "prn.anything",
            "aux",
            "nul.txt",
            "com1",
            "com9.log",
            "lpt1",
            "lpt9.txt",
            "guides/trailing.",
        ] {
            let error = validate_id(id).unwrap_err().to_string();
            assert!(error.contains("not portable"), "{id}: {error}");
        }

        for id in ["com0", "com10", "lpt0", "lpt10", "context.txt"] {
            validate_id(id).unwrap();
        }

        let long_segment = "a".repeat(256);
        assert!(validate_id(&format!("code/{long_segment}")).is_err());
        let long_id = (0..400).map(|_| "abc").collect::<Vec<_>>().join("/");
        assert!(validate_id(&long_id).is_err());
    }

    #[test]
    fn unlock_is_serialized_by_the_shared_mutation_guard() {
        let (base, wiki) = wiki_fixture("unlock-guard");
        let marker = wiki.dir.join(".unlocks/style.toml");
        let guard = wiki.acquire_mutation_guard().unwrap();

        let error = wiki.unlock("style", 15).unwrap_err().to_string();
        assert!(
            error.contains("publication lock") || error.contains("not reentrant"),
            "{error}"
        );
        assert!(!marker.exists());
        assert!(!wiki.is_unlocked("style"));

        drop(guard);
        wiki.unlock("style", 15).unwrap();
        assert!(marker.is_file());
        assert!(wiki.is_unlocked("style"));
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn relock_is_serialized_by_the_shared_mutation_guard() {
        let (base, wiki) = wiki_fixture("relock-guard");
        wiki.unlock("style", 15).unwrap();
        let guard = wiki.acquire_mutation_guard().unwrap();

        let error = wiki.relock("style").unwrap_err().to_string();
        assert!(
            error.contains("publication lock") || error.contains("not reentrant"),
            "{error}"
        );
        assert!(wiki.is_unlocked("style"));

        drop(guard);
        wiki.relock("style").unwrap();
        assert!(!wiki.is_unlocked("style"));
        fs::remove_dir_all(base).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn page_operations_reject_symlinked_ancestors() {
        use std::os::unix::fs::symlink;

        let base = temp_dir("page-symlink");
        let home = base.join("home");
        let wiki_dir = home.join("test");
        let pages = wiki_dir.join("pages");
        let outside = base.join("outside");
        fs::create_dir_all(&pages).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(
            wiki_dir.join("wookie.toml"),
            "name = \"test\"\nproject_roots = []\nauto_commit = false\n",
        )
        .unwrap();
        fs::write(outside.join("secret.md"), "outside content").unwrap();
        symlink(&outside, pages.join("linked")).unwrap();
        let wiki = open(&home, "test").unwrap();

        let read_error = wiki.load_page("linked/secret").unwrap_err().to_string();
        assert!(read_error.contains("symlink"), "{read_error}");

        let mut page = Page::parse("linked/secret", "replacement");
        let write_error = wiki.save_page(&mut page, false).unwrap_err().to_string();
        assert!(write_error.contains("symlink"), "{write_error}");

        let delete_error = wiki.delete_page("linked/secret").unwrap_err().to_string();
        assert!(delete_error.contains("symlink"), "{delete_error}");
        assert_eq!(
            fs::read_to_string(outside.join("secret.md")).unwrap(),
            "outside content"
        );
        fs::remove_dir_all(base).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn open_rejects_symlinked_wiki_config() {
        use std::os::unix::fs::symlink;

        let base = temp_dir("config-symlink");
        let home = base.join("home");
        let wiki_dir = home.join("test");
        fs::create_dir_all(wiki_dir.join("pages")).unwrap();
        let outside = base.join("outside.toml");
        fs::write(&outside, "name = \"test\"\nproject_roots = []\n").unwrap();
        symlink(&outside, wiki_dir.join("wookie.toml")).unwrap();

        let error = open(&home, "test").err().unwrap().to_string();
        assert!(error.contains("symlink"), "{error}");
        fs::remove_dir_all(base).unwrap();
    }
}
