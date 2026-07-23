//! Minimal, project-local page protocols.
//!
//! A protocol is a single Markdown template beneath `<wiki>/protocols/`. Its
//! path, without the `.md` suffix, is its name. An optional TOML header may
//! provide discovery metadata and page defaults:
//!
//! ```text
//! +++
//! description = "Record an architectural decision"
//! section = "decisions"
//! tags = ["decision"]
//! +++
//! # {{title}}
//!
//! **{{title}}** records ...
//! ```
//!
//! Templates are deliberately not a plugin language. Only `{{id}}`,
//! `{{title}}`, and `{{date}}` are recognized, and expansion is single-pass.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, Metadata, OpenOptions};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const MAX_PROTOCOL_BYTES: u64 = 1024 * 1024;
const MAX_RENDERED_BYTES: usize = 2 * 1024 * 1024;
const MAX_TAGS: usize = 32;
const MAX_TAG_BYTES: usize = 128;
const MAX_DESCRIPTION_BYTES: usize = 1024;
const MAX_PROTOCOL_TREE_ENTRIES: usize = 100_000;
const MAX_PROTOCOL_FILE_COUNT: usize = 50_000;
const MAX_PROTOCOL_COUNT: usize = 10_000;
const MAX_PROTOCOL_CATALOG_BYTES: usize = 128 * 1024 * 1024;
const MAX_PROTOCOL_PATH_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
struct ProtocolCatalogLimits {
    tree_entries: usize,
    files: usize,
    protocols: usize,
    aggregate_bytes: usize,
    path_bytes: usize,
}

const PROTOCOL_CATALOG_LIMITS: ProtocolCatalogLimits = ProtocolCatalogLimits {
    tree_entries: MAX_PROTOCOL_TREE_ENTRIES,
    files: MAX_PROTOCOL_FILE_COUNT,
    protocols: MAX_PROTOCOL_COUNT,
    aggregate_bytes: MAX_PROTOCOL_CATALOG_BYTES,
    path_bytes: MAX_PROTOCOL_PATH_BYTES,
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProtocolFileIdentity {
    length: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(unix)]
    ctime: i64,
    #[cfg(unix)]
    ctime_nsec: i64,
    #[cfg(not(unix))]
    created: Option<SystemTime>,
}

fn protocol_file_identity(metadata: &Metadata, name: &str) -> Result<ProtocolFileIdentity> {
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("protocol '{name}' must be a regular file, not a symlink or directory");
    }
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt as _;
    Ok(ProtocolFileIdentity {
        length: metadata.len(),
        modified: metadata.modified().ok(),
        #[cfg(unix)]
        dev: metadata.dev(),
        #[cfg(unix)]
        ino: metadata.ino(),
        #[cfg(unix)]
        ctime: metadata.ctime(),
        #[cfg(unix)]
        ctime_nsec: metadata.ctime_nsec(),
        #[cfg(not(unix))]
        created: metadata.created().ok(),
    })
}

fn configure_protocol_no_follow(options: &mut OpenOptions) {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
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
        use std::os::unix::fs::OpenOptionsExt as _;
        const O_NOFOLLOW: i32 = 0x100;
        const O_NONBLOCK: i32 = 0x4;
        options.custom_flags(O_NOFOLLOW | O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt as _;
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

fn bounded_add(total: &mut usize, amount: usize, limit: usize, label: &str) -> Result<()> {
    *total = total
        .checked_add(amount)
        .with_context(|| format!("protocol catalog {label} overflow"))?;
    if *total > limit {
        bail!("protocol catalog exceeds immutable {label} limit of {limit}");
    }
    Ok(())
}

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

/// Optional metadata at the top of a protocol template.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProtocolHeader {
    /// One-line text shown by protocol discovery.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Default/required top-level page section. A bare requested id is
    /// prefixed with this section; an already-filed id must match it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
    /// Tags merged into the new page by the command layer.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

/// A loaded protocol and its Markdown body template.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Protocol {
    pub name: String,
    pub header: ProtocolHeader,
    pub template: String,
}

/// Compact discovery data returned by [`ProtocolStore::list`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProtocolSummary {
    pub name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

impl From<&Protocol> for ProtocolSummary {
    fn from(protocol: &Protocol) -> Self {
        Self {
            name: protocol.name.clone(),
            description: protocol.header.description.clone(),
            section: protocol.header.section.clone(),
            tags: protocol.header.tags.clone(),
        }
    }
}

/// Inputs accepted by the fixed protocol renderer.
#[derive(Debug, Clone, Copy)]
pub struct RenderInput<'a> {
    pub id: &'a str,
    pub title: Option<&'a str>,
    /// Defaults to the local calendar date returned by `page::today`.
    pub date: Option<&'a str>,
}

