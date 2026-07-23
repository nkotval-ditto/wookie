//! Canonical identities for the on-disk wiki catalog.
//!
//! A catalog identity is a small Merkle-style aggregate: each page leaf is
//! the SHA-256 of its exact raw bytes, and the root frames the sorted page id
//! and leaf digest. Only page ids and content influence the result. File
//! permissions, timestamps, parse normalization, and cache serialization do
//! not, so every caller can name the same logical snapshot.

use anyhow::{bail, Result};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::Read as _;
use std::path::Path;
use std::time::SystemTime;

pub const MAX_SNAPSHOT_PAGE_BYTES: u64 = 16 * 1024 * 1024;
pub const MAX_SNAPSHOT_CATALOG_BYTES: u64 = 256 * 1024 * 1024;

const CATALOG_DOMAIN: &[u8] = b"wookie.raw-catalog/v1";

/// One exact on-disk page represented without retaining its raw bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawPageFingerprint {
    pub id: String,
    pub raw_sha256: String,
}

#[derive(Debug, Clone)]
pub struct CapturedPage {
    pub id: String,
    pub raw: Vec<u8>,
    pub raw_sha256: String,
}

#[derive(Debug, Clone)]
pub struct CapturedCatalog {
    pub pages: Vec<CapturedPage>,
    pub content_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileIdentity {
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
}

fn file_identity(metadata: &fs::Metadata) -> Result<FileIdentity> {
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        bail!("snapshot inputs must be regular files, not symlinks");
    }
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt as _;
    Ok(FileIdentity {
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
    })
}

fn hash_field(hash: &mut Sha256, bytes: &[u8]) {
    hash.update(u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_be_bytes());
    hash.update(bytes);
}

