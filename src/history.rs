//! Serialized, path-scoped Git history for wiki mutations. Multiple agent
//! processes can mutate one wiki concurrently; the lock covers the complete
//! add+commit transaction so one command cannot accidentally label another
//! command's staged changes.

use crate::config::HistorySettings;
use anyhow::{bail, Context, Result};
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

pub(crate) struct ExclusiveLock {
    path: PathBuf,
    token: String,
}

impl Drop for ExclusiveLock {
    fn drop(&mut self) {
        remove_lock_if_token(&self.path, &self.token);
    }
}

static LOCK_COUNTER: AtomicU64 = AtomicU64::new(0);
const MAX_GIT_STDERR_BYTES: usize = 64 * 1024;

struct GitCommandOutput {
    status: std::process::ExitStatus,
    stderr: Vec<u8>,
    stderr_truncated: bool,
}

fn drain_git_stderr(mut stderr: impl std::io::Read) -> std::io::Result<(Vec<u8>, bool)> {
    let mut retained = Vec::with_capacity(8 * 1024);
    let mut truncated = false;
    let mut chunk = [0_u8; 8 * 1024];
    loop {
        let read = stderr.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        let remaining = MAX_GIT_STDERR_BYTES.saturating_sub(retained.len());
        let keep = remaining.min(read);
        retained.extend_from_slice(&chunk[..keep]);
        truncated |= keep < read;
    }
    Ok((retained, truncated))
}

/// Mutating Git commands must be allowed to finish: killing a commit merely
/// because a hook is noisy can leave its success ambiguous. Stdout is unused,
/// and stderr is continuously drained while retaining only a bounded prefix.
fn run_git_command(command: &mut Command) -> Result<GitCommandOutput> {
    command.stdout(Stdio::null()).stderr(Stdio::piped());
    let mut child = command.spawn().context("starting Git subprocess")?;
    let Some(stderr) = child.stderr.take() else {
        // The command may already have crossed the commit boundary. Never
        // kill it here; wait for completion before reporting the impossible
        // pipe-state error.
        let _ = child.wait();
        bail!("Git subprocess has no stderr pipe");
    };
    let stderr_handle = std::thread::spawn(move || drain_git_stderr(stderr));
    let first_wait = child.wait();
    let stderr_result = stderr_handle
        .join()
        .map_err(|_| anyhow::anyhow!("Git stderr reader panicked"))?
        .context("reading bounded Git stderr");
    // If the first wait failed, the closed stderr pipe proves the process has
    // finished; retry once to reap it. Killing is never safe after `git
    // commit` may already have updated HEAD.
    let status = match first_wait {
        Ok(status) => status,
        Err(first_error) => child.wait().with_context(|| {
            format!("waiting for Git subprocess after initial error: {first_error}")
        })?,
    };
    let (stderr, stderr_truncated) = stderr_result?;
    Ok(GitCommandOutput {
        status,
        stderr,
        stderr_truncated,
    })
}

fn git_stderr(output: &GitCommandOutput) -> String {
    let mut rendered = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if output.stderr_truncated {
        if !rendered.is_empty() {
            rendered.push('\n');
        }
        rendered.push_str("...[stderr truncated]");
    }
    rendered
}

fn lock_token() -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!(
        "{}-{nanos}-{}",
        std::process::id(),
        LOCK_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn read_lock_owner(path: &Path) -> Option<(u32, String)> {
    let mut owner = None;
    for entry in fs::read_dir(path).ok()?.flatten() {
        let name = entry.file_name();
        let name = name.to_str()?;
        let Some(encoded) = name.strip_prefix("owner-") else {
            continue;
        };
        let (pid, token) = encoded.split_once('-')?;
        let pid = pid.parse().ok()?;
        let file_type = entry.file_type().ok()?;
        if file_type.is_symlink() || !file_type.is_file() || token.is_empty() {
            return None;
        }
        if owner.replace((pid, token.to_string())).is_some() {
            return None;
        }
    }
    owner
}

fn remove_lock_if_token(path: &Path, expected: &str) -> bool {
    if let Some((pid, token)) = read_lock_owner(path) {
        if token == expected && fs::remove_file(path.join(format!("owner-{pid}-{token}"))).is_ok() {
            return fs::remove_dir(path).is_ok();
        }
    }
    false
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }
    let Ok(pid) = i32::try_from(pid) else {
        return true;
    };
    // SAFETY: signal 0 performs existence/permission checking only.
    if unsafe { kill(pid, 0) } == 0 {
        return true;
    }
    // EPERM means the process exists but cannot be signalled. Treat every
    // permission denial conservatively so a lock is never stolen from a live
    // process owned under different credentials.
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
    // SAFETY: the Windows APIs receive a process id and initialized output
    // pointer; every successfully opened handle is closed below.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            // Permission failures are treated as live: safety beats stealing.
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

fn lock_is_stale(path: &Path, stale_after: Duration) -> bool {
    fs::symlink_metadata(path)
        .ok()
        .filter(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
        .and_then(|metadata| metadata.modified().ok())
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age >= stale_after)
}

/// Inspect a lock directory that `create_dir` just reported as existing.
///
/// Another process can release the lock between those two operations.  That
/// disappearance is normal lock contention, not a history failure, so callers
/// should retry acquisition when this returns `None`.
fn existing_lock_metadata(path: &Path) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("inspecting history lock {}", path.display()))
        }
    }
}