/// Fully resolved output. The command layer remains responsible for creating
/// frontmatter, enforcing section locks, and saving the page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RenderedProtocol {
    pub protocol: String,
    pub id: String,
    pub title: String,
    pub date: String,
    pub body: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

/// Filesystem-backed protocol discovery for one wiki.
#[derive(Debug, Clone)]
pub struct ProtocolStore {
    wiki_dir: PathBuf,
}

impl ProtocolStore {
    /// Create a store rooted at a real wiki directory. The `protocols/`
    /// directory itself is optional, so existing wikis need no migration.
    pub fn new(wiki_dir: impl AsRef<Path>) -> Result<Self> {
        let wiki_dir = wiki_dir.as_ref();
        let metadata = fs::symlink_metadata(wiki_dir)
            .with_context(|| format!("inspecting wiki directory {}", wiki_dir.display()))?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            bail!(
                "wiki directory {} must be a real directory, not a symlink",
                wiki_dir.display()
            );
        }
        Ok(Self {
            wiki_dir: wiki_dir.to_path_buf(),
        })
    }

    /// List all protocols in stable name order. Any symlink inside the
    /// managed protocol tree is rejected instead of followed or ignored.
    pub fn list(&self) -> Result<Vec<ProtocolSummary>> {
        self.list_with_limits(PROTOCOL_CATALOG_LIMITS)
    }

    fn list_with_limits(&self, limits: ProtocolCatalogLimits) -> Result<Vec<ProtocolSummary>> {
        let Some(root) = self.protocols_root()? else {
            return Ok(Vec::new());
        };

        let mut names = Vec::new();
        let mut tree_entries = 0_usize;
        let mut files = 0_usize;
        let mut protocols = 0_usize;
        let mut path_bytes = 0_usize;
        for entry in walkdir::WalkDir::new(&root).follow_links(false) {
            let entry = entry.with_context(|| format!("walking {}", root.display()))?;
            if entry.depth() == 0 {
                continue;
            }
            bounded_add(&mut tree_entries, 1, limits.tree_entries, "tree-entry")?;
            let relative = entry
                .path()
                .strip_prefix(&root)
                .with_context(|| format!("resolving protocol path {}", entry.path().display()))?;
            bounded_add(
                &mut path_bytes,
                relative.as_os_str().len(),
                limits.path_bytes,
                "path-byte",
            )?;
            if entry.file_type().is_symlink() {
                bail!(
                    "refusing protocol tree because it contains symlink {}",
                    entry.path().display()
                );
            }
            if !entry.file_type().is_dir() {
                bounded_add(&mut files, 1, limits.files, "file-count")?;
            }
            if !entry.file_type().is_file()
                || entry.path().extension().and_then(|value| value.to_str()) != Some("md")
            {
                continue;
            }
            bounded_add(&mut protocols, 1, limits.protocols, "protocol-count")?;
            let relative = relative
                .to_str()
                .with_context(|| format!("protocol path {} is not UTF-8", entry.path().display()))?
                .replace('\\', "/");
            let name = relative
                .strip_suffix(".md")
                .context("protocol template has no .md suffix")?
                .to_string();
            validate_name(&name)?;
            names.push(name);
        }
        names.sort();

        let mut aggregate_bytes = 0_usize;
        let mut summaries = Vec::with_capacity(names.len());
        for name in names {
            let (protocol, bytes) = self.load_with_inspection_hook(&name, |_| {})?;
            bounded_add(
                &mut aggregate_bytes,
                bytes,
                limits.aggregate_bytes,
                "aggregate-byte",
            )?;
            summaries.push((&protocol).into());
        }
        Ok(summaries)
    }

    /// Load and validate one protocol by its namespaced path.
    pub fn load(&self, name: &str) -> Result<Protocol> {
        self.load_with_inspection_hook(name, |_| {})
            .map(|(protocol, _)| protocol)
    }

    fn load_with_inspection_hook<F>(
        &self,
        name: &str,
        after_inspection: F,
    ) -> Result<(Protocol, usize)>
    where
        F: FnOnce(&Path),
    {
        validate_name(name)?;
        let relative = Path::new("protocols").join(format!("{name}.md"));
        let path = crate::wiki::contained_path(&self.wiki_dir, &relative)?;
        let before = fs::symlink_metadata(&path)
            .with_context(|| format!("no protocol '{name}' (looked at {})", path.display()))?;
        let before = protocol_file_identity(&before, name)?;
        if before.length > MAX_PROTOCOL_BYTES {
            bail!(
                "protocol '{}' is too large ({} bytes; maximum is {})",
                name,
                before.length,
                MAX_PROTOCOL_BYTES
            );
        }
        after_inspection(&path);

        let immediately_before_open = fs::symlink_metadata(&path).with_context(|| {
            format!(
                "protocol '{name}' changed before opening {}",
                path.display()
            )
        })?;
        let immediately_before_open = protocol_file_identity(&immediately_before_open, name)?;
        if immediately_before_open != before {
            bail!("protocol '{name}' changed while it was being opened");
        }

        let mut options = OpenOptions::new();
        options.read(true);
        configure_protocol_no_follow(&mut options);
        let mut file = options
            .open(&path)
            .with_context(|| format!("opening protocol '{}' at {}", name, path.display()))?;
        let opened = protocol_file_identity(
            &file
                .metadata()
                .with_context(|| format!("inspecting opened protocol '{name}'"))?,
            name,
        )?;
        let path_after_open = protocol_file_identity(
            &fs::symlink_metadata(&path)
                .with_context(|| format!("rechecking protocol '{name}' after opening"))?,
            name,
        )?;
        if opened != before || path_after_open != opened {
            bail!("protocol '{name}' changed while it was being opened");
        }

        let mut bytes = Vec::with_capacity(usize::try_from(opened.length).unwrap_or_default());
        (&mut file)
            .take(MAX_PROTOCOL_BYTES + 1)
            .read_to_end(&mut bytes)
            .with_context(|| format!("reading protocol '{}' at {}", name, path.display()))?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_PROTOCOL_BYTES {
            bail!(
                "protocol '{}' is too large (maximum is {} bytes)",
                name,
                MAX_PROTOCOL_BYTES
            );
        }
        let opened_after_read = protocol_file_identity(
            &file
                .metadata()
                .with_context(|| format!("rechecking opened protocol '{name}'"))?,
            name,
        )?;
        let path_after_read = protocol_file_identity(
            &fs::symlink_metadata(&path)
                .with_context(|| format!("rechecking protocol '{name}' after reading"))?,
            name,
        )?;
        if opened_after_read != opened
            || path_after_read != opened
            || opened.length != u64::try_from(bytes.len()).unwrap_or(u64::MAX)
        {
            bail!("protocol '{name}' changed while it was being read");
        }
        // Revalidate every managed path component after the read as well as
        // the opened file identity above. This turns a concurrent directory
        // replacement into an explicit failure instead of trusting it.
        crate::wiki::contained_path(&self.wiki_dir, &relative)?;
        let raw = String::from_utf8(bytes)
            .with_context(|| format!("protocol '{name}' is not valid UTF-8"))?;
        let raw_bytes = raw.len();

        Ok((parse(name, &raw)?, raw_bytes))
    }

    /// Alias for `load`, useful for command handlers implementing
    /// `wookie protocol show`.
    pub fn show(&self, name: &str) -> Result<Protocol> {
        self.load(name)
    }

    /// Load and render one protocol.
    pub fn render(&self, name: &str, input: RenderInput<'_>) -> Result<RenderedProtocol> {
        self.load(name)?.render(input)
    }

    fn protocols_root(&self) -> Result<Option<PathBuf>> {
        let root = crate::wiki::contained_path(&self.wiki_dir, Path::new("protocols"))?;
        match fs::symlink_metadata(&root) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => bail!(
                "protocol root {} must be a real directory, not a symlink",
                root.display()
            ),
            Ok(_) => Ok(Some(root)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("inspecting {}", root.display())),
        }
    }
}

