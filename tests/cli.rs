//! End-to-end tests driving the built binary against a temp WOOKIE_HOME.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Env {
    home: PathBuf,
    project: PathBuf,
}

impl Env {
    fn new() -> Env {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let base = std::env::temp_dir().join(format!(
            "wookie-test-{}-{n}",
            std::process::id()
        ));
        let home = base.join("home");
        let project = base.join("proj");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        Env { home, project }
    }

    fn run_in(&self, cwd: &Path, args: &[&str], stdin: Option<&str>) -> (bool, String, String) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_wookie"));
        cmd.args(args)
            .env("WOOKIE_HOME", &self.home)
            .current_dir(cwd)
            .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        if let Some(body) = stdin {
            use std::io::Write;
            child.stdin.take().unwrap().write_all(body.as_bytes()).unwrap();
        }
        let out = child.wait_with_output().unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        )
    }

    fn run(&self, args: &[&str], stdin: Option<&str>) -> (bool, String, String) {
        self.run_in(&self.project.clone(), args, stdin)
    }

    fn ok(&self, args: &[&str], stdin: Option<&str>) -> String {
        let (success, stdout, stderr) = self.run(args, stdin);
        assert!(success, "wookie {args:?} failed:\n{stdout}\n{stderr}");
        stdout
    }
}

#[test]
fn init_list_and_cwd_resolution() {
    let env = Env::new();
    let out = env.ok(&["init", "myproj", "--description", "Test wiki"], None);
    assert!(out.contains("Created wiki 'myproj'"));

    let out = env.ok(&["list"], None);
    assert!(out.contains("myproj"));

    // Resolution from cwd, no --wiki flag: seeded index page is readable.
    let out = env.ok(&["read", "index"], None);
    assert!(out.contains("managed by wookie"));

    // Outside the project, resolution fails with a helpful error.
    let (success, _, stderr) = env.run_in(&std::env::temp_dir(), &["toc"], None);
    assert!(!success);
    assert!(stderr.contains("--wiki"), "unexpected stderr: {stderr}");

    // ...but --wiki works from anywhere.
    let (success, stdout, _) = env.run_in(&std::env::temp_dir(), &["toc", "--wiki", "myproj"], None);
    assert!(success);
    assert!(stdout.contains("index"));
}

#[test]
fn new_write_read_expand_flow() {
    let env = Env::new();
    env.ok(&["init", "flow"], None);

    env.ok(
        &["new", "scheduler", "--tags", "core"],
        Some("Coordinates run execution.\n\nDetails involve the [[run-lifecycle]] and [[retry-policy]]."),
    );

    // Broken links become stubs via expand.
    let out = env.ok(&["expand"], None);
    assert!(out.contains("run-lifecycle"));
    assert!(out.contains("retry-policy"));

    // Stub is listed in toc and readable.
    let out = env.ok(&["toc"], None);
    assert!(out.contains("run-lifecycle"));
    assert!(out.contains("[stub]"));

    // Writing clears stub status.
    env.ok(
        &["write", "run-lifecycle"],
        Some("A run moves through queued, active and done states.\n\nSee also [[scheduler]]."),
    );
    let out = env.ok(&["read", "run-lifecycle"], None);
    assert!(!out.contains("status: stub"));

    // Expanded read inlines the linked summary.
    let out = env.ok(&["read", "scheduler", "--expand"], None);
    assert!(out.contains("Linked context"));
    assert!(out.contains("queued, active and done"));

    // Backlinks resolve both directions.
    let out = env.ok(&["links", "run-lifecycle"], None);
    assert!(out.contains("[[scheduler]]"));
}

#[test]
fn mv_rewrites_inbound_links() {
    let env = Env::new();
    env.ok(&["init", "mvtest"], None);
    env.ok(&["new", "alpha"], Some("Alpha page.\n\nLinks to [[beta]]."));
    env.ok(&["new", "beta"], Some("Beta page."));

    let out = env.ok(&["mv", "beta", "internals/beta"], None);
    assert!(out.contains("alpha"));

    let out = env.ok(&["read", "alpha"], None);
    assert!(out.contains("[[internals/beta]]"));
    assert!(!out.contains("[[beta]]"));
}

#[test]
fn doctor_reports_issues() {
    let env = Env::new();
    env.ok(&["init", "doc"], None);
    env.ok(&["new", "solo"], Some("A page that links to [[nowhere]]."));

    let out = env.ok(&["doctor"], None);
    assert!(out.contains("broken link"));
    assert!(out.contains("orphan"));
}

#[test]
fn search_finds_body_matches() {
    let env = Env::new();
    env.ok(&["init", "searchy"], None);
    env.ok(&["new", "notes"], Some("Summary here.\n\nThe flux capacitor needs 1.21 gigawatts."));

    let out = env.ok(&["search", "flux capacitor"], None);
    assert!(out.contains("notes"));
    assert!(out.contains("gigawatts"));

    let out = env.ok(&["search", "no-such-token-anywhere"], None);
    assert!(out.contains("No pages match"));
}

#[test]
fn json_output_is_parseable() {
    let env = Env::new();
    env.ok(&["init", "jsonwiki"], None);
    let out = env.ok(&["list", "--json"], None);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["wikis"][0]["slug"], "jsonwiki");

    let out = env.ok(&["context", "--json"], None);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["wiki"], "jsonwiki");
}

#[test]
fn invalid_page_ids_rejected() {
    let env = Env::new();
    env.ok(&["init", "ids"], None);
    let (success, _, stderr) = env.run(&["new", "../escape"], None);
    assert!(!success);
    assert!(stderr.contains("invalid") || stderr.contains("segment"), "stderr: {stderr}");
}

#[test]
fn worktree_resolves_to_main_checkout_wiki() {
    let env = Env::new();
    let git = |args: &[&str], cwd: &Path| {
        let ok = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "git {args:?} failed");
    };
    // Make the project a real git repo with one commit.
    git(&["init", "-q"], &env.project);
    std::fs::write(env.project.join("f.txt"), "x").unwrap();
    git(&["add", "."], &env.project);
    git(
        &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "init"],
        &env.project,
    );

    env.ok(&["init", "wtwiki"], None);

    // Create a linked worktree elsewhere and resolve from inside it.
    let wt = env.home.parent().unwrap().join("wt");
    git(&["worktree", "add", "-q", wt.to_str().unwrap()], &env.project);
    let (success, stdout, stderr) = env.run_in(&wt, &["context"], None);
    assert!(success, "resolution from worktree failed: {stderr}");
    assert!(stdout.contains("wtwiki"));
}
