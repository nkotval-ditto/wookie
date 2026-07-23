//! Lossless parsers for Git's NUL-delimited path output.
//!
//! Git's line-oriented formats quote unusual names and cannot represent a
//! filename containing a newline without an extra decoding layer. Wookie's
//! reports use UTF-8 strings, so invalid UTF-8 is rejected explicitly instead
//! of silently lossy-decoding it. Control characters remain exact in machine
//! data and are escaped only when rendered to a terminal.

use anyhow::{bail, Context, Result};
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::io::Read as _;
use std::path::Path;
use std::process::{Command, Stdio};

/// Capture stdout from a Git command without allowing an adversarial or
/// unexpectedly large repository to make `Command::output` allocate without a
/// bound. Callers still validate the record count after parsing because a byte
/// ceiling alone does not bound the number of tiny paths.
pub(crate) fn bounded_git_stdout<S: AsRef<OsStr>>(
    root: &Path,
    args: &[S],
    label: &str,
    max_bytes: usize,
) -> Result<Vec<u8>> {
    let mut command = Command::new("git");
    command.arg("-C").arg(root);
    for arg in args {
        command.arg(arg.as_ref());
    }
    let mut child = command
        .stdout(Stdio::piped())
        // The caller reports a stable high-level error. Discard stderr so a
        // hostile Git configuration cannot fill a second pipe while stdout is
        // consumed with a bound.
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("starting {label} in {}", root.display()))?;

    let mut stdout = Vec::new();
    let read_limit = u64::try_from(max_bytes)
        .unwrap_or(u64::MAX - 1)
        .saturating_add(1);
    let read_result = child
        .stdout
        .take()
        .context("Git stdout pipe is unavailable")?
        .take(read_limit)
        .read_to_end(&mut stdout);
    if let Err(error) = read_result {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error).with_context(|| format!("reading {label} in {}", root.display()));
    }
    if stdout.len() > max_bytes {
        let _ = child.kill();
        let _ = child.wait();
        bail!("{label} exceeds the {max_bytes}-byte safety limit");
    }

    let status = child
        .wait()
        .with_context(|| format!("waiting for {label} in {}", root.display()))?;
    if !status.success() {
        bail!("{label} failed in {}", root.display());
    }
    Ok(stdout)
}

/// Enforce both count and aggregate-byte bounds on a parsed path inventory.
/// The aggregate includes one delimiter byte per path, matching Git's `-z`
/// representation closely enough to keep the parsed allocation bounded too.
pub(crate) fn validate_path_inventory(
    paths: Vec<String>,
    label: &str,
    max_paths: usize,
    max_bytes: usize,
) -> Result<Vec<String>> {
    if paths.len() > max_paths {
        bail!(
            "{label} exceeds the {max_paths}-path safety limit (got {})",
            paths.len()
        );
    }
    let bytes = paths.iter().try_fold(0usize, |total, path| {
        total
            .checked_add(path.len())
            .and_then(|value| value.checked_add(1))
            .context("Git path inventory size overflow")
    })?;
    if bytes > max_bytes {
        bail!("{label} exceeds the {max_bytes}-byte safety limit");
    }
    Ok(paths)
}

fn fields<'a>(output: &'a [u8], label: &str) -> Result<Vec<&'a [u8]>> {
    if output.is_empty() {
        return Ok(Vec::new());
    }
    if output.last() != Some(&0) {
        bail!("{label} is not terminated by NUL");
    }
    Ok(output[..output.len() - 1]
        .split(|byte| *byte == 0)
        .collect())
}

fn path(field: &[u8], label: &str) -> Result<String> {
    if field.is_empty() {
        bail!("{label} contains an empty path");
    }
    let value = std::str::from_utf8(field)
        .with_context(|| format!("{label} contains a path that is not valid UTF-8"))?;
    Ok(value.to_string())
}

pub fn validate_path(value: &str, label: &str) -> Result<String> {
    path(value.as_bytes(), label)
}

/// Parse `git ls-files -z` or another plain NUL-delimited path list.
pub fn parse_path_list(output: &[u8], label: &str) -> Result<Vec<String>> {
    let mut paths = BTreeSet::new();
    for field in fields(output, label)? {
        paths.insert(path(field, label)?);
    }
    Ok(paths.into_iter().collect())
}

fn rename_or_copy(status: &[u8]) -> bool {
    matches!(status.first(), Some(b'R' | b'C'))
}