impl Protocol {
    /// Resolve section defaults and substitute the three fixed variables.
    pub fn render(&self, input: RenderInput<'_>) -> Result<RenderedProtocol> {
        let id = resolve_id(input.id, self.header.section.as_deref())?;
        let title = input
            .title
            .map(str::to_string)
            .unwrap_or_else(|| crate::page::humanize(&id));
        let date = input
            .date
            .map(str::to_string)
            .unwrap_or_else(crate::page::today);
        let body = interpolate(&self.name, &self.template, &id, &title, &date)?;
        if body.len() > MAX_RENDERED_BYTES {
            bail!(
                "protocol '{}' rendered more than {} bytes",
                self.name,
                MAX_RENDERED_BYTES
            );
        }
        Ok(RenderedProtocol {
            protocol: self.name.clone(),
            id,
            title,
            date,
            body,
            section: self.header.section.clone(),
            tags: self.header.tags.clone(),
        })
    }
}

/// Convenience API for command/MCP integration without retaining a store.
pub fn list(wiki_dir: &Path) -> Result<Vec<ProtocolSummary>> {
    ProtocolStore::new(wiki_dir)?.list()
}

/// Convenience API for `protocol show`.
pub fn show(wiki_dir: &Path, name: &str) -> Result<Protocol> {
    ProtocolStore::new(wiki_dir)?.show(name)
}

