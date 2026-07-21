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
        let base = std::env::temp_dir().join(format!("wookie-test-{}-{n}", std::process::id()));
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
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().unwrap();
        if let Some(body) = stdin {
            use std::io::Write;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(body.as_bytes())
                .unwrap();
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
    assert!(out.contains("front door of this wiki"));

    // Outside the project, resolution fails with a helpful error.
    let (success, _, stderr) = env.run_in(&std::env::temp_dir(), &["toc"], None);
    assert!(!success);
    assert!(stderr.contains("--wiki"), "unexpected stderr: {stderr}");

    // ...but --wiki works from anywhere.
    let (success, stdout, _) =
        env.run_in(&std::env::temp_dir(), &["toc", "--wiki", "myproj"], None);
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
    env.ok(
        &["new", "notes"],
        Some("Summary here.\n\nThe flux capacitor needs 1.21 gigawatts."),
    );

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
    assert!(
        stderr.contains("invalid") || stderr.contains("segment"),
        "stderr: {stderr}"
    );
}

#[test]
fn wiki_slugs_cannot_escape_wookie_home() {
    let env = Env::new();
    let outside = env.home.parent().unwrap().join("outside-wiki");
    std::fs::create_dir_all(outside.join("pages")).unwrap();
    std::fs::write(
        outside.join("wookie.toml"),
        "name = \"outside-wiki\"\nproject_roots = []\n",
    )
    .unwrap();

    let (success, _, stderr) = env.run(&["context", "--wiki", "../outside-wiki"], None);
    assert!(!success, "path-like wiki slug must be rejected");
    assert!(stderr.contains("invalid wiki slug"), "got: {stderr}");

    let (success, _, _) = env.run(&["remove-wiki", "../outside-wiki", "--force"], None);
    assert!(!success, "remove-wiki must not escape WOOKIE_HOME");
    assert!(outside.exists(), "outside directory was deleted");
}

#[cfg(unix)]
#[test]
fn wiki_symlinks_cannot_escape_wookie_home() {
    let env = Env::new();
    let outside = env.home.parent().unwrap().join("symlink-target");
    std::fs::create_dir_all(outside.join("pages")).unwrap();
    std::fs::write(
        outside.join("wookie.toml"),
        "name = \"symlink-target\"\nproject_roots = []\n",
    )
    .unwrap();
    std::os::unix::fs::symlink(&outside, env.home.join("linked-wiki")).unwrap();

    let (success, _, stderr) = env.run(&["context", "--wiki", "linked-wiki"], None);
    assert!(!success, "symlinked wiki must not escape WOOKIE_HOME");
    assert!(stderr.contains("direct directory"), "got: {stderr}");
}

#[test]
fn ingest_fresh_then_update_lifecycle() {
    let env = Env::new();
    let git = |args: &[&str]| {
        let ok = Command::new("git")
            .args(args)
            .current_dir(&env.project)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "git {args:?} failed");
    };
    let commit = |msg: &str| {
        git(&["add", "-A"]);
        git(&[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            msg,
        ]);
    };

    // A small fake codebase.
    git(&["init", "-q"]);
    std::fs::create_dir_all(env.project.join("src/scheduler")).unwrap();
    std::fs::create_dir_all(env.project.join("docs")).unwrap();
    std::fs::write(env.project.join("README.md"), "# proj").unwrap();
    for f in [
        "src/main.rs",
        "src/lib.rs",
        "src/scheduler/mod.rs",
        "src/scheduler/queue.rs",
        "src/scheduler/retry.rs",
        "docs/notes.md",
    ] {
        std::fs::write(env.project.join(f), "x").unwrap();
    }
    commit("init");

    env.ok(&["init", "ingesty"], None);

    // Fresh ingest seeds module stubs and prints the worklist.
    let out = env.ok(&["ingest", "--level", "standard"], None);
    assert!(out.contains("fresh"), "expected fresh mode: {out}");
    assert!(out.contains("code/src"), "expected src stub: {out}");
    assert!(
        out.contains("code/src/scheduler"),
        "expected submodule stub: {out}"
    );
    assert!(out.contains("README.md"), "expected entry points: {out}");
    assert!(
        out.contains("ingest --mark"),
        "worklist should end with --mark: {out}"
    );

    // The seeded stub carries sources pointing at its directory.
    let page = env.ok(&["read", "code/src/scheduler"], None);
    assert!(
        page.contains("sources: [\"src/scheduler/\"]"),
        "stub sources missing: {page}"
    );

    // Mark, then confirm a no-change update reports in-sync.
    env.ok(&["ingest", "--mark"], None);
    let out = env.ok(&["ingest"], None);
    assert!(out.contains("in sync"), "expected in-sync: {out}");

    // Change a scheduler file; the scheduler page must go stale.
    std::fs::write(env.project.join("src/scheduler/retry.rs"), "changed").unwrap();
    commit("touch scheduler");
    let out = env.ok(&["ingest"], None);
    assert!(out.contains("update"), "expected update mode: {out}");
    assert!(
        out.contains("code/src/scheduler"),
        "expected stale page: {out}"
    );
    assert!(
        out.contains("src/scheduler/retry.rs"),
        "expected changed file: {out}"
    );

    // Doctor also notices the wiki is behind the code.
    let out = env.ok(&["doctor"], None);
    assert!(
        out.contains("since last ingest"),
        "doctor should flag staleness: {out}"
    );

    // A brand-new top-level module gets seeded during update.
    std::fs::create_dir_all(env.project.join("plugins")).unwrap();
    std::fs::write(env.project.join("plugins/loader.rs"), "x").unwrap();
    commit("add plugins");
    let out = env.ok(&["ingest"], None);
    assert!(
        out.contains("code/plugins"),
        "expected new module stub: {out}"
    );
}