/// Parse `git diff --name-status -z`. Rename and copy records contribute both
/// source and destination paths so provenance on either side is reconciled.
pub fn parse_name_status(output: &[u8], label: &str) -> Result<Vec<String>> {
    let fields = fields(output, label)?;
    let mut cursor = 0;
    let mut paths = BTreeSet::new();
    while cursor < fields.len() {
        let status = fields[cursor];
        cursor += 1;
        if status.is_empty()
            || !status[0].is_ascii_uppercase()
            || !status.iter().all(u8::is_ascii_alphanumeric)
        {
            bail!("{label} contains an invalid name-status record");
        }
        let count = if rename_or_copy(status) { 2 } else { 1 };
        if fields.len().saturating_sub(cursor) < count {
            bail!("{label} contains a truncated name-status record");
        }
        for field in &fields[cursor..cursor + count] {
            paths.insert(path(field, label)?);
        }
        cursor += count;
    }
    Ok(paths.into_iter().collect())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PorcelainPaths {
    pub dirty: Vec<String>,
    pub staged: Vec<String>,
}

/// Parse `git status --porcelain=v1 -z`. In `-z` mode a rename/copy record is
/// `XY destination\0source\0`; both names are included deterministically.
pub fn parse_porcelain_v1(output: &[u8], label: &str) -> Result<PorcelainPaths> {
    let fields = fields(output, label)?;
    let mut cursor = 0;
    let mut dirty = BTreeSet::new();
    let mut staged = BTreeSet::new();
    while cursor < fields.len() {
        let record = fields[cursor];
        cursor += 1;
        if record.len() < 4 || record[2] != b' ' {
            bail!("{label} contains an invalid porcelain record");
        }
        let x = record[0];
        let y = record[1];
        let destination = path(&record[3..], label)?;
        let is_staged = x != b' ' && !matches!(x, b'?' | b'!');
        if is_staged {
            staged.insert(destination.clone());
        }
        dirty.insert(destination);

        if matches!(x, b'R' | b'C') || matches!(y, b'R' | b'C') {
            let Some(source) = fields.get(cursor) else {
                bail!("{label} contains a truncated rename/copy record");
            };
            cursor += 1;
            let source = path(source, label)?;
            if is_staged {
                staged.insert(source.clone());
            }
            dirty.insert(source);
        }
    }
    Ok(PorcelainPaths {
        dirty: dirty.into_iter().collect(),
        staged: staged.into_iter().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn temp_repo(label: &str) -> std::path::PathBuf {
        let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "wookie-git-paths-{label}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        assert!(Command::new("git")
            .args(["init", "-q"])
            .current_dir(&root)
            .status()
            .unwrap()
            .success());
        root
    }

    #[test]
    fn plain_paths_preserve_spaces_and_unicode() {
        let parsed = parse_path_list("a file\0bé.rs\0".as_bytes(), "paths").unwrap();
        assert_eq!(parsed, vec!["a file", "bé.rs"]);
    }

    #[test]
    fn name_status_includes_both_sides_of_renames_and_copies() {
        let parsed = parse_name_status(
            b"M\0same.rs\0R100\0old name.rs\0new name.rs\0C75\0src.rs\0copy.rs\0",
            "diff",
        )
        .unwrap();
        assert_eq!(
            parsed,
            vec!["copy.rs", "new name.rs", "old name.rs", "same.rs", "src.rs"]
        );
    }

    #[test]
    fn porcelain_rename_is_staged_on_both_paths() {
        let parsed =
            parse_porcelain_v1(b"R  new name\0old name\0?? loose file\0", "status").unwrap();
        assert_eq!(parsed.dirty, vec!["loose file", "new name", "old name"]);
        assert_eq!(parsed.staged, vec!["new name", "old name"]);
    }

    #[test]
    fn controls_are_preserved_for_machine_data_and_escaped_for_humans() {
        let parsed = parse_path_list(b"line\nbreak\0escape\x1bname\0", "paths").unwrap();
        assert_eq!(parsed, vec!["escape\u{1b}name", "line\nbreak"]);
        assert_eq!(
            crate::report::terminal_safe(&parsed[0]),
            "escape\\u{1b}name"
        );
        assert_eq!(crate::report::terminal_safe(&parsed[1]), "line\\nbreak");
    }

    #[test]
    fn invalid_utf8_and_truncated_records_are_rejected() {
        assert!(parse_path_list(b"bad\xff\0", "paths").is_err());
        assert!(parse_name_status(b"R100\0only-old\0", "diff").is_err());
    }

    #[test]
    fn bounded_git_capture_stops_before_unbounded_allocation() {
        let root = temp_repo("capture-bound");
        fs::write(root.join("first-path"), "one").unwrap();
        fs::write(root.join("second-path"), "two").unwrap();
        assert!(Command::new("git")
            .args(["add", "--", "first-path", "second-path"])
            .current_dir(&root)
            .status()
            .unwrap()
            .success());

        let error = bounded_git_stdout(&root, &["ls-files", "-z"], "test Git path inventory", 8)
            .unwrap_err()
            .to_string();
        assert!(error.contains("8-byte safety limit"), "{error}");

        let output =
            bounded_git_stdout(&root, &["ls-files", "-z"], "test Git path inventory", 1024)
                .unwrap();
        assert_eq!(parse_path_list(&output, "test paths").unwrap().len(), 2);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn parsed_path_inventory_has_independent_count_and_byte_limits() {
        let count_error = validate_path_inventory(
            vec!["a".into(), "b".into(), "c".into()],
            "test paths",
            2,
            1024,
        )
        .unwrap_err()
        .to_string();
        assert!(count_error.contains("2-path safety limit"), "{count_error}");

        let byte_error =
            validate_path_inventory(vec!["abcd".into(), "efgh".into()], "test paths", 10, 8)
                .unwrap_err()
                .to_string();
        assert!(byte_error.contains("8-byte safety limit"), "{byte_error}");
    }
}