/// Parse and validate a protocol without touching the filesystem. Mutation
/// commands can call this before atomically publishing a template.
pub fn parse(name: &str, raw: &str) -> Result<Protocol> {
    validate_name(name)?;
    if raw.len() as u64 > MAX_PROTOCOL_BYTES {
        bail!(
            "protocol '{name}' is too large ({} bytes; maximum is {MAX_PROTOCOL_BYTES})",
            raw.len()
        );
    }
    let (header, template) = parse_template(name, raw)?;
    validate_header(name, &header)?;
    validate_placeholders(name, &template)?;
    Ok(Protocol {
        name: name.to_string(),
        header,
        template,
    })
}

/// Convenience API for page creation.
pub fn render(wiki_dir: &Path, name: &str, input: RenderInput<'_>) -> Result<RenderedProtocol> {
    ProtocolStore::new(wiki_dir)?.render(name, input)
}

/// Protocol names are paths, but intentionally use a stricter alphabet than
/// page ids so `.md` mapping is unambiguous.
pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 1_024 || name.starts_with('/') || name.ends_with('/') {
        bail!("invalid protocol name '{name}'");
    }
    for segment in name.split('/') {
        if segment.is_empty()
            || crate::wiki::validate_portable_segment(segment).is_err()
            || !segment
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '-' | '_'))
        {
            bail!(
                "invalid protocol name '{name}' — use lowercase letters, digits, '-' and '_' in each path segment"
            );
        }
    }
    Ok(())
}