#[test]
fn write_sets_sources() {
    let env = Env::new();
    env.ok(&["init", "srcy"], None);
    env.ok(
        &["new", "concepts/auth", "--sources", "src/auth"],
        Some("Auth overview."),
    );
    let page = env.ok(&["read", "concepts/auth"], None);
    assert!(page.contains("sources: [\"src/auth\"]"), "got: {page}");

    env.ok(
        &[
            "write",
            "concepts/auth",
            "--sources",
            "src/auth,src/session.rs",
        ],
        Some("Updated overview."),
    );
    let page = env.ok(&["read", "concepts/auth"], None);
    assert!(
        page.contains("sources: [\"src/auth\", \"src/session.rs\"]"),
        "got: {page}"
    );
}

#[test]
fn sections_group_toc_and_flag_unfiled() {
    let env = Env::new();
    env.ok(&["init", "secty"], None);
    env.ok(&["new", "architecture/overview"], Some("The big picture."));
    let out = env.ok(&["new", "randopage"], Some("Floating knowledge."));
    assert!(out.contains("unfiled"), "expected filing note: {out}");

    let toc = env.ok(&["toc"], None);
    assert!(
        toc.contains("architecture/ —"),
        "expected section header: {toc}"
    );
    assert!(
        toc.contains("workflow/ [rules, locked] —"),
        "expected workflow flags: {toc}"
    );
    assert!(toc.contains("unfiled"), "expected unfiled group: {toc}");

    let out = env.ok(&["doctor"], None);
    assert!(
        out.contains("unfiled page"),
        "doctor should flag unfiled: {out}"
    );
}

#[test]
fn doctor_requires_section_required_pages() {
    let env = Env::new();
    env.ok(&["init", "reqy"], None);
    let out = env.ok(&["doctor"], None);
    assert!(
        out.contains("missing required page: 'architecture/overview'"),
        "got: {out}"
    );
    env.ok(&["new", "architecture/overview"], Some("The big picture."));
    let out = env.ok(&["doctor"], None);
    assert!(!out.contains("missing required page"), "got: {out}");
}

#[test]
fn locked_sections_block_until_unlocked() {
    let env = Env::new();
    env.ok(&["init", "locky"], None);

    // Rules sections are locked by default.
    let (success, _, stderr) = env.run(&["new", "style/naming"], Some("Use snake_case."));
    assert!(!success);
    assert!(stderr.contains("locked"), "expected lock error: {stderr}");
    assert!(
        stderr.contains("ask the user"),
        "error should instruct asking: {stderr}"
    );

    // Unlock, write, relock, blocked again.
    env.ok(&["unlock", "style"], None);
    env.ok(&["new", "style/naming"], Some("Use snake_case."));
    env.ok(&["lock", "style"], None);
    let (success, _, stderr) = env.run(&["write", "style/naming"], Some("Changed."));
    assert!(!success, "relock should block writes: {stderr}");

    // Info sections are never locked.
    env.ok(&["new", "guides/build"], Some("Run cargo build."));

    // Expand skips stubs that would land in locked sections.
    env.ok(
        &["new", "guides/deploy"],
        Some("See [[style/imports]] first."),
    );
    let out = env.ok(&["expand"], None);
    assert!(
        out.contains("Skipped"),
        "expand should skip locked targets: {out}"
    );
    let (created, _, _) = env.run(&["read", "style/imports"], None);
    assert!(!created, "stub must not be created in a locked section");
}