pub(crate) fn acquire_named_lock(
    root: &Path,
    name: &str,
    settings: &HistorySettings,
) -> Result<ExclusiveLock> {
    if name.is_empty()
        || Path::new(name).is_absolute()
        || Path::new(name)
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        bail!("lock name must be one relative path segment: '{name}'");
    }
    let path = root.join(name);
    let started = Instant::now();
    let timeout = Duration::from_millis(settings.lock_timeout_ms);
    let stale_after = Duration::from_secs(settings.lock_stale_seconds);
    let token = lock_token();
    loop {
        match fs::create_dir(&path) {
            Ok(()) => {
                let owner_path = path.join(format!("owner-{}-{token}", std::process::id()));
                let setup = (|| -> Result<()> {
                    let file = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&owner_path)
                        .with_context(|| format!("creating {}", owner_path.display()))?;
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
                    return Err(error).context("initializing wiki history lock");
                }
                return Ok(ExclusiveLock {
                    path: path.clone(),
                    token: token.clone(),
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let Some(metadata) = existing_lock_metadata(&path)? else {
                    // The owner released the lock after our `create_dir`
                    // observed it. Retry immediately instead of surfacing a
                    // spurious ENOENT warning to the mutation that is about
                    // to commit.
                    continue;
                };
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    bail!(
                        "wiki history lock must be a real directory: {}",
                        path.display()
                    );
                }
                if lock_is_stale(&path, stale_after) {
                    if let Some((pid, token)) = read_lock_owner(&path) {
                        if !process_is_alive(pid) {
                            // The directory cannot be replaced until it is
                            // removed, so token verification plus remove_dir
                            // prevents deleting a newer owner's lock.
                            if remove_lock_if_token(&path, &token) {
                                continue;
                            }
                        }
                    } else if fs::remove_dir(&path).is_ok() {
                        // An empty directory means acquisition was interrupted
                        // before its owner marker became durable.
                        continue;
                    }
                }
                if started.elapsed() >= timeout {
                    bail!(
                        "timed out after {} ms waiting for wiki history lock {}",
                        settings.lock_timeout_ms,
                        path.display()
                    );
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                return Err(error).with_context(|| format!("creating {}", path.display()));
            }
        }
    }
}

impl ExclusiveLock {
    pub(crate) fn is_for(&self, root: &Path, name: &str) -> bool {
        self.path == root.join(name)
    }
}

fn run_git(wiki_dir: &Path, args: &[&str]) -> Result<GitCommandOutput> {
    let mut command = Command::new("git");
    command.arg("-C").arg(wiki_dir).args(args);
    run_git_command(&mut command).with_context(|| format!("running git in {}", wiki_dir.display()))
}

fn validate_history_paths(relative_paths: &[String]) -> Result<()> {
    if relative_paths.is_empty() {
        bail!("history operation requires at least one wiki-relative path");
    }
    if relative_paths.len() > 512
        || relative_paths
            .iter()
            .map(|path| path.len() + 1)
            .sum::<usize>()
            > 12 * 1024
    {
        bail!("history path arguments exceed the safe process limit");
    }
    for path in relative_paths {
        let candidate = Path::new(path);
        if path.is_empty()
            || candidate.is_absolute()
            || candidate.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir
                        | std::path::Component::RootDir
                        | std::path::Component::Prefix(_)
                )
            })
        {
            bail!("history path must be wiki-relative: '{path}'");
        }
    }
    Ok(())
}

