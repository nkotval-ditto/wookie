//! Disposable incremental index for deterministic retrieval.
//!
//! The index is deliberately a single ignored JSON file, not a service or a
//! database. It caches parsed pages behind strong file signatures and exact
//! raw SHA-256 leaves. Every failure falls back to canonical page reads; cache
//! persistence is never required for a read-only query to succeed.

use crate::page::{Page, PinLevel};
use crate::snapshot::{self, RawPageFingerprint};
use crate::wiki::{self, Wiki};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const CACHE_SCHEMA: &str = "wookie.retrieval-index/v1";
const CACHE_DIR: &str = ".cache";
const CACHE_FILE: &str = "retrieval-v1.json";
const MAX_CACHE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_PAGE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_CATALOG_BYTES: u64 = 256 * 1024 * 1024;
const MAX_CATALOG_PAGES: usize = wiki::MAX_PAGE_CATALOG_ENTRIES;
const MAX_BUILD_ATTEMPTS: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheState {
    Hit,
    Updated,
    Bypassed,
}

impl std::fmt::Display for CacheState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Hit => "hit",
            Self::Updated => "updated",
            Self::Bypassed => "bypassed",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CacheTelemetry {
    pub state: CacheState,
    pub pages_reused: usize,
    pub pages_refreshed: usize,
    pub pages_deleted: usize,
    /// Pin projections reused without reopening unchanged page bodies.
    pub pin_memberships_reused: usize,
    /// Strict metadata generations enumerated for this coherent snapshot.
    pub catalog_scans: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Catalog {
    pub pages: Vec<Page>,
    pub raw_sha256: BTreeMap<String, String>,
    /// Pin membership from fresh canonical parses or integrity-checked cached
    /// projections bound to the current strong file generation.
    pub pin_levels: BTreeMap<String, crate::page::PinLevel>,
    pub content_hash: String,
    pub cache: CacheTelemetry,
    generation: CatalogGeneration,
}

#[derive(Debug, Clone)]
struct CatalogGeneration(Vec<CatalogFile>);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Timestamp {
    seconds: i64,
    nanos: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeIdentity {
    platform: String,
    values: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StatSignature {
    length: u64,
    modified: Timestamp,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    native: Option<NativeIdentity>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CachedPage {
    id: String,
    signature: StatSignature,
    raw_sha256: String,
    parsed_sha256: String,
    page: Page,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CacheFile {
    schema: String,
    wiki_slug: String,
    content_hash: String,
    pages: Vec<CachedPage>,
}

#[derive(Debug, Clone)]
struct CatalogFile {
    id: String,
    path: PathBuf,
    signature: StatSignature,
}

fn timestamp(value: SystemTime) -> Result<Timestamp> {
    match value.duration_since(UNIX_EPOCH) {
        Ok(duration) => Ok(Timestamp {
            seconds: i64::try_from(duration.as_secs())
                .context("page modification time is out of range")?,
            nanos: duration.subsec_nanos(),
        }),
        Err(error) => {
            let duration = error.duration();
            let seconds = i64::try_from(duration.as_secs())
                .context("page modification time is out of range")?;
            if duration.subsec_nanos() == 0 {
                Ok(Timestamp {
                    seconds: seconds.saturating_neg(),
                    nanos: 0,
                })
            } else {
                Ok(Timestamp {
                    seconds: seconds.saturating_neg().saturating_sub(1),
                    nanos: 1_000_000_000 - duration.subsec_nanos(),
                })
            }
        }
    }
}

fn signature(metadata: &fs::Metadata) -> Result<StatSignature> {
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("retrieval catalog entries must be regular files");
    }
    #[cfg(unix)]
    let native = {
        use std::os::unix::fs::MetadataExt as _;
        Some(NativeIdentity {
            platform: "unix".to_string(),
            values: vec![
                metadata.dev(),
                metadata.ino(),
                u64::from_ne_bytes(metadata.ctime().to_ne_bytes()),
                u64::from_ne_bytes(metadata.ctime_nsec().to_ne_bytes()),
            ],
        })
    };
    #[cfg(windows)]
    let native = {
        use std::os::windows::fs::MetadataExt as _;
        Some(NativeIdentity {
            platform: "windows".to_string(),
            values: vec![
                metadata.creation_time(),
                metadata.last_write_time(),
                u64::from(metadata.file_attributes()),
                metadata.file_size(),
            ],
        })
    };
    #[cfg(not(any(unix, windows)))]
    let native = None;

    Ok(StatSignature {
        length: metadata.len(),
        modified: timestamp(metadata.modified()?)?,
        native,
    })
}

fn catalog_files(wiki: &Wiki) -> Result<Vec<CatalogFile>> {
    let mut files = Vec::new();
    let mut catalog_bytes = 0_u64;
    for (id, path) in wiki.page_files_strict()? {
        let metadata = fs::symlink_metadata(&path)?;
        if metadata.len() > MAX_PAGE_BYTES {
            bail!("page '{id}' exceeds the {MAX_PAGE_BYTES}-byte retrieval safety limit");
        }
        catalog_bytes = catalog_bytes
            .checked_add(metadata.len())
            .context("retrieval catalog size overflow")?;
        if catalog_bytes > MAX_CATALOG_BYTES {
            bail!("retrieval catalog exceeds the {MAX_CATALOG_BYTES}-byte safety limit");
        }
        files.push(CatalogFile {
            id,
            path,
            signature: signature(&metadata)?,
        });
        if files.len() > MAX_CATALOG_PAGES {
            bail!("retrieval catalog exceeds the {MAX_CATALOG_PAGES}-page safety limit");
        }
    }
    files.sort_by(|left, right| left.id.cmp(&right.id));
    if files
        .windows(2)
        .any(|pair| pair[0].id.as_str() == pair[1].id.as_str())
    {
        bail!("retrieval catalog contains duplicate page ids");
    }
    Ok(files)
}

fn cache_path(wiki: &Wiki) -> Result<PathBuf> {
    wiki.contained_path(Path::new(CACHE_DIR).join(CACHE_FILE).as_path())
}

fn read_bounded_regular(path: &Path) -> Result<Vec<u8>> {
    let file = File::open(path).with_context(|| format!("opening cache {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("inspecting cache {}", path.display()))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        bail!("retrieval cache must be a regular file");
    }
    if metadata.len() > MAX_CACHE_BYTES {
        bail!("retrieval cache exceeds the {MAX_CACHE_BYTES}-byte safety limit");
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or_default());
    file.take(MAX_CACHE_BYTES + 1).read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_CACHE_BYTES {
        bail!("retrieval cache exceeds the {MAX_CACHE_BYTES}-byte safety limit");
    }
    Ok(bytes)
}

fn load_cache(wiki: &Wiki) -> Result<Option<CacheFile>> {
    let path = match cache_path(wiki) {
        Ok(path) => path,
        Err(_) => return Ok(None),
    };
    let metadata = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Ok(None),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Ok(None);
    }
    let bytes = match read_bounded_regular(&path) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };
    let cache: CacheFile = match serde_json::from_slice(&bytes) {
        Ok(cache) => cache,
        Err(_) => return Ok(None),
    };
    if cache.schema != CACHE_SCHEMA
        || cache.wiki_slug != wiki.slug
        || cache.pages.len() > MAX_CATALOG_PAGES
        || cache.pages.windows(2).any(|pair| pair[0].id >= pair[1].id)
    {
        return Ok(None);
    }
    let mut seen = BTreeSet::new();
    let mut fingerprints = Vec::with_capacity(cache.pages.len());
    for entry in &cache.pages {
        if wiki::validate_id(&entry.id).is_err()
            || entry.page.id != entry.id
            || !seen.insert(entry.id.as_str())
            || parsed_page_sha256(&entry.raw_sha256, &entry.page)
                .ok()
                .as_deref()
                != Some(entry.parsed_sha256.as_str())
        {
            return Ok(None);
        }
        fingerprints.push(RawPageFingerprint {
            id: entry.id.clone(),
            raw_sha256: entry.raw_sha256.clone(),
        });
    }
    if snapshot::catalog_content_hash(&fingerprints)
        .ok()
        .as_deref()
        != Some(cache.content_hash.as_str())
    {
        return Ok(None);
    }
    Ok(Some(cache))
}

fn hash_field(hash: &mut Sha256, bytes: &[u8]) {
    hash.update(u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_be_bytes());
    hash.update(bytes);
}

fn parsed_page_sha256(raw_sha256: &str, page: &Page) -> Result<String> {
    let encoded = serde_json::to_vec(page)?;
    let mut hash = Sha256::new();
    hash_field(&mut hash, b"wookie.retrieval-parsed-page/v1");
    hash_field(&mut hash, raw_sha256.as_bytes());
    hash_field(&mut hash, &encoded);
    Ok(format!("sha256:{:x}", hash.finalize()))
}

fn read_page(file: &CatalogFile) -> Result<CachedPage> {
    let opened = File::open(&file.path)
        .with_context(|| format!("opening retrieval page {}", file.path.display()))?;
    let opened_before = signature(&opened.metadata()?)?;
    if opened_before != file.signature || opened_before.length > MAX_PAGE_BYTES {
        bail!("page '{}' changed while building retrieval index", file.id);
    }
    let mut raw = Vec::with_capacity(usize::try_from(opened_before.length).unwrap_or_default());
    opened.take(MAX_PAGE_BYTES + 1).read_to_end(&mut raw)?;
    if u64::try_from(raw.len()).unwrap_or(u64::MAX) > MAX_PAGE_BYTES {
        bail!(
            "page '{}' exceeds the {MAX_PAGE_BYTES}-byte retrieval safety limit",
            file.id
        );
    }
    let after = signature(&fs::symlink_metadata(&file.path)?)?;
    if after != opened_before {
        bail!("page '{}' changed while building retrieval index", file.id);
    }
    let raw_text = std::str::from_utf8(&raw)
        .with_context(|| format!("page '{}' is not valid UTF-8", file.id))?;
    let raw_sha256 = snapshot::raw_page_sha256(&raw);
    let page = Page::parse(&file.id, raw_text);
    Ok(CachedPage {
        id: file.id.clone(),
        signature: file.signature.clone(),
        parsed_sha256: parsed_page_sha256(&raw_sha256, &page)?,
        raw_sha256,
        page,
    })
}

#[derive(Default)]
struct AssembleResult {
    pages: Vec<CachedPage>,
    pin_levels: BTreeMap<String, PinLevel>,
    reused: usize,
    refreshed: usize,
    deleted: usize,
    pin_memberships_reused: usize,
}

fn assemble(files: &[CatalogFile], cached: Option<&CacheFile>) -> Result<AssembleResult> {
    let cached = cached
        .map(|cache| {
            cache
                .pages
                .iter()
                .map(|entry| (entry.id.as_str(), entry))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let current = files
        .iter()
        .map(|file| file.id.as_str())
        .collect::<BTreeSet<_>>();
    let mut result = AssembleResult {
        pages: Vec::with_capacity(files.len()),
        deleted: cached.keys().filter(|id| !current.contains(**id)).count(),
        ..AssembleResult::default()
    };
    for file in files {
        if let Some(entry) = cached
            .get(file.id.as_str())
            .filter(|entry| entry.signature.native.is_some() && entry.signature == file.signature)
        {
            if let Some(level) = entry.page.pin_level() {
                result.pin_levels.insert(file.id.clone(), level);
            }
            result.pages.push((*entry).clone());
            result.reused += 1;
            result.pin_memberships_reused += 1;
        } else {
            let refreshed = read_page(file)?;
            if let Some(level) = refreshed.page.pin_level() {
                result.pin_levels.insert(file.id.clone(), level);
            }
            result.pages.push(refreshed);
            result.refreshed += 1;
        }
    }
    Ok(result)
}

fn persist(wiki: &Wiki, cache: &CacheFile) -> Result<bool> {
    // Cache writes are optional derived state. Only publish while the shared
    // mutation guard is immediately available; never make retrieval wait for
    // a publication or fail because a crash journal needs recovery.
    let Some(_guard) = wiki.try_acquire_mutation_guard() else {
        return Ok(false);
    };
    let ignored = wiki
        .contained_path(Path::new(".gitignore"))
        .ok()
        .and_then(|path| snapshot::read_page_prefix(&path, 64 * 1024).ok())
        .filter(|contents| contents.len() <= 64 * 1024)
        .and_then(|contents| String::from_utf8(contents).ok())
        .is_some_and(|contents| contents.lines().any(|line| line.trim() == ".cache/"));
    if !ignored {
        // Existing wikis are upgraded by ordinary Wookie maintenance that
        // owns `.gitignore`; a read-only query must not dirty history merely
        // to enable an optimization.
        return Ok(false);
    }
    if wiki::create_contained_dir_all(&wiki.dir, Path::new(CACHE_DIR)).is_err() {
        return Ok(false);
    }
    let path = match wiki.contained_path(Path::new(CACHE_DIR).join(CACHE_FILE).as_path()) {
        Ok(path) => path,
        Err(_) => return Ok(false),
    };
    let encoded = serde_json::to_vec(cache)?;
    if u64::try_from(encoded.len()).unwrap_or(u64::MAX) > MAX_CACHE_BYTES {
        return Ok(false);
    }
    match wiki::atomic_write_with_permissions(
        &path,
        encoded,
        Some(wiki::AtomicWritePermissions {
            readonly: false,
            unix_mode: cfg!(unix).then_some(0o600),
        }),
    ) {
        Ok(()) => Ok(true),
        Err(_) => Ok(false),
    }
}

fn same_generation(left: &[CatalogFile], right: &[CatalogFile]) -> bool {
    left.iter()
        .map(|file| (&file.id, &file.signature))
        .eq(right.iter().map(|file| (&file.id, &file.signature)))
}

/// Fail closed if any page was added, removed, replaced, or changed after the
/// coherent retrieval generation was captured. This is metadata-only: prime
/// already rereads the small selected pin set through canonical handles.
pub fn verify_generation(wiki: &Wiki, catalog: &Catalog) -> Result<()> {
    let current = catalog_files(wiki)?;
    if !same_generation(&catalog.generation.0, &current) {
        bail!("wiki catalog changed while priming; retry the command");
    }
    Ok(())
}

/// Load the current page catalog, reusing parsed entries whose strong stat
/// signatures are unchanged. Canonical reads always win over cache state.
pub fn load(wiki: &Wiki) -> Result<Catalog> {
    let cached = load_cache(wiki).unwrap_or(None);
    for _ in 0..MAX_BUILD_ATTEMPTS {
        let before = catalog_files(wiki)?;
        let assembled = assemble(&before, cached.as_ref())?;
        let after = catalog_files(wiki)?;
        if !same_generation(&before, &after) {
            continue;
        }
        let fingerprints = assembled
            .pages
            .iter()
            .map(|entry| RawPageFingerprint {
                id: entry.id.clone(),
                raw_sha256: entry.raw_sha256.clone(),
            })
            .collect::<Vec<_>>();
        let content_hash = snapshot::catalog_content_hash(&fingerprints)?;
        let raw_sha256 = fingerprints
            .iter()
            .map(|page| (page.id.clone(), page.raw_sha256.clone()))
            .collect();
        let cache = CacheFile {
            schema: CACHE_SCHEMA.to_string(),
            wiki_slug: wiki.slug.clone(),
            content_hash: content_hash.clone(),
            pages: assembled.pages.clone(),
        };
        let needs_update = assembled.refreshed > 0
            || assembled.deleted > 0
            || cached
                .as_ref()
                .is_none_or(|previous| previous.content_hash != content_hash);
        let persisted = if needs_update {
            persist(wiki, &cache).unwrap_or(false)
        } else {
            true
        };
        return Ok(Catalog {
            pages: assembled
                .pages
                .into_iter()
                .map(|entry| entry.page)
                .collect(),
            raw_sha256,
            pin_levels: assembled.pin_levels,
            content_hash,
            cache: CacheTelemetry {
                state: if needs_update && persisted {
                    CacheState::Updated
                } else if !needs_update {
                    CacheState::Hit
                } else {
                    CacheState::Bypassed
                },
                pages_reused: assembled.reused,
                pages_refreshed: assembled.refreshed,
                pages_deleted: assembled.deleted,
                pin_memberships_reused: assembled.pin_memberships_reused,
                catalog_scans: 2,
                detail: (!persisted).then(|| {
                    "cache update unavailable; retrieval used fresh in-memory pages".to_string()
                }),
            },
            generation: CatalogGeneration(after),
        });
    }

    bail!("wiki catalog changed repeatedly while indexing; retry after concurrent writes finish")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::{Frontmatter, PinLevel};

    fn fixture() -> (tempfile_shim::TempDir, Wiki) {
        let temp = tempfile_shim::TempDir::new();
        let root = temp.path().join("cache-wiki");
        fs::create_dir_all(root.join("pages")).unwrap();
        fs::write(
            root.join("wookie.toml"),
            "name = \"cache-wiki\"\ndescription = \"cache test\"\n",
        )
        .unwrap();
        fs::write(root.join(".gitignore"), ".cache/\n").unwrap();
        let wiki = crate::wiki::open(temp.path(), "cache-wiki").unwrap();
        (temp, wiki)
    }

    fn write_page(wiki: &Wiki, id: &str, body: &str) {
        let mut page = Page {
            id: id.to_string(),
            fm: Frontmatter {
                title: id.to_string(),
                description: format!("{id} description"),
                tags: vec![],
                created: "2026-07-22".to_string(),
                updated: "2026-07-22".to_string(),
                status: None,
                sources: vec![],
                pin: false,
                pin_level: None::<PinLevel>,
                aliases: vec![],
                extra: vec![],
            },
            body: body.to_string(),
        };
        wiki.save_page_raw(&mut page, false).unwrap();
    }

    #[test]
    fn cold_warm_and_incremental_loads() {
        let (_temp, wiki) = fixture();
        write_page(&wiki, "a", "**A.** first");
        write_page(&wiki, "b", "**B.** second");

        let cold = load(&wiki).unwrap();
        assert_eq!(cold.cache.state, CacheState::Updated);
        assert_eq!(cold.cache.pages_refreshed, 2);
        let warm = load(&wiki).unwrap();
        assert_eq!(warm.cache.state, CacheState::Hit);
        assert_eq!(warm.cache.pages_reused, 2);
        assert_eq!(cold.content_hash, warm.content_hash);

        write_page(&wiki, "b", "**B.** changed");
        let changed = load(&wiki).unwrap();
        assert_eq!(changed.cache.state, CacheState::Updated);
        assert_eq!(changed.cache.pages_reused, 1);
        assert_eq!(changed.cache.pages_refreshed, 1);
        assert_ne!(warm.content_hash, changed.content_hash);

        fs::remove_file(wiki.page_path("a").unwrap()).unwrap();
        let deleted = load(&wiki).unwrap();
        assert_eq!(deleted.cache.pages_deleted, 1);
        assert_eq!(deleted.pages.len(), 1);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn strong_native_identity_enables_warm_reuse() {
        let (_temp, wiki) = fixture();
        write_page(&wiki, "identity", "**Identity.** Stable page.");
        let metadata = fs::symlink_metadata(wiki.page_path("identity").unwrap()).unwrap();
        let signature = signature(&metadata).unwrap();
        assert!(signature.native.is_some());
        load(&wiki).unwrap();
        let warm = load(&wiki).unwrap();
        assert_eq!(warm.cache.pages_reused, 1);
        assert_eq!(warm.cache.pages_refreshed, 0);
    }

    #[test]
    fn corrupted_cache_rebuilds_without_hiding_pages() {
        let (_temp, wiki) = fixture();
        write_page(&wiki, "a", "**A.** first");
        load(&wiki).unwrap();
        fs::write(wiki.dir.join(CACHE_DIR).join(CACHE_FILE), b"not json").unwrap();

        let rebuilt = load(&wiki).unwrap();
        assert_eq!(rebuilt.pages.len(), 1);
        assert_eq!(rebuilt.cache.pages_refreshed, 1);
        assert_eq!(rebuilt.cache.state, CacheState::Updated);
    }

    #[test]
    fn corrupted_cache_pin_projection_cannot_hide_instruction() {
        let (_temp, wiki) = fixture();
        write_page(
            &wiki,
            "workflow-rule",
            "**Rule.** Always verify the result.",
        );
        let mut pinned = wiki.load_page("workflow-rule").unwrap();
        pinned.fm.pin = true;
        pinned.fm.pin_level = Some(PinLevel::Instruction);
        wiki.save_page_raw(&mut pinned, false).unwrap();
        load(&wiki).unwrap();

        let mut cache = load_cache(&wiki).unwrap().unwrap();
        let entry = cache
            .pages
            .iter_mut()
            .find(|entry| entry.id == "workflow-rule")
            .unwrap();
        entry.page.fm.pin = false;
        entry.page.fm.pin_level = None;
        wiki::atomic_write(
            &wiki.dir.join(CACHE_DIR).join(CACHE_FILE),
            serde_json::to_vec(&cache).unwrap(),
        )
        .unwrap();

        let output = crate::commands::prime(
            &wiki,
            &crate::commands::PrimeOptions {
                query: "verify work".to_string(),
                tokens: Some(2_000),
                instruction_tokens: Some(1_000),
                limit: Some(5),
                max_per_section: Some(5),
                since: None,
                cursor: 0,
                context_hash: None,
                cwd: None,
            },
            true,
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(value["instructions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|instruction| instruction["id"] == "workflow-rule"));
    }

    #[test]
    fn oversized_cache_and_blocked_writer_fall_back_to_canonical_pages() {
        let (_temp, wiki) = fixture();
        write_page(&wiki, "a", "**A.** first");
        load(&wiki).unwrap();
        let cache_path = wiki.dir.join(CACHE_DIR).join(CACHE_FILE);
        File::options()
            .write(true)
            .open(&cache_path)
            .unwrap()
            .set_len(MAX_CACHE_BYTES + 1)
            .unwrap();
        let rebuilt = load(&wiki).unwrap();
        assert_eq!(rebuilt.pages.len(), 1);
        assert_eq!(rebuilt.cache.state, CacheState::Updated);

        write_page(&wiki, "a", "**A.** changed while journal exists");
        fs::write(wiki.dir.join(crate::publish::PUBLISH_JOURNAL_PATH), "{}").unwrap();
        let bypassed = load(&wiki).unwrap();
        assert_eq!(bypassed.cache.state, CacheState::Bypassed);
        assert!(bypassed.pages[0].body.contains("changed while journal"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_cache_is_never_followed_or_replaced() {
        use std::os::unix::fs::symlink;

        let (temp, wiki) = fixture();
        write_page(&wiki, "a", "**A.** first");
        load(&wiki).unwrap();
        let cache_path = wiki.dir.join(CACHE_DIR).join(CACHE_FILE);
        fs::remove_file(&cache_path).unwrap();
        let outside = temp.path().join("outside-sentinel");
        fs::write(&outside, "unchanged").unwrap();
        symlink(&outside, &cache_path).unwrap();

        let catalog = load(&wiki).unwrap();
        assert_eq!(catalog.cache.state, CacheState::Bypassed);
        assert_eq!(fs::read_to_string(outside).unwrap(), "unchanged");
    }

    #[test]
    fn many_page_warm_load_reuses_every_projection() {
        let (_temp, wiki) = fixture();
        for index in 0..250 {
            let id = format!("page-{index:03}");
            let page = Page::parse(&id, &format!("**Page {index}.** searchable body {index}."));
            fs::write(wiki.page_path(&id).unwrap(), page.render()).unwrap();
        }
        let cold = load(&wiki).unwrap();
        assert_eq!(cold.cache.pages_refreshed, 250);
        let warm = load(&wiki).unwrap();
        assert_eq!(warm.cache.state, CacheState::Hit);
        assert_eq!(warm.cache.pages_reused, 250);
        assert_eq!(warm.cache.pages_refreshed, 0);
        assert_eq!(warm.cache.pin_memberships_reused, 250);
        assert_eq!(warm.cache.catalog_scans, 2);

        let output = crate::commands::prime(
            &wiki,
            &crate::commands::PrimeOptions {
                query: "searchable".to_string(),
                tokens: Some(2_000),
                instruction_tokens: Some(500),
                limit: Some(5),
                max_per_section: Some(5),
                since: None,
                cursor: 0,
                context_hash: None,
                cwd: None,
            },
            true,
        )
        .unwrap();
        let value: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(value["telemetry"]["cache"]["pages_refreshed"], 0);
        assert_eq!(value["telemetry"]["cache"]["pin_memberships_reused"], 250);
        assert_eq!(value["telemetry"]["pin_pages_reread"], 0);
    }

    // Tiny dependency-free temporary directory shim for binary-unit tests.
    mod tempfile_shim {
        use std::path::{Path, PathBuf};
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT: AtomicU64 = AtomicU64::new(0);

        pub struct TempDir(PathBuf);

        impl TempDir {
            pub fn new() -> Self {
                let path = std::env::temp_dir().join(format!(
                    "wookie-retrieval-index-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ));
                std::fs::create_dir_all(&path).unwrap();
                Self(path)
            }

            pub fn path(&self) -> &Path {
                &self.0
            }
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