#[test]
fn critique_briefing_includes_rules_and_target() {
    let env = Env::new();
    let git = |args: &[&str]| {
        assert!(Command::new("git")
            .args(args)
            .current_dir(&env.project)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success());
    };
    git(&["init", "-q"]);
    std::fs::write(env.project.join("main.rs"), "fn main() {}").unwrap();
    git(&["add", "-A"]);
    git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-q",
        "-m",
        "init",
    ]);

    env.ok(&["init", "crity"], None);
    env.ok(&["unlock", "style"], None);
    env.ok(
        &["new", "style/checks"],
        Some("Look at every changed .rs file and check naming of new functions."),
    );
    env.ok(
        &["new", "style/naming"],
        Some("Functions are snake_case, no abbreviations."),
    );

    // An uncommitted change is the default target.
    std::fs::write(env.project.join("main.rs"), "fn main() { let x = 1; }").unwrap();

    let out = env.ok(&["critique"], None);
    assert!(out.contains("main.rs"), "target file missing: {out}");
    assert!(
        out.contains("How to verify (style/checks)"),
        "checks page missing: {out}"
    );
    assert!(
        out.contains("snake_case, no abbreviations"),
        "rule body missing: {out}"
    );
    assert!(out.contains("Output contract"), "contract missing: {out}");
    assert!(
        out.contains("workflow/checks page") || out.contains("no workflow/checks"),
        "workflow section without checks should be noted: {out}"
    );
}

#[test]
fn pinned_pages_inline_in_context() {
    let env = Env::new();
    env.ok(&["init", "pinny"], None);
    env.ok(&["unlock", "workflow"], None);
    env.ok(
        &["new", "workflow/commits", "--pin"],
        Some("Always use conventional commits.\n\nScope tags come from the module name."),
    );
    let out = env.ok(&["context"], None);
    assert!(out.contains("Pinned instructions"), "got: {out}");
    assert!(
        out.contains("conventional commits"),
        "pinned body should be inlined: {out}"
    );

    env.ok(
        &["write", "workflow/commits", "--unpin"],
        Some("Always use conventional commits."),
    );
    let out = env.ok(&["context"], None);
    assert!(!out.contains("Pinned instructions"), "unpin failed: {out}");
}

#[test]
fn uppercase_ids_rejected_no_case_bypass() {
    let env = Env::new();
    env.ok(&["init", "casey"], None);
    env.ok(&["unlock", "style"], None);
    env.ok(&["new", "style/checks"], Some("The real rule."));
    env.ok(&["lock", "style"], None);

    // The APFS attack: STYLE/checks aliases style/checks on macOS.
    let (success, _, stderr) = env.run(&["write", "STYLE/checks"], Some("overwritten"));
    assert!(!success, "case-variant write must fail");
    assert!(stderr.contains("lowercase"), "got: {stderr}");
    let (success, _, _) = env.run(&["new", "Style/naming"], Some("x"));
    assert!(!success, "case-variant create must fail");

    let page = env.ok(&["read", "style/checks"], None);
    assert!(
        page.contains("The real rule."),
        "locked page must be intact: {page}"
    );
}

#[test]
fn critique_sees_untracked_files() {
    let env = Env::new();
    let git = |args: &[&str]| {
        assert!(Command::new("git")
            .args(args)
            .current_dir(&env.project)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success());
    };
    git(&["init", "-q"]);
    std::fs::write(env.project.join("main.rs"), "fn main() {}").unwrap();
    git(&["add", "-A"]);
    git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-q",
        "-m",
        "init",
    ]);
    env.ok(&["init", "untracky"], None);

    // A brand-new file, never git-added.
    std::fs::write(env.project.join("brand_new.rs"), "fn f() {}").unwrap();
    let out = env.ok(&["critique"], None);
    assert!(
        out.contains("brand_new.rs"),
        "untracked file missing from target: {out}"
    );
}

#[test]
fn doctor_strict_exits_nonzero() {
    let env = Env::new();
    env.ok(&["init", "stricty"], None);
    env.ok(&["new", "solo"], Some("Links to [[nowhere]]."));
    let (success, stdout, _) = env.run(&["doctor", "--strict"], None);
    assert!(!success, "strict doctor must fail on issues");
    assert!(stdout.contains("issue"), "report still printed: {stdout}");
    let (success, _, _) = env.run(&["doctor"], None);
    assert!(success, "non-strict doctor still exits 0");
}