/// Restore only the named index entries after a failed path-scoped commit.
/// Output follows the same bounded, completion-preserving contract as commit.
pub(crate) fn reset_paths(wiki_dir: &Path, relative_paths: &[String]) -> Result<()> {
    validate_history_paths(relative_paths)?;
    let mut reset = Command::new("git");
    reset
        .arg("-C")
        .arg(wiki_dir)
        .args(["reset", "-q", "HEAD", "--"])
        .args(relative_paths);
    let output = run_git_command(&mut reset)
        .with_context(|| format!("restoring the Git index in {}", wiki_dir.display()))?;
    if !output.status.success() {
        bail!("git reset failed: {}", git_stderr(&output));
    }
    Ok(())
}

/// Commit only the provided wiki-relative paths. Empty path sets are rejected
/// so a caller bug can never fall through to whole-wiki staging. Returns false
/// when the named paths contain no changes.
pub fn commit_paths(
    wiki_dir: &Path,
    message: &str,
    relative_paths: &[String],
    settings: &HistorySettings,
) -> Result<bool> {
    settings.validate()?;
    let message = canonical_commit_message(message);
    validate_history_paths(relative_paths)?;
    validate_git_command_size(wiki_dir, &message, relative_paths)?;
    let _lock = acquire_named_lock(wiki_dir, ".history.lock", settings)?;

    let mut add = Command::new("git");
    add.arg("-C").arg(wiki_dir).arg("add").arg("-A");
    add.arg("--");
    for path in relative_paths {
        add.arg(path);
    }
    let output = run_git_command(&mut add)
        .with_context(|| format!("staging wiki changes in {}", wiki_dir.display()))?;
    if !output.status.success() {
        bail!("git add failed: {}", git_stderr(&output));
    }

    let mut diff_args = vec!["diff", "--cached", "--quiet"];
    diff_args.push("--");
    diff_args.extend(relative_paths.iter().map(String::as_str));
    let diff = run_git(wiki_dir, &diff_args)?;
    match diff.status.code() {
        Some(0) => return Ok(false),
        Some(1) => {}
        _ => bail!("git diff --cached failed: {}", git_stderr(&diff)),
    }

    let mut commit = Command::new("git");
    commit.arg("-C").arg(wiki_dir).args([
        "-c",
        "user.name=wookie",
        "-c",
        "user.email=wookie@localhost",
        "commit",
        "-q",
        "--cleanup=verbatim",
        "--allow-empty-message",
        "-m",
        &message,
    ]);
    // Path-limited commits avoid consuming unrelated entries that a human or
    // interrupted older wookie process left staged in the index.
    commit.arg("--only").arg("--");
    commit.args(relative_paths);
    let output = run_git_command(&mut commit)
        .with_context(|| format!("committing wiki changes in {}", wiki_dir.display()))?;
    if !output.status.success() {
        bail!("git commit failed: {}", git_stderr(&output));
    }
    Ok(true)
}

/// Canonical message supplied to Git and recorded in publish journals.
///
/// Git commit objects terminate every non-empty `-m` message with one LF. We
/// therefore remove only terminal CR/LF characters up front, preserve every
/// other byte (including comment-looking lines and trailing spaces), and use
/// `--cleanup=verbatim` at commit time.
pub(crate) fn canonical_commit_message(message: &str) -> String {
    message.trim_end_matches(['\r', '\n']).to_string()
}