fn parse_template(name: &str, raw: &str) -> Result<(ProtocolHeader, String)> {
    // Normalize line endings once so delimiter matching and rendered output
    // are stable across machines.
    let normalized = raw.replace("\r\n", "\n");
    let Some(rest) = normalized.strip_prefix("+++\n") else {
        return Ok((ProtocolHeader::default(), normalized));
    };

    let (header_raw, template) = if let Some(template) = rest.strip_prefix("+++\n") {
        ("", template.to_string())
    } else if rest == "+++" {
        ("", String::new())
    } else if let Some(end) = rest.find("\n+++\n") {
        (&rest[..end], rest[end + "\n+++\n".len()..].to_string())
    } else if let Some(header) = rest.strip_suffix("\n+++") {
        (header, String::new())
    } else {
        bail!("protocol '{name}' has an opening +++ header but no closing +++ line");
    };
    let header: ProtocolHeader = toml::from_str(header_raw)
        .with_context(|| format!("parsing TOML header for protocol '{name}'"))?;
    Ok((header, template))
}

fn validate_header(name: &str, header: &ProtocolHeader) -> Result<()> {
    if header.description.len() > MAX_DESCRIPTION_BYTES
        || contains_terminal_control(&header.description)
    {
        bail!(
            "protocol '{name}' description must be one short line without control or terminal-direction characters"
        );
    }
    if let Some(section) = &header.section {
        validate_name(section)?;
        if section.contains('/') {
            bail!("protocol '{name}' section must be one path segment");
        }
    }
    if header.tags.len() > MAX_TAGS {
        bail!("protocol '{name}' has too many tags (maximum is {MAX_TAGS})");
    }
    for tag in &header.tags {
        if tag.trim().is_empty()
            || tag != tag.trim()
            || contains_terminal_control(tag)
            || tag.len() > MAX_TAG_BYTES
        {
            bail!(
                "protocol '{name}' contains an invalid tag; tags must not contain control or terminal-direction characters"
            );
        }
    }
    Ok(())
}

fn resolve_id(requested: &str, section: Option<&str>) -> Result<String> {
    crate::wiki::validate_id(requested)?;
    let Some(section) = section else {
        return Ok(requested.to_string());
    };
    let id = if requested.contains('/') {
        if requested.split('/').next() != Some(section) {
            bail!("page id '{requested}' does not belong to protocol section '{section}'");
        }
        requested.to_string()
    } else {
        format!("{section}/{requested}")
    };
    crate::wiki::validate_id(&id)?;
    Ok(id)
}

fn validate_placeholders(name: &str, template: &str) -> Result<()> {
    interpolate(name, template, "", "", "").map(|_| ())
}