#[test]
fn roots_edit_resolution_source_of_truth() {
    let env = Env::new();
    env.ok(&["init", "rooty"], None);

    // Second project dir, registered via roots --add on the wiki's own toml.
    let other = env.home.parent().unwrap().join("other-proj");
    std::fs::create_dir_all(&other).unwrap();
    env.ok(&["roots", "--add", other.to_str().unwrap()], None);

    // Resolution from the new root works with no global registry involved.
    let (success, stdout, stderr) = env.run_in(&other, &["context"], None);
    assert!(success, "resolution from added root failed: {stderr}");
    assert!(stdout.contains("rooty"));

    let out = env.ok(&["roots", "--remove", other.to_str().unwrap()], None);
    assert!(!out.contains("other-proj"), "root should be gone: {out}");

    let status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(env.home.join("rooty"))
        .output()
        .unwrap();
    assert!(status.status.success());
    assert!(
        status.stdout.is_empty(),
        "root mutations should auto-commit"
    );
}

#[test]
fn roots_update_is_atomic_when_remove_is_invalid() {
    let env = Env::new();
    env.ok(&["init", "atomic-roots"], None);
    let added = env.home.parent().unwrap().join("added-root");
    std::fs::create_dir_all(&added).unwrap();
    let missing = env.home.parent().unwrap().join("missing-root");

    let (success, _, _) = env.run(
        &[
            "roots",
            "--add",
            added.to_str().unwrap(),
            "--remove",
            missing.to_str().unwrap(),
        ],
        None,
    );
    assert!(!success);
    let config = std::fs::read_to_string(env.home.join("atomic-roots/wookie.toml")).unwrap();
    assert!(!config.contains(added.to_str().unwrap()));
}

#[test]
fn wiki_lifecycle_rename_and_remove() {
    let env = Env::new();
    env.ok(&["init", "old-name"], None);
    env.ok(&["rename-wiki", "old-name", "new-name"], None);
    let out = env.ok(&["list"], None);
    assert!(
        out.contains("new-name") && !out.contains("old-name"),
        "got: {out}"
    );
    let status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(env.home.join("new-name"))
        .output()
        .unwrap();
    assert!(status.status.success());
    assert!(
        status.stdout.is_empty(),
        "rename should auto-commit config changes"
    );

    // remove-wiki refuses without --force.
    let (success, _, stderr) = env.run(&["remove-wiki", "new-name"], None);
    assert!(!success);
    assert!(stderr.contains("--force"), "got: {stderr}");
    env.ok(&["remove-wiki", "new-name", "--force"], None);
    let out = env.ok(&["list"], None);
    assert!(out.contains("No wikis yet"), "got: {out}");
}

#[test]
fn unlock_state_lives_outside_config_and_history() {
    let env = Env::new();
    env.ok(&["init", "stately"], None);
    env.ok(&["unlock", "style"], None);
    assert!(env.home.join("stately/.unlocks.toml").exists());
    let cfg = std::fs::read_to_string(env.home.join("stately/wookie.toml")).unwrap();
    assert!(
        !cfg.contains("unlocks"),
        "unlock state leaked into wookie.toml: {cfg}"
    );
    let gi = std::fs::read_to_string(env.home.join("stately/.gitignore")).unwrap();
    assert!(
        gi.contains(".unlocks.toml"),
        "gitignore missing entry: {gi}"
    );
}

#[test]
fn ingest_on_non_git_project_says_so() {
    let env = Env::new();
    std::fs::create_dir_all(env.project.join("src")).unwrap();
    std::fs::write(env.project.join("src/a.py"), "x = 1").unwrap();
    std::fs::write(env.project.join("README.md"), "# p").unwrap();
    env.ok(&["init", "nogit"], None);
    let out = env.ok(&["ingest"], None);
    assert!(
        !out.contains("record the sync point"),
        "must not instruct --mark on non-git: {out}"
    );
    assert!(
        out.contains("not a git repo"),
        "should explain the limitation: {out}"
    );
}