/// Canonical SHA-256 of exact page bytes, suitable for an index leaf.
pub fn raw_page_sha256(raw: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(raw))
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// Aggregate sorted page fingerprints into the canonical public wiki content
/// hash. Duplicate ids and malformed leaf digests are rejected instead of
/// producing an ambiguous identity.
pub fn catalog_content_hash(entries: &[RawPageFingerprint]) -> Result<String> {
    let mut entries = entries.iter().collect::<Vec<_>>();
    entries.sort_by(|left, right| left.id.cmp(&right.id));

    let mut previous: Option<&str> = None;
    let mut hash = Sha256::new();
    hash_field(&mut hash, CATALOG_DOMAIN);
    hash.update(
        u64::try_from(entries.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    for entry in entries {
        if previous == Some(entry.id.as_str()) {
            bail!("duplicate page id '{}' in catalog snapshot", entry.id);
        }
        if !valid_sha256(&entry.raw_sha256) {
            bail!("invalid raw SHA-256 for page '{}'", entry.id);
        }
        hash_field(&mut hash, entry.id.as_bytes());
        hash_field(&mut hash, entry.raw_sha256.as_bytes());
        previous = Some(&entry.id);
    }
    Ok(format!("sha256:{:x}", hash.finalize()))
}

/// Convenience for callers that already hold exact raw page bytes.
pub fn catalog_content_hash_from_raw<'a>(
    entries: impl IntoIterator<Item = (&'a str, &'a [u8])>,
) -> Result<String> {
    let entries = entries
        .into_iter()
        .map(|(id, raw)| RawPageFingerprint {
            id: id.to_string(),
            raw_sha256: raw_page_sha256(raw),
        })
        .collect::<Vec<_>>();
    catalog_content_hash(&entries)
}

/// Bounded stable read of one canonical page. The path is checked before and
/// after consuming one opened handle so replacements fail instead of silently
/// producing a mixed snapshot.
pub fn read_raw_page(path: &Path) -> Result<Vec<u8>> {
    let path_before = file_identity(&fs::symlink_metadata(path)?)?;
    if path_before.length > MAX_SNAPSHOT_PAGE_BYTES {
        bail!(
            "page {} exceeds the {MAX_SNAPSHOT_PAGE_BYTES}-byte snapshot safety limit",
            path.display()
        );
    }
    let file = File::open(path)?;
    let opened = file_identity(&file.metadata()?)?;
    if opened != path_before {
        bail!("page {} changed while opening snapshot", path.display());
    }
    let mut raw = Vec::with_capacity(usize::try_from(opened.length).unwrap_or_default());
    file.take(MAX_SNAPSHOT_PAGE_BYTES + 1)
        .read_to_end(&mut raw)?;
    if u64::try_from(raw.len()).unwrap_or(u64::MAX) > MAX_SNAPSHOT_PAGE_BYTES {
        bail!(
            "page {} exceeds the {MAX_SNAPSHOT_PAGE_BYTES}-byte snapshot safety limit",
            path.display()
        );
    }
    let path_after = file_identity(&fs::symlink_metadata(path)?)?;
    if path_after != opened || path_after.length != u64::try_from(raw.len()).unwrap_or(u64::MAX) {
        bail!("page {} changed while reading snapshot", path.display());
    }
    Ok(raw)
}

/// Read at most `limit + 1` bytes from a stable regular page. This supports
/// authoritative frontmatter discovery without loading every page body.
pub fn read_page_prefix(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let path_before = file_identity(&fs::symlink_metadata(path)?)?;
    if path_before.length > MAX_SNAPSHOT_PAGE_BYTES {
        bail!(
            "page {} exceeds the {MAX_SNAPSHOT_PAGE_BYTES}-byte snapshot safety limit",
            path.display()
        );
    }
    let file = File::open(path)?;
    let opened = file_identity(&file.metadata()?)?;
    if opened != path_before {
        bail!("page {} changed while opening snapshot", path.display());
    }
    let mut raw = Vec::with_capacity(
        usize::try_from(opened.length.min(limit.saturating_add(1))).unwrap_or_default(),
    );
    file.take(limit.saturating_add(1)).read_to_end(&mut raw)?;
    let path_after = file_identity(&fs::symlink_metadata(path)?)?;
    if path_after != opened {
        bail!("page {} changed while reading snapshot", path.display());
    }
    Ok(raw)
}

/// Capture an internally consistent strict catalog. Consumers can parse these
/// exact bytes instead of hashing and reopening each path independently.
pub fn capture_catalog(wiki: &crate::wiki::Wiki) -> Result<CapturedCatalog> {
    for _ in 0..3 {
        let files = wiki.page_files_strict()?;
        let before = files
            .iter()
            .map(|(id, path)| Ok((id.clone(), file_identity(&fs::symlink_metadata(path)?)?)))
            .collect::<Result<Vec<_>>>()?;
        let mut total = 0_u64;
        let mut pages = Vec::new();
        for (id, path) in &files {
            let raw = read_raw_page(path)?;
            total = total
                .checked_add(u64::try_from(raw.len()).unwrap_or(u64::MAX))
                .ok_or_else(|| anyhow::anyhow!("wiki snapshot size overflow"))?;
            if total > MAX_SNAPSHOT_CATALOG_BYTES {
                bail!(
                    "wiki catalog exceeds the {MAX_SNAPSHOT_CATALOG_BYTES}-byte snapshot safety limit"
                );
            }
            let raw_sha256 = raw_page_sha256(&raw);
            pages.push(CapturedPage {
                id: id.clone(),
                raw,
                raw_sha256,
            });
        }
        let after_files = wiki.page_files_strict()?;
        let after = after_files
            .iter()
            .map(|(id, path)| Ok((id.clone(), file_identity(&fs::symlink_metadata(path)?)?)))
            .collect::<Result<Vec<_>>>()?;
        if before != after {
            continue;
        }
        let content_hash = catalog_content_hash(
            &pages
                .iter()
                .map(|page| RawPageFingerprint {
                    id: page.id.clone(),
                    raw_sha256: page.raw_sha256.clone(),
                })
                .collect::<Vec<_>>(),
        )?;
        return Ok(CapturedCatalog {
            pages,
            content_hash,
        });
    }
    bail!("wiki catalog changed repeatedly while capturing a snapshot")
}

/// Read the strict on-disk catalog and return its canonical public content
/// identity. This is the single helper for `snapshot.wiki.content_hash`.
pub fn wiki_content_hash(wiki: &crate::wiki::Wiki) -> Result<String> {
    Ok(capture_catalog(wiki)?.content_hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_hash_is_order_independent_and_content_sensitive() {
        let first = catalog_content_hash_from_raw([
            ("b", b"second".as_slice()),
            ("a", b"first".as_slice()),
        ])
        .unwrap();
        let reordered = catalog_content_hash_from_raw([
            ("a", b"first".as_slice()),
            ("b", b"second".as_slice()),
        ])
        .unwrap();
        let changed = catalog_content_hash_from_raw([
            ("a", b"changed".as_slice()),
            ("b", b"second".as_slice()),
        ])
        .unwrap();

        assert_eq!(first, reordered);
        assert_ne!(first, changed);
        assert!(first.starts_with("sha256:"));
    }
}