/// Conservative cross-platform bound for the largest Git invocation used by
/// `commit_paths`. Windows CreateProcess counts UTF-16 command-line units and
/// quoting can expand arguments, so budget twice the visible units plus fixed
/// Git flags rather than adding independent path/message byte ceilings.
fn validate_git_command_size(
    wiki_dir: &Path,
    message: &str,
    relative_paths: &[String],
) -> Result<()> {
    const MAX_COMMAND_UNITS: usize = 30 * 1024;
    const FIXED_AND_TERMINATOR_UNITS: usize = 1024;
    let visible = wiki_dir.to_string_lossy().encode_utf16().count()
        + message.encode_utf16().count()
        + relative_paths
            .iter()
            .map(|path| path.encode_utf16().count() + 1)
            .sum::<usize>();
    let conservative = FIXED_AND_TERMINATOR_UNITS.saturating_add(visible.saturating_mul(2));
    if conservative > MAX_COMMAND_UNITS {
        bail!(
            "history Git command would exceed the safe cross-platform command-line limit ({conservative} > {MAX_COMMAND_UNITS} UTF-16 units)"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_parent_history_paths() {
        let settings = HistorySettings::default();
        let result = commit_paths(
            Path::new("/path/that/is/not/used"),
            "test",
            &["../escape".into()],
            &settings,
        );
        assert!(result.unwrap_err().to_string().contains("wiki-relative"));
    }

    #[test]
    fn rejects_empty_history_path_sets_before_running_git() {
        let result = commit_paths(
            Path::new("/path/that/is/not/used"),
            "test",
            &[],
            &HistorySettings::default(),
        );
        assert!(result.unwrap_err().to_string().contains("at least one"));
    }

    #[test]
    fn combined_message_and_paths_fit_a_windows_safe_command_budget() {
        let root = Path::new("/a/reasonably/long/wiki/root");
        let paths = (0..400)
            .map(|index| format!("pages/section/page-{index:04}.md"))
            .collect::<Vec<_>>();
        let error = validate_git_command_size(root, &"m".repeat(12_000), &paths)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("cross-platform command-line limit"),
            "{error}"
        );

        validate_git_command_size(
            root,
            "normal publish message",
            &["pages/architecture/overview.md".to_string()],
        )
        .unwrap();
    }

    #[test]
    fn commit_message_cleanup_is_verbatim_with_one_canonical_terminator() {
        let dir = std::env::temp_dir().join(format!(
            "wookie-history-message-{}-{}",
            std::process::id(),
            LOCK_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(dir.join("pages")).unwrap();
        assert!(Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success());
        fs::write(dir.join("pages/example.md"), "**Example.**\n").unwrap();

        let supplied = "Subject\n# comment-looking line\ntrailing spaces stay  \r\n\n";
        assert!(commit_paths(
            &dir,
            supplied,
            &["pages/example.md".to_string()],
            &HistorySettings::default(),
        )
        .unwrap());

        let object = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["cat-file", "commit", "HEAD"])
            .output()
            .unwrap();
        assert!(object.status.success());
        let separator = object
            .stdout
            .windows(2)
            .position(|window| window == b"\n\n")
            .unwrap();
        assert_eq!(
            &object.stdout[separator + 2..],
            b"Subject\n# comment-looking line\ntrailing spaces stay  \n"
        );
        assert_eq!(
            canonical_commit_message(supplied),
            "Subject\n# comment-looking line\ntrailing spaces stay  "
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn noisy_successful_commit_hook_is_drained_without_killing_commit() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = std::env::temp_dir().join(format!(
            "wookie-history-noisy-hook-{}-{}",
            std::process::id(),
            LOCK_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(dir.join("pages")).unwrap();
        assert!(Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["init", "-q"])
            .status()
            .unwrap()
            .success());
        let hook = dir.join(".git/hooks/pre-commit");
        fs::write(
            &hook,
            "#!/bin/sh\ni=0\nwhile [ \"$i\" -lt 8192 ]; do\n  printf '0123456789abcdef' >&2\n  i=$((i + 1))\ndone\nexit 0\n",
        )
        .unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(dir.join("pages/example.md"), "**Example.**\n").unwrap();

        assert!(commit_paths(
            &dir,
            "noisy hook succeeds",
            &["pages/example.md".to_string()],
            &HistorySettings::default(),
        )
        .unwrap());
        assert!(Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["rev-parse", "--verify", "HEAD"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn old_owner_cannot_remove_replaced_lock() {
        let dir = std::env::temp_dir().join(format!(
            "wookie-history-token-{}-{}",
            std::process::id(),
            LOCK_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".history.lock");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("owner-1-old"), "").unwrap();
        let lock = ExclusiveLock {
            path: path.clone(),
            token: "old".into(),
        };
        fs::remove_file(path.join("owner-1-old")).unwrap();
        fs::remove_dir(&path).unwrap();
        fs::create_dir(&path).unwrap();
        fs::write(path.join("owner-2-new"), "").unwrap();
        drop(lock);
        assert!(path.join("owner-2-new").exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn disappeared_existing_lock_is_retryable_contention() {
        let dir = std::env::temp_dir().join(format!(
            "wookie-history-disappeared-{}-{}",
            std::process::id(),
            LOCK_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".history.lock");

        // This is the state seen when create_dir returned AlreadyExists but
        // the current owner removed its lock before metadata inspection.
        assert!(existing_lock_metadata(&path).unwrap().is_none());

        fs::remove_dir_all(dir).unwrap();
    }
}