#[test]
fn mcp_protocol_smoke() {
    use std::io::Write;
    let env = Env::new();
    env.ok(&["init", "mcpy"], None);

    let mut child = Command::new(env!("CARGO_BIN_EXE_wookie"))
        .arg("serve")
        .env("WOOKIE_HOME", &env.home)
        .current_dir(&env.project)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(
            concat!(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
                "\n",
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
                "\n",
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
                "\n",
                r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"wiki_list","arguments":{}}}"#,
                "\n",
                r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"session_start","arguments":{"wiki":"mcpy","agent":"codex"}}}"#,
                "\n"
            )
            .as_bytes(),
        )
        .unwrap();
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    let lines: Vec<serde_json::Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 4, "notification must get no response");
    assert_eq!(lines[0]["result"]["serverInfo"]["name"], "wookie");
    let tool_names: Vec<&str> = lines[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    for expected in [
        "session_start",
        "notify",
        "notifications",
        "notification_read",
    ] {
        assert!(
            tool_names.contains(&expected),
            "missing MCP tool {expected}"
        );
    }
    assert_eq!(lines[2]["result"]["isError"], false);
    assert!(lines[2]["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("mcpy"));
    assert_eq!(lines[3]["result"]["isError"], false);
    assert!(lines[3]["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("Started session"));

    // unlock_section without user_approved must refuse.
    let mut child = Command::new(env!("CARGO_BIN_EXE_wookie"))
        .arg("serve")
        .env("WOOKIE_HOME", &env.home)
        .current_dir(&env.project)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(
            concat!(
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"unlock_section","arguments":{"section":"style","wiki":"mcpy"}}}"#,
                "\n"
            )
            .as_bytes(),
        )
        .unwrap();
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out.stdout).lines().next().unwrap()).unwrap();
    assert_eq!(v["result"]["isError"], true);
    assert!(v["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("user_approved"));
}

#[test]
fn obsidian_print_is_side_effect_free() {
    let env = Env::new();
    env.ok(&["init", "obsi"], None);
    let out = env.ok(&["obsidian", "--print"], None);
    assert!(out.starts_with("obsidian://open?path="), "got: {out}");
    assert!(!env.home.join("obsi/pages/.obsidian").exists());

    let out = env.ok(&["obsidian", "--print", "--json"], None);
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["opened"], false);
    assert!(!env.home.join("obsi/pages/.obsidian").exists());
}

#[test]
fn ingest_and_doctor_see_dirty_and_untracked_files() {
    let env = Env::new();
    let git = |args: &[&str]| {
        assert!(Command::new("git")
            .args(args)
            .current_dir(&env.project)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success());
    };
    git(&["init", "-q"]);
    std::fs::create_dir_all(env.project.join("src")).unwrap();
    std::fs::write(env.project.join("src/main.rs"), "fn main() {}").unwrap();
    git(&["add", "-A"]);
    git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@t",
        "commit",
        "-q",
        "-m",
        "init",
    ]);

    env.ok(&["init", "dirty-ingest"], None);
    env.ok(&["ingest", "--mark"], None);
    std::fs::write(
        env.project.join("src/main.rs"),
        "fn main() { println!(\"dirty\"); }",
    )
    .unwrap();
    std::fs::write(env.project.join("src/new.rs"), "fn new_file() {}").unwrap();

    let out = env.ok(&["ingest"], None);
    assert!(
        out.contains("src/main.rs"),
        "dirty tracked file missing: {out}"
    );
    assert!(out.contains("src/new.rs"), "untracked file missing: {out}");
    assert!(!out.contains("wiki is in sync"));

    let out = env.ok(&["doctor"], None);
    assert!(out.contains("code changed since last ingest"), "got: {out}");
}

#[test]
fn doctor_reports_invalid_page_filenames() {
    let env = Env::new();
    env.ok(&["init", "bad-page"], None);
    std::fs::write(env.home.join("bad-page/pages/Bad.md"), "Body.").unwrap();
    let out = env.ok(&["doctor"], None);
    assert!(
        out.contains("invalid or unreadable page 'Bad'"),
        "got: {out}"
    );
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
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-q",
            "-m",
            "init",
        ],
        &env.project,
    );

    env.ok(&["init", "wtwiki"], None);

    // Create a linked worktree elsewhere and resolve from inside it.
    let wt = env.home.parent().unwrap().join("wt");
    git(
        &["worktree", "add", "-q", wt.to_str().unwrap()],
        &env.project,
    );
    let (success, stdout, stderr) = env.run_in(&wt, &["context"], None);
    assert!(success, "resolution from worktree failed: {stderr}");
    assert!(stdout.contains("wtwiki"));
}