/// Single-pass expansion prevents values containing `{{...}}` from being
/// interpreted as more template syntax.
fn interpolate(name: &str, template: &str, id: &str, title: &str, date: &str) -> Result<String> {
    let mut output = String::with_capacity(template.len());
    let mut remaining = template;
    loop {
        let Some(open) = remaining.find("{{") else {
            if remaining.contains("}}") {
                bail!("protocol '{name}' contains an unmatched }} delimiter");
            }
            output.push_str(remaining);
            break;
        };
        let before = &remaining[..open];
        if before.contains("}}") {
            bail!("protocol '{name}' contains an unmatched }} delimiter");
        }
        output.push_str(before);
        let after_open = &remaining[open + 2..];
        let close = after_open
            .find("}}")
            .with_context(|| format!("protocol '{name}' contains an unclosed {{{{ delimiter"))?;
        let variable = &after_open[..close];
        let value = match variable {
            "id" => id,
            "title" => title,
            "date" => date,
            _ => bail!(
                "protocol '{name}' uses unsupported variable '{{{{{variable}}}}}'; allowed variables are id, title, date"
            ),
        };
        output.push_str(value);
        remaining = &after_open[close + 2..];
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(label: &str) -> Self {
            let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "wookie-protocol-test-{label}-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_protocol(root: &Path, name: &str, content: &str) {
        let path = root.join("protocols").join(format!("{name}.md"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    #[test]
    fn lists_namespaces_in_stable_order() {
        let root = TempDir::new("list");
        write_protocol(&root.0, "guides/runbook", "Run {{title}}.");
        write_protocol(&root.0, "architecture/decision", "Decide {{id}}.");

        let listed = list(&root.0).unwrap();
        assert_eq!(
            listed
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            vec!["architecture/decision", "guides/runbook"]
        );
    }

    #[test]
    fn catalog_limits_fail_without_returning_a_partial_list() {
        let root = TempDir::new("catalog-limits");
        write_protocol(&root.0, "one", "One.");
        write_protocol(&root.0, "two", "Two.");
        let store = ProtocolStore::new(&root.0).unwrap();

        let over_count = store
            .list_with_limits(ProtocolCatalogLimits {
                protocols: 1,
                ..PROTOCOL_CATALOG_LIMITS
            })
            .unwrap_err()
            .to_string();
        assert!(over_count.contains("protocol-count limit"), "{over_count}");

        let over_aggregate = store
            .list_with_limits(ProtocolCatalogLimits {
                aggregate_bytes: 7,
                ..PROTOCOL_CATALOG_LIMITS
            })
            .unwrap_err()
            .to_string();
        assert!(
            over_aggregate.contains("aggregate-byte limit"),
            "{over_aggregate}"
        );

        fs::write(root.0.join("protocols/ignored.txt"), "not a protocol").unwrap();
        let over_files = store
            .list_with_limits(ProtocolCatalogLimits {
                files: 2,
                ..PROTOCOL_CATALOG_LIMITS
            })
            .unwrap_err()
            .to_string();
        assert!(over_files.contains("file-count limit"), "{over_files}");

        let over_paths = store
            .list_with_limits(ProtocolCatalogLimits {
                path_bytes: 1,
                ..PROTOCOL_CATALOG_LIMITS
            })
            .unwrap_err()
            .to_string();
        assert!(over_paths.contains("path-byte limit"), "{over_paths}");
    }

    #[test]
    fn header_defaults_and_fixed_variables_render() {
        let root = TempDir::new("render");
        write_protocol(
            &root.0,
            "architecture/decision",
            "+++\ndescription = \"Record a choice\"\nsection = \"decisions\"\ntags = [\"decision\"]\n+++\n# {{title}}\n\nID {{id}} on {{date}}.",
        );

        let rendered = render(
            &root.0,
            "architecture/decision",
            RenderInput {
                id: "cache-policy",
                title: None,
                date: Some("2026-07-22"),
            },
        )
        .unwrap();
        assert_eq!(rendered.id, "decisions/cache-policy");
        assert_eq!(rendered.title, "Cache Policy");
        assert_eq!(rendered.tags, vec!["decision"]);
        assert_eq!(
            rendered.body,
            "# Cache Policy\n\nID decisions/cache-policy on 2026-07-22."
        );
    }

    #[test]
    fn empty_header_is_valid() {
        let parsed = parse("plain", "+++\n+++\nHello {{title}}").unwrap();
        assert_eq!(parsed.header, ProtocolHeader::default());
        assert_eq!(parsed.template, "Hello {{title}}");
    }

    #[test]
    fn visible_header_fields_reject_controls_and_bidi_but_body_remains_markdown() {
        for escaped in [r"\u001b", r"\t", r"\u202e"] {
            let description =
                format!("+++\ndescription = \"unsafe{escaped}text\"\n+++\nSafe body.");
            let error = parse("unsafe-description", &description)
                .unwrap_err()
                .to_string();
            assert!(error.contains("description"), "{escaped}: {error}");

            let tag = format!("+++\ntags = [\"unsafe{escaped}tag\"]\n+++\nSafe body.");
            let error = parse("unsafe-tag", &tag).unwrap_err().to_string();
            assert!(error.contains("invalid tag"), "{escaped}: {error}");
        }

        let body = "Markdown body keeps\ttabs, \u{1b} escapes, and \u{202e} direction marks.";
        assert_eq!(parse("body-controls", body).unwrap().template, body);
    }

    #[test]
    fn interpolation_is_not_recursive() {
        let protocol = Protocol {
            name: "test".into(),
            header: ProtocolHeader::default(),
            template: "Title: {{title}}".into(),
        };
        let rendered = protocol
            .render(RenderInput {
                id: "page",
                title: Some("literal {{date}}"),
                date: Some("2026-07-22"),
            })
            .unwrap();
        assert_eq!(rendered.body, "Title: literal {{date}}");
    }

    #[test]
    fn unknown_and_unclosed_variables_are_rejected_on_load() {
        let root = TempDir::new("variables");
        write_protocol(&root.0, "unknown", "{{owner}}");
        write_protocol(&root.0, "unclosed", "{{title");
        assert!(ProtocolStore::new(&root.0)
            .unwrap()
            .load("unknown")
            .unwrap_err()
            .to_string()
            .contains("unsupported variable"));
        assert!(ProtocolStore::new(&root.0)
            .unwrap()
            .load("unclosed")
            .unwrap_err()
            .to_string()
            .contains("unclosed"));
    }

    #[test]
    fn section_rejects_misfiled_page() {
        let protocol = Protocol {
            name: "decision".into(),
            header: ProtocolHeader {
                section: Some("decisions".into()),
                ..ProtocolHeader::default()
            },
            template: String::new(),
        };
        let error = protocol
            .render(RenderInput {
                id: "guides/cache-policy",
                title: None,
                date: None,
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("does not belong"), "{error}");
    }

    #[test]
    fn traversal_names_are_rejected() {
        let root = TempDir::new("traversal");
        let error = ProtocolStore::new(&root.0)
            .unwrap()
            .load("../outside")
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid protocol name"), "{error}");
    }

    #[test]
    fn nonportable_and_excessive_protocol_names_are_rejected() {
        for name in ["con", "review/prn", "com1", "nested/lpt9"] {
            let error = validate_name(name).unwrap_err().to_string();
            assert!(error.contains("invalid protocol name"), "{name}: {error}");
        }
        assert!(validate_name(&format!("guides/{}", "a".repeat(256))).is_err());
        assert!(validate_name(&(0..400).map(|_| "abc").collect::<Vec<_>>().join("/")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_protocol_is_rejected() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new("symlink");
        let outside = root.0.join("outside.md");
        fs::write(&outside, "outside").unwrap();
        fs::create_dir(root.0.join("protocols")).unwrap();
        symlink(&outside, root.0.join("protocols/linked.md")).unwrap();

        let error = ProtocolStore::new(&root.0)
            .unwrap()
            .load("linked")
            .unwrap_err()
            .to_string();
        assert!(error.contains("symlink"), "{error}");
        let list_error = list(&root.0).unwrap_err().to_string();
        assert!(list_error.contains("symlink"), "{list_error}");
    }

    #[cfg(unix)]
    #[test]
    fn protocol_replacement_between_inspection_and_open_is_rejected() {
        use std::os::unix::fs::symlink;

        let root = TempDir::new("replacement");
        write_protocol(&root.0, "regular", "Original.");
        let replacement = root.0.join("replacement.md");
        fs::write(&replacement, "Replacement.").unwrap();
        let store = ProtocolStore::new(&root.0).unwrap();
        let regular_error = store
            .load_with_inspection_hook("regular", |path| {
                fs::rename(&replacement, path).unwrap();
            })
            .unwrap_err()
            .to_string();
        assert!(regular_error.contains("changed"), "{regular_error}");

        write_protocol(&root.0, "linked", "Original.");
        let outside = root.0.join("outside.md");
        fs::write(&outside, "Outside.").unwrap();
        let symlink_error = store
            .load_with_inspection_hook("linked", |path| {
                fs::remove_file(path).unwrap();
                symlink(&outside, path).unwrap();
            })
            .unwrap_err()
            .to_string();
        assert!(symlink_error.contains("symlink"), "{symlink_error}");
    }
}