#[test]
fn sessions_exchange_and_triage_notifications() {
    let env = Env::new();
    env.ok(&["init", "coord"], None);

    let first: serde_json::Value = serde_json::from_str(&env.ok(
        &[
            "session", "start", "--agent", "codex", "--label", "writer", "--json",
        ],
        None,
    ))
    .unwrap();
    let first_id = first["id"].as_str().unwrap();

    let second: serde_json::Value = serde_json::from_str(&env.ok(
        &[
            "session", "start", "--agent", "claude", "--label", "reviewer", "--json",
        ],
        None,
    ))
    .unwrap();
    let second_id = second["id"].as_str().unwrap();
    assert_ne!(first_id, second_id);

    let published: serde_json::Value = serde_json::from_str(&env.ok(
        &[
            "notify",
            "--session",
            first_id,
            "--summary",
            "Changed retry behavior",
            "--kind",
            "code-change",
            "--importance",
            "high",
            "--paths",
            "src/retry.rs,tests/retry.rs",
            "--json",
        ],
        Some("Retries now stop after three attempts."),
    ))
    .unwrap();
    let notification_id = published["notification"]["id"].as_str().unwrap();

    let out = env.ok(&["notifications", "--session", second_id], None);
    assert!(out.contains(notification_id), "notification missing: {out}");
    assert!(
        out.contains("Changed retry behavior"),
        "summary missing: {out}"
    );
    assert!(out.contains("src/retry.rs"), "paths missing: {out}");

    let own = env.ok(&["notifications", "--session", first_id], None);
    assert!(
        own.contains("No unread"),
        "sender saw its own notification: {own}"
    );

    let read = env.ok(
        &[
            "notification",
            "read",
            notification_id,
            "--session",
            second_id,
        ],
        None,
    );
    assert!(read.contains("Retries now stop after three attempts."));
    let out = env.ok(&["notifications", "--session", second_id], None);
    assert!(out.contains("No unread"), "read item repeated: {out}");
    let history = env.ok(&["notifications", "--session", second_id, "--all"], None);
    assert!(
        history.contains(notification_id),
        "history omitted read item: {history}"
    );

    let second_notice: serde_json::Value = serde_json::from_str(&env.ok(
        &[
            "notify",
            "--session",
            first_id,
            "--summary",
            "Unrelated documentation note",
            "--json",
        ],
        None,
    ))
    .unwrap();
    let second_notice_id = second_notice["notification"]["id"].as_str().unwrap();
    env.ok(
        &[
            "notification",
            "dismiss",
            second_notice_id,
            "--session",
            second_id,
        ],
        None,
    );
    let out = env.ok(&["notifications", "--session", second_id], None);
    assert!(out.contains("No unread"), "dismissed item repeated: {out}");

    env.ok(&["session", "close", first_id], None);
    let (success, _, stderr) = env.run(
        &["notify", "--session", first_id, "--summary", "Should fail"],
        None,
    );
    assert!(!success);
    assert!(stderr.contains("closed"), "unexpected error: {stderr}");

    // Reading and dismissing only change gitignored inbox state.
    let status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(env.home.join("coord"))
        .output()
        .unwrap();
    assert!(status.status.success());
    assert!(status.stdout.is_empty(), "session wiki should be clean");
}

#[test]
fn new_session_starts_caught_up_but_can_inspect_history() {
    let env = Env::new();
    env.ok(&["init", "history"], None);
    let sender: serde_json::Value =
        serde_json::from_str(&env.ok(&["session", "start", "--agent", "codex", "--json"], None))
            .unwrap();
    let sender_id = sender["id"].as_str().unwrap();
    let notice: serde_json::Value = serde_json::from_str(&env.ok(
        &[
            "notify",
            "--session",
            sender_id,
            "--summary",
            "Predates receiver",
            "--json",
        ],
        None,
    ))
    .unwrap();
    let notice_id = notice["notification"]["id"].as_str().unwrap();

    let receiver: serde_json::Value =
        serde_json::from_str(&env.ok(&["session", "start", "--agent", "claude", "--json"], None))
            .unwrap();
    let receiver_id = receiver["id"].as_str().unwrap();
    let unread = env.ok(&["notifications", "--session", receiver_id], None);
    assert!(
        unread.contains("No unread"),
        "old history flooded new session: {unread}"
    );
    let history = env.ok(&["notifications", "--session", receiver_id, "--all"], None);
    assert!(
        history.contains(notice_id),
        "old history unavailable: {history}"
    );
}
