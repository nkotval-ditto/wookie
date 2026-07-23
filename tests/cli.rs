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

    fn run_with_env(
        &self,
        args: &[&str],
        stdin: Option<&str>,
        vars: &[(&str, &str)],
    ) -> (bool, String, String) {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_wookie"));
        cmd.args(args)
            .env("WOOKIE_HOME", &self.home)
            .envs(vars.iter().copied())
            .current_dir(&self.project)
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

    fn ok_with_env(&self, args: &[&str], stdin: Option<&str>, vars: &[(&str, &str)]) -> String {
        let (success, stdout, stderr) = self.run_with_env(args, stdin, vars);
        assert!(success, "wookie {args:?} failed:\n{stdout}\n{stderr}");
        stdout
    }

    fn add_required_audit_pages(&self) {
        self.ok(
            &["new", "architecture/overview"],
            Some("**The architecture is documented.** See [[index]]."),
        );
        let checks = "**These checks make the rules executable.** See [[index]].\n\n## Scope\n\nAll changes.\n\n## Procedure\n\nReview the diff.\n\n## Violations\n\nA missed check.\n\n## Exceptions\n\nNone.";
        self.ok(&["unlock", "style"], None);
        self.ok(&["new", "style/checks"], Some(checks));
        self.ok(&["lock", "style"], None);
        self.ok(&["unlock", "workflow"], None);
        self.ok(&["new", "workflow/checks"], Some(checks));
        self.ok(&["lock", "workflow"], None);
    }
}

fn init_git_repository(path: &Path, marker: &str) {
    std::fs::create_dir_all(path).unwrap();
    let run = |args: &[&str]| {
        let output = Command::new("git")
            .arg("-C")
            .arg(path)
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
    };
    run(&["init", "-q"]);
    std::fs::write(path.join("marker.txt"), marker).unwrap();
    run(&["add", "marker.txt"]);
    run(&["commit", "-q", "-m", "initial"]);
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
fn expand_bounds_only_the_worklist_not_stub_creation() {
    let env = Env::new();
    env.ok(&["init", "bounded-expand"], None);
    let links = (0..30)
        .map(|index| format!("[[guides/generated-{index:02}]]"))
        .collect::<Vec<_>>()
        .join(" ");
    let body = format!("**Expansion source.**\n\n{links}");
    env.ok(&["new", "guides/source"], Some(&body));

    let raw = env.ok(
        &["--json", "expand", "--limit", "2", "--tokens", "1000"],
        None,
    );
    let result: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(result["schema"], "wookie.report/v1");
    assert_eq!(result["command"], "expand");
    assert_eq!(result["totals"]["created"], 30);
    assert_eq!(result["totals"]["stubs"], 30);
    assert!(result["created"].as_array().unwrap().len() <= 2);
    assert!(result["stubs"].as_array().unwrap().len() <= 2);
    assert!(result["omissions"]["created"].as_u64().unwrap() > 0);
    assert!(result["omissions"]["stubs"].as_u64().unwrap() > 0);
    assert_eq!(result["telemetry"]["budget_tokens"], 1000);
    assert!(raw.len().div_ceil(3) <= 1000);
    assert_eq!(result["continuation"]["command"], "wookie expand --all");

    // IDs omitted from the response were still created and remain directly
    // reachable. The exhaustive follow-up lists the complete current worklist.
    let last = env.ok(&["read", "guides/generated-29"], None);
    assert!(last.contains("TODO: define"));
    let exhaustive_raw = env.ok(&["--json", "expand", "--all"], None);
    let exhaustive: serde_json::Value = serde_json::from_str(&exhaustive_raw).unwrap();
    assert_eq!(exhaustive["totals"]["created"], 0);
    assert_eq!(exhaustive["totals"]["stubs"], 30);
    assert_eq!(exhaustive["stubs"].as_array().unwrap().len(), 30);
    assert_eq!(exhaustive["omissions"]["stubs"], 0);
    assert_eq!(exhaustive["telemetry"]["all"], true);
}

#[test]
fn expand_rejects_an_unsafe_budget_before_writing() {
    let env = Env::new();
    env.ok(&["init", "expand-budget-preflight"], None);
    env.ok(
        &["new", "guides/source"],
        Some("**Expansion source.**\n\nSee [[guides/not-created]]."),
    );

    let (success, _, stderr) = env.run(&["expand", "--tokens", "255"], None);
    assert!(!success);
    assert!(
        stderr.contains("at least 256"),
        "unexpected stderr: {stderr}"
    );
    let (success, _, stderr) = env.run(&["expand", "--tokens", "1000001"], None);
    assert!(!success);
    assert!(
        stderr.contains("must not exceed 1000000"),
        "unexpected stderr: {stderr}"
    );
    let (exists, _, _) = env.run(&["read", "guides/not-created"], None);
    assert!(
        !exists,
        "budget validation must happen before stub creation"
    );
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

    // Complete the seeded worklist and required audit workflows. A mark is
    // receipt-bound and refuses stubs or missing rule checks.
    for (id, source) in [
        ("code/docs", "docs/"),
        ("code/src", "src/"),
        ("code/src/scheduler", "src/scheduler/"),
    ] {
        env.ok(
            &["write", id],
            Some(&format!(
                "**The {id} module is documented.** It is part of [[index]].\n\nFile: `{source}`\n\n## Role\n\nTest fixture documentation."
            )),
        );
    }
    env.add_required_audit_pages();

    let final_worklist: serde_json::Value =
        serde_json::from_str(&env.ok(&["--json", "ingest", "--full"], None)).unwrap();
    let receipt = final_worklist["data"]["worklist_receipt"].as_str().unwrap();
    env.ok(
        &["ingest", "--mark", "--expect-worklist", receipt, "--full"],
        None,
    );
    // Confirm a no-change update reports in-sync.
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
        Some(
            "**Naming rules govern function identifiers.** Read the detailed rule before review.\n\nFunctions are snake_case, no abbreviations.",
        ),
    );

    // An uncommitted change is the default target.
    std::fs::write(env.project.join("main.rs"), "fn main() { let x = 1; }").unwrap();

    let compact = env.ok(&["critique"], None);
    assert!(
        compact.contains("main.rs"),
        "target file missing: {compact}"
    );
    assert!(
        compact.contains("wookie read style/naming"),
        "compact briefing lacks exact continuation: {compact}"
    );
    assert!(
        !compact.contains("snake_case, no abbreviations"),
        "compact briefing leaked a complete rule body: {compact}"
    );

    let out = env.ok(&["critique", "--all"], None);
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
        out.contains("ERROR: workflow/checks is missing"),
        "workflow section without checks should be noted: {out}"
    );

    let json: serde_json::Value =
        serde_json::from_str(&env.ok(&["critique", "--json"], None)).unwrap();
    assert_eq!(json["data"]["output_mode"], "compact");
    assert!(json["data"]["omissions"]["rule_bodies"].as_u64().unwrap() >= 1);
    let naming = json["data"]["rules"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|section| section["rules"].as_array().unwrap())
        .find(|rule| rule["id"] == "style/naming")
        .unwrap();
    assert!(naming.get("body").is_none());
    assert_eq!(naming["read_command"], "wookie read style/naming");
    assert!(json["diagnostics"].as_array().unwrap().iter().any(|item| {
        item["code"] == "missing_checks"
            && item["severity"] == "error"
            && item["page"] == "workflow/checks"
    }));
    let argv = json["data"]["diff_argv"]
        .as_array()
        .expect("critique JSON exposes structured Git argv");
    assert_eq!(argv[0], "git");
    assert!(argv.iter().any(|arg| arg == "--"));
    let legacy = json["data"]["diff_command"]
        .as_str()
        .expect("legacy command field is a JSON argv string");
    assert_eq!(
        serde_json::from_str::<Vec<String>>(legacy).unwrap(),
        argv.iter()
            .map(|arg| arg.as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    );

    // Moving names are resolved once. The exact target used to select files is
    // carried into both the inspection argv and the machine-readable snapshot.
    let head = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&env.project)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    let revision_json: serde_json::Value =
        serde_json::from_str(&env.ok(&["critique", "--revision", "HEAD", "--json"], None)).unwrap();
    assert_eq!(revision_json["data"]["target_revision"], head);
    assert_eq!(
        revision_json["data"]["base_revision"],
        serde_json::Value::Null
    );
    assert_eq!(revision_json["snapshot"]["project"]["revision"], head);
    let revision_argv = revision_json["data"]["diff_argv"].as_array().unwrap();
    assert!(revision_argv.iter().any(|arg| arg == head.as_str()));
    assert!(!revision_argv.iter().any(|arg| arg == "HEAD"));

    let (success, _, stderr) = env.run(&["critique", "--tokens", "256"], None);
    assert!(!success, "undersized critique budget unexpectedly passed");
    assert!(
        stderr.contains("exceeding the 256-token budget"),
        "unexpected budget error: {stderr}"
    );
}

#[test]
fn critique_rejects_control_characters_in_explicit_paths() {
    let env = Env::new();
    env.ok(&["init", "safe-paths"], None);

    let (success, _, stderr) = env.run(&["critique", "--paths", "bad\npath.rs"], None);
    assert!(!success, "control-bearing path must be rejected");
    assert!(stderr.contains("control character"), "{stderr}");
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
    assert!(env.home.join("stately/.unlocks/style.toml").exists());
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
                "\n",
                r#"{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"config_get","arguments":{"wiki":"mcpy","key":"sessions.poll_limit","effective":true}}}"#,
                "\n",
                r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"publish_recovery_status","arguments":{"wiki":"mcpy"}}}"#,
                "\n",
                r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"publish_recover","arguments":{"wiki":"mcpy","action":"discard"}}}"#,
                "\n",
                r#"{"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"publish_recovery_status","arguments":{"wiki":"mcpy","unexpected":true}}}"#,
                "\n",
                r#"{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":"wiki_prime","arguments":{"wiki":"mcpy","query":"index","tokens":1500}}}"#,
                "\n",
                r#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"search","arguments":{"wiki":"mcpy","query":"index","tokens":512}}}"#,
                "\n",
                r#"{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"publish","arguments":{"wiki":"mcpy","manifest":"{\"schema\":\"wookie.changeset/v1\",\"changes\":[]}"}}}"#,
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
    assert_eq!(lines.len(), 11, "notification must get no response");
    assert_eq!(lines[0]["result"]["serverInfo"]["name"], "wookie");
    let tool_names: Vec<&str> = lines[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    for expected in [
        "session_start",
        "session_list",
        "session_show",
        "session_heartbeat",
        "session_close",
        "session_prune",
        "notify",
        "notifications",
        "notification_read",
        "notification_dismiss",
        "config_show",
        "config_get",
        "config_set",
        "config_unset",
        "config_keys",
        "publish_recovery_status",
        "publish_recover",
    ] {
        assert!(
            tool_names.contains(&expected),
            "missing MCP tool {expected}"
        );
    }
    let notifications_tool = lines[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "notifications")
        .unwrap();
    let notification_properties = &notifications_tool["inputSchema"]["properties"];
    assert_eq!(notification_properties["offset"]["minimum"], 0);
    assert_eq!(notification_properties["newest_first"]["default"], true);
    assert_eq!(
        notification_properties["lookback_hours"]["maximum"],
        878_400
    );
    let session_start = lines[1]["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "session_start")
        .unwrap();
    assert_eq!(
        session_start["inputSchema"]["properties"]["lookback_hours"]["maximum"],
        878_400
    );
    assert_eq!(lines[2]["result"]["isError"], false);
    assert_eq!(
        lines[2]["result"]["content"][0]["text"],
        "Structured result available in structuredContent."
    );
    assert_eq!(
        lines[2]["result"]["structuredContent"]["wikis"][0]["slug"],
        "mcpy"
    );
    assert_eq!(lines[3]["result"]["isError"], false);
    assert_eq!(
        lines[3]["result"]["content"][0]["text"],
        "Structured result available in structuredContent."
    );
    assert_eq!(
        lines[3]["result"]["structuredContent"]["session"]["agent"],
        "codex"
    );
    assert_eq!(lines[4]["result"]["isError"], false);
    assert_eq!(lines[4]["result"]["content"][0]["text"], "100");
    assert_eq!(lines[4]["result"]["structuredContent"]["value"], 100);
    assert_eq!(lines[5]["result"]["isError"], false);
    assert_eq!(
        lines[5]["result"]["structuredContent"]["schema"],
        "wookie.publish-recovery-status/v1"
    );
    assert_eq!(
        lines[5]["result"]["structuredContent"]["recovery_required"],
        false
    );
    assert!(lines[5]["result"]["structuredContent"]["recovery"].is_null());
    assert_eq!(lines[6]["result"]["isError"], true);
    assert!(lines[6]["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("unsupported value"));
    assert_eq!(lines[7]["result"]["isError"], true);
    assert!(lines[7]["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("unknown tool argument 'unexpected'"));
    for (index, schema) in [
        (8, "wookie.prime/v1"),
        (9, "wookie.search/v1"),
        (10, "wookie.publish-preview/v1"),
    ] {
        assert_eq!(
            lines[index]["result"]["isError"], false,
            "{schema} MCP call failed: {}",
            lines[index]["result"]["content"]
        );
        assert_eq!(
            lines[index]["result"]["structuredContent"]["schema"],
            schema
        );
        assert_eq!(
            lines[index]["result"]["content"][0]["text"],
            "Structured result available in structuredContent."
        );
        assert_ne!(
            lines[index]["result"]["content"][0]["text"],
            lines[index]["result"]["structuredContent"].to_string(),
            "bounded {schema} payload must not be duplicated in MCP text content"
        );
    }

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
                "\n",
                r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"session_start","arguments":{"wiki":"mcpy","lookback_hours":18446744073709551615}}}"#,
                "\n"
            )
            .as_bytes(),
        )
        .unwrap();
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    let errors: Vec<serde_json::Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(errors.len(), 2);
    assert_eq!(errors[0]["result"]["isError"], true);
    assert!(errors[0]["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("user_approved"));
    assert_eq!(errors[1]["result"]["isError"], true);
    let oversized_lookback = errors[1]["result"]["content"][0]["text"].as_str().unwrap();
    assert!(
        oversized_lookback.contains("at most 878400") || oversized_lookback.contains("too large"),
        "unexpected error: {oversized_lookback}"
    );
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
    env.ok(&["ingest", "--full"], None);
    env.ok(
        &["write", "code/src"],
        Some(
            "**The source module is documented.** See [[index]].\n\nFile: `src/`\n\n## Role\n\nIt contains the executable.",
        ),
    );
    env.add_required_audit_pages();
    let worklist: serde_json::Value =
        serde_json::from_str(&env.ok(&["--json", "ingest", "--full"], None)).unwrap();
    let receipt = worklist["data"]["worklist_receipt"].as_str().unwrap();
    env.ok(
        &["ingest", "--mark", "--expect-worklist", receipt, "--full"],
        None,
    );
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
    let (success, _, out) = env.run(&["doctor"], None);
    assert!(!success, "strict catalog capture accepted an invalid id");
    assert!(
        out.contains("page id 'Bad' must be lowercase"),
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
fn explicit_wiki_never_captures_git_context_from_an_unrelated_cwd() {
    let env = Env::new();
    init_git_repository(&env.project, "registered");
    env.ok(&["init", "context-bound"], None);
    let session = env
        .ok(&["session", "start", "--id-only"], None)
        .trim()
        .to_string();

    let unrelated = env.home.parent().unwrap().join("unrelated-project");
    init_git_repository(&unrelated, "unrelated");
    let (success, stdout, stderr) = env.run_in(
        &unrelated,
        &[
            "--wiki",
            "context-bound",
            "--json",
            "notify",
            "--session",
            &session,
            "--summary",
            "outside invocation",
        ],
        None,
    );
    assert!(success, "explicit-wiki notify failed: {stdout}\n{stderr}");
    let outside: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(
        outside["notification"].get("git").is_none(),
        "unrelated repository context leaked into notification: {outside}"
    );

    let inside: serde_json::Value = serde_json::from_str(&env.ok(
        &[
            "--json",
            "notify",
            "--session",
            &session,
            "--summary",
            "inside invocation",
        ],
        None,
    ))
    .unwrap();
    let git = &inside["notification"]["git"];
    assert!(
        git.is_object(),
        "registered project context missing: {inside}"
    );
    assert_eq!(
        PathBuf::from(git["worktree"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        env.project.canonicalize().unwrap()
    );
}

#[test]
fn notification_polling_is_newest_first_and_reports_continuations() {
    let env = Env::new();
    env.ok(&["init", "notification-pages"], None);
    let sender = env
        .ok(
            &["session", "start", "--agent", "sender", "--id-only"],
            None,
        )
        .trim()
        .to_string();
    let receiver = env
        .ok(
            &["session", "start", "--agent", "receiver", "--id-only"],
            None,
        )
        .trim()
        .to_string();

    let mut ids = Vec::new();
    for summary in ["oldest", "middle", "newest blocker"] {
        let published: serde_json::Value = serde_json::from_str(&env.ok(
            &[
                "notify",
                "--session",
                &sender,
                "--summary",
                summary,
                "--json",
            ],
            None,
        ))
        .unwrap();
        ids.push(
            published["notification"]["id"]
                .as_str()
                .unwrap()
                .to_string(),
        );
    }
    env.ok(&["config", "set", "sessions.poll_limit", "1"], None);

    let first: serde_json::Value =
        serde_json::from_str(&env.ok(&["notifications", "--session", &receiver, "--json"], None))
            .unwrap();
    assert_eq!(first["newest_first"], true);
    assert_eq!(first["offset"], 0);
    assert_eq!(first["total"], 3);
    assert_eq!(first["returned"], 1);
    assert_eq!(first["omitted"], 2);
    assert_eq!(first["notifications"][0]["notification"]["id"], ids[2]);
    assert_eq!(first["continuation"]["offset"], 1);
    assert_eq!(first["continuation"]["limit"], 1);
    assert_eq!(first["continuation"]["remaining"], 2);

    let second: serde_json::Value = serde_json::from_str(&env.ok(
        &[
            "notifications",
            "--session",
            &receiver,
            "--limit",
            "1",
            "--offset",
            "1",
            "--json",
        ],
        None,
    ))
    .unwrap();
    assert_eq!(second["notifications"][0]["notification"]["id"], ids[1]);
    assert_eq!(second["continuation"]["offset"], 2);
    assert_eq!(second["continuation"]["remaining"], 1);

    let oldest: serde_json::Value = serde_json::from_str(&env.ok(
        &[
            "notifications",
            "--session",
            &receiver,
            "--limit",
            "1",
            "--oldest-first",
            "--json",
        ],
        None,
    ))
    .unwrap();
    assert_eq!(oldest["newest_first"], false);
    assert_eq!(oldest["notifications"][0]["notification"]["id"], ids[0]);

    let human = env.ok(
        &["notifications", "--session", &receiver, "--limit", "1"],
        None,
    );
    assert!(human.contains("Showing 1 of 3"), "missing count: {human}");
    assert!(
        human.contains("plus --offset 1 (2 remaining)"),
        "missing continuation: {human}"
    );

    let (success, _, stderr) = env.run(
        &["notifications", "--session", &receiver, "--offset", "4"],
        None,
    );
    assert!(!success);
    assert!(
        stderr.contains("offset 4 exceeds"),
        "unexpected error: {stderr}"
    );

    let (success, _, stderr) = env.run(
        &["notifications", "--session", &receiver, "--limit", "0"],
        None,
    );
    assert!(!success);
    assert!(stderr.contains("between 1"), "unexpected error: {stderr}");
}

#[test]
fn mcp_notification_polling_uses_the_same_safe_paging_contract() {
    use std::io::Write;

    let env = Env::new();
    env.ok(&["init", "mcp-notification-pages"], None);
    let sender = env
        .ok(
            &["session", "start", "--agent", "sender", "--id-only"],
            None,
        )
        .trim()
        .to_string();
    let receiver = env
        .ok(
            &["session", "start", "--agent", "receiver", "--id-only"],
            None,
        )
        .trim()
        .to_string();
    let mut ids = Vec::new();
    for summary in ["older", "fresh blocker"] {
        let published: serde_json::Value = serde_json::from_str(&env.ok(
            &[
                "notify",
                "--session",
                &sender,
                "--summary",
                summary,
                "--json",
            ],
            None,
        ))
        .unwrap();
        ids.push(
            published["notification"]["id"]
                .as_str()
                .unwrap()
                .to_string(),
        );
    }
    env.ok(&["config", "set", "sessions.poll_limit", "1"], None);

    let requests = [
        serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "notifications", "arguments": {
                "wiki": "mcp-notification-pages", "session": receiver
            }}
        }),
        serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": {"name": "notifications", "arguments": {
                "wiki": "mcp-notification-pages", "session": receiver, "limit": 1, "offset": 1
            }}
        }),
        serde_json::json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": {"name": "notifications", "arguments": {
                "wiki": "mcp-notification-pages", "session": receiver,
                "limit": 1, "newest_first": false
            }}
        }),
        serde_json::json!({
            "jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": {"name": "notifications", "arguments": {
                "wiki": "mcp-notification-pages", "session": receiver, "offset": 3
            }}
        }),
        serde_json::json!({
            "jsonrpc": "2.0", "id": 5, "method": "tools/call",
            "params": {"name": "notifications", "arguments": {
                "wiki": "mcp-notification-pages", "session": receiver, "limit": 0
            }}
        }),
    ];
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
    for request in requests {
        writeln!(stdin, "{request}").unwrap();
    }
    drop(stdin);
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    let lines: Vec<serde_json::Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 5);

    let first = &lines[0]["result"]["structuredContent"];
    assert_eq!(first["newest_first"], true);
    assert_eq!(first["total"], 2);
    assert_eq!(first["returned"], 1);
    assert_eq!(first["omitted"], 1);
    assert_eq!(first["notifications"][0]["notification"]["id"], ids[1]);
    assert_eq!(first["continuation"]["offset"], 1);
    assert_eq!(first["continuation"]["limit"], 1);

    let second = &lines[1]["result"]["structuredContent"];
    assert_eq!(second["notifications"][0]["notification"]["id"], ids[0]);
    assert!(second.get("continuation").is_none());

    let oldest = &lines[2]["result"]["structuredContent"];
    assert_eq!(oldest["newest_first"], false);
    assert_eq!(oldest["notifications"][0]["notification"]["id"], ids[0]);

    assert_eq!(lines[3]["result"]["isError"], true);
    assert!(lines[3]["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("offset 3 exceeds"));
    assert_eq!(lines[4]["result"]["isError"], true);
    assert!(lines[4]["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("at least 1"));
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

#[test]
fn notification_scan_rejects_frontmatter_that_disagrees_with_storage_path() {
    let env = Env::new();
    env.ok(&["init", "notification-integrity"], None);
    let sender = env
        .ok(
            &["session", "start", "--agent", "sender", "--id-only"],
            None,
        )
        .trim()
        .to_string();
    let receiver = env
        .ok(
            &["session", "start", "--agent", "receiver", "--id-only"],
            None,
        )
        .trim()
        .to_string();
    let publish = |summary: &str| -> String {
        let result: serde_json::Value = serde_json::from_str(&env.ok(
            &[
                "notify",
                "--session",
                &sender,
                "--summary",
                summary,
                "--no-git-context",
                "--json",
            ],
            None,
        ))
        .unwrap();
        result["notification"]["id"].as_str().unwrap().to_string()
    };
    let valid_id = publish("valid notification");
    let wrong_id_file = publish("frontmatter id mismatch");
    let wrong_source_file = publish("frontmatter source mismatch");
    let notification_dir = env
        .home
        .join("notification-integrity/sessions")
        .join(&sender)
        .join("notifications");

    let wrong_id_path = notification_dir.join(format!("{wrong_id_file}.md"));
    let raw = std::fs::read_to_string(&wrong_id_path).unwrap();
    let altered = raw.replacen(
        &format!("id = \"{wrong_id_file}\""),
        "id = \"notify-20000101-000000-deadbeef\"",
        1,
    );
    assert_ne!(raw, altered);
    std::fs::write(&wrong_id_path, altered).unwrap();

    let wrong_source_path = notification_dir.join(format!("{wrong_source_file}.md"));
    let raw = std::fs::read_to_string(&wrong_source_path).unwrap();
    let altered = raw.replacen(
        &format!("source_session = \"{sender}\""),
        &format!("source_session = \"{receiver}\""),
        1,
    );
    assert_ne!(raw, altered);
    std::fs::write(&wrong_source_path, altered).unwrap();

    let inbox: serde_json::Value =
        serde_json::from_str(&env.ok(&["notifications", "--session", &receiver, "--json"], None))
            .unwrap();
    assert_eq!(inbox["notifications"].as_array().unwrap().len(), 1);
    assert_eq!(inbox["notifications"][0]["notification"]["id"], valid_id);
    assert_eq!(inbox["warnings_total"], 2);
    assert_eq!(inbox["warnings_omitted"], 0);
    let warnings = inbox["warnings"].as_array().unwrap();
    assert_eq!(
        warnings.len(),
        2,
        "integrity failures must be warnings: {inbox}"
    );
    assert!(warnings.iter().any(|warning| warning["path"]
        .as_str()
        .is_some_and(|path| path.ends_with(&format!("{wrong_id_file}.md")))));
    assert!(warnings.iter().any(|warning| warning["path"]
        .as_str()
        .is_some_and(|path| path.ends_with(&format!("{wrong_source_file}.md")))));
}

#[test]
fn session_listing_isolates_corrupt_metadata_and_activity() {
    let env = Env::new();
    env.ok(&["init", "corrupt-sessions"], None);
    let valid = env
        .ok(&["session", "start", "--agent", "valid", "--id-only"], None)
        .trim()
        .to_string();
    let corrupt = env
        .ok(
            &["session", "start", "--agent", "corrupt", "--id-only"],
            None,
        )
        .trim()
        .to_string();
    let sessions = env.home.join("corrupt-sessions/sessions");

    let corrupt_path = sessions.join(&corrupt).join("session.toml");
    let mut metadata: toml::Value =
        toml::from_str(&std::fs::read_to_string(&corrupt_path).unwrap()).unwrap();
    metadata["id"] = toml::Value::String("session-20000101-000000-deadbeef".into());
    std::fs::write(&corrupt_path, toml::to_string_pretty(&metadata).unwrap()).unwrap();

    let activity = sessions.join(&valid).join("activity");
    std::fs::create_dir(&activity).unwrap();
    std::fs::write(
        activity.join("activity-20000101-000000-deadbeef.toml"),
        "not valid toml = [",
    )
    .unwrap();

    let listed: serde_json::Value =
        serde_json::from_str(&env.ok(&["session", "list", "--json"], None)).unwrap();
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 1);
    assert_eq!(listed["sessions"][0]["id"], valid);
    let warnings = listed["warnings"].as_array().unwrap();
    assert_eq!(warnings.len(), 2, "unexpected warning set: {listed}");
    assert!(warnings.iter().any(|warning| warning["message"]
        .as_str()
        .is_some_and(|message| message.contains("id does not match"))));
    assert!(warnings.iter().any(|warning| warning["path"]
        .as_str()
        .is_some_and(|path| path.contains("activity-20000101"))));

    let (success, _, stderr) = env.run(&["session", "show", &corrupt], None);
    assert!(!success);
    assert!(stderr.contains("id does not match"), "got: {stderr}");
}

#[test]
fn session_list_and_show_are_bounded_and_continuable() {
    let env = Env::new();
    env.ok(&["init", "bounded-sessions"], None);
    let mut ids = Vec::new();
    for agent in ["one", "two", "three"] {
        ids.push(
            env.ok(&["session", "start", "--agent", agent, "--id-only"], None)
                .trim()
                .to_string(),
        );
    }

    let first: serde_json::Value =
        serde_json::from_str(&env.ok(&["session", "list", "--limit", "1", "--json"], None))
            .unwrap();
    assert_eq!(first["total_matches"], 3);
    assert_eq!(first["returned"], 1);
    assert_eq!(first["omitted"], 2);
    assert_eq!(first["continuation"], 1);
    assert_eq!(first["scan_complete"], true);

    let second: serde_json::Value = serde_json::from_str(&env.ok(
        &["session", "list", "--limit", "2", "--cursor", "1", "--json"],
        None,
    ))
    .unwrap();
    assert_eq!(second["returned"], 2);
    assert_eq!(second["omitted"], 0);
    assert!(second.get("continuation").is_none());

    for index in 0..3 {
        env.ok(
            &[
                "notify",
                "--session",
                &ids[0],
                "--summary",
                &format!("notice {index}"),
            ],
            None,
        );
    }
    let shown: serde_json::Value = serde_json::from_str(&env.ok(
        &["session", "show", &ids[0], "--limit", "1", "--json"],
        None,
    ))
    .unwrap();
    assert_eq!(shown["total_notifications_sent"], 3);
    assert_eq!(shown["notifications_returned"], 1);
    assert_eq!(shown["notifications_omitted"], 2);
    assert_eq!(shown["continuation"], 1);
    assert!(shown["notifications_sent"][0].get("paths").is_none());
}

#[test]
fn activity_events_are_applied_in_timestamp_order_across_offsets() {
    let env = Env::new();
    env.ok(&["init", "activity-order"], None);
    let session = env
        .ok(&["session", "start", "--id-only"], None)
        .trim()
        .to_string();
    let session_dir = env.home.join("activity-order/sessions").join(&session);
    let session_path = session_dir.join("session.toml");
    let mut metadata: toml::Value =
        toml::from_str(&std::fs::read_to_string(&session_path).unwrap()).unwrap();
    for key in ["created_at", "updated_at", "last_seen_at"] {
        metadata[key] = toml::Value::String("2025-12-31T00:00:00Z".into());
    }
    std::fs::write(&session_path, toml::to_string_pretty(&metadata).unwrap()).unwrap();

    let activity = session_dir.join("activity");
    std::fs::create_dir(&activity).unwrap();
    std::fs::write(
        activity.join("activity-20000101-000000-00000001.toml"),
        concat!(
            "id = \"activity-20000101-000000-00000001\"\n",
            "at = \"2026-01-01T01:00:00+02:00\"\n",
            "action = \"earlier instant\"\n",
            "status = \"active\"\n",
        ),
    )
    .unwrap();
    std::fs::write(
        activity.join("activity-20000101-000000-00000002.toml"),
        concat!(
            "id = \"activity-20000101-000000-00000002\"\n",
            "at = \"2026-01-01T00:30:00Z\"\n",
            "action = \"later instant\"\n",
            "status = \"closed\"\n",
        ),
    )
    .unwrap();

    let shown: serde_json::Value =
        serde_json::from_str(&env.ok(&["session", "show", &session, "--json"], None)).unwrap();
    assert_eq!(shown["session"]["status"], "closed");
    assert_eq!(shown["session"]["last_seen_at"], "2026-01-01T00:30:00Z");
}

#[test]
fn configuration_supports_sparse_overrides_inheritance_and_validation() {
    let env = Env::new();
    env.ok(&["init", "configurable"], None);

    env.ok(&["config", "set", "sessions.poll_limit", "7"], None);
    assert_eq!(
        env.ok(
            &["config", "get", "sessions.poll_limit", "--effective"],
            None
        )
        .trim(),
        "7"
    );
    let stored = std::fs::read_to_string(env.home.join("configurable/wookie.toml")).unwrap();
    assert!(stored.contains("poll_limit = 7"));
    assert!(
        !stored.contains("max_body_bytes"),
        "one override should not materialize the entire defaults block: {stored}"
    );

    env.ok(
        &[
            "config",
            "set",
            "defaults.sessions.default_kind",
            "decision",
            "--global",
        ],
        None,
    );
    assert_eq!(
        env.ok(
            &["config", "get", "sessions.default_kind", "--effective"],
            None
        )
        .trim(),
        "decision"
    );

    env.ok(
        &[
            "config",
            "set",
            "defaults.sessions.retention_days",
            "30",
            "--global",
        ],
        None,
    );
    assert_eq!(
        env.ok(
            &["config", "get", "sessions.retention_days", "--effective"],
            None,
        )
        .trim(),
        "30"
    );
    env.ok(&["config", "set", "sessions.retention_days", "0"], None);
    let effective: serde_json::Value =
        serde_json::from_str(&env.ok(&["config", "show", "--effective", "--json"], None)).unwrap();
    assert!(
        effective["sessions"].get("retention_days").is_none(),
        "zero wiki retention must disable the inherited value: {effective}"
    );
    env.ok(&["config", "unset", "sessions.retention_days"], None);
    assert_eq!(
        env.ok(
            &["config", "get", "sessions.retention_days", "--effective"],
            None,
        )
        .trim(),
        "30"
    );

    env.ok(
        &[
            "config",
            "set",
            "defaults.history.lock_stale_seconds",
            "120",
            "--global",
        ],
        None,
    );
    env.ok(&["config", "set", "history.lock_timeout_ms", "7000"], None);
    assert_eq!(
        env.ok(
            &["config", "get", "history.lock_timeout_ms", "--effective"],
            None,
        )
        .trim(),
        "7000"
    );
    assert_eq!(
        env.ok(
            &["config", "get", "history.lock_stale_seconds", "--effective",],
            None,
        )
        .trim(),
        "120"
    );
    let stored = std::fs::read_to_string(env.home.join("configurable/wookie.toml")).unwrap();
    assert!(stored.contains("lock_timeout_ms = 7000"));
    assert!(
        !stored.contains("lock_stale_seconds"),
        "history overrides must remain sparse: {stored}"
    );

    let before = std::fs::read_to_string(env.home.join("configurable/wookie.toml")).unwrap();
    let (success, _, stderr) = env.run(&["config", "set", "sessions.poll_limit", "0"], None);
    assert!(!success);
    assert!(stderr.contains("greater than zero"));
    assert_eq!(
        before,
        std::fs::read_to_string(env.home.join("configurable/wookie.toml")).unwrap()
    );

    let (success, _, stderr) = env.run(&["config", "set", "sessions.poll_limit", "10001"], None);
    assert!(!success);
    assert!(stderr.contains("no greater than 10000"), "{stderr}");
    assert_eq!(
        before,
        std::fs::read_to_string(env.home.join("configurable/wookie.toml")).unwrap(),
        "an over-ceiling session resource limit must not persist"
    );

    let (success, _, stderr) = env.run(&["config", "set", "name", "other"], None);
    assert!(!success);
    assert!(stderr.contains("dedicated command"));
    let (success, _, stderr) = env.run(&["config", "set", "sections.style.locked", "false"], None);
    assert!(!success);
    assert!(stderr.contains("user-approved"));
    env.ok(
        &[
            "config",
            "set",
            "sections.style.locked",
            "false",
            "--user-approved",
        ],
        None,
    );

    env.ok(&["config", "unset", "sessions.poll_limit"], None);
    assert_eq!(
        env.ok(
            &["config", "get", "sessions.poll_limit", "--effective"],
            None
        )
        .trim(),
        "100"
    );
}

#[test]
fn configuration_exposes_retrieval_audit_and_publish_controls() {
    let env = Env::new();
    env.ok(&["init", "feature-config"], None);

    let global_set: serde_json::Value = serde_json::from_str(&env.ok(
        &[
            "config",
            "set",
            "defaults.retrieval.search_tokens",
            "2500",
            "--global",
            "--json",
        ],
        None,
    ))
    .unwrap();
    assert_eq!(global_set["value"], 2500);
    let number_set: serde_json::Value = serde_json::from_str(&env.ok(
        &["config", "set", "retrieval.search_limit", "4", "--json"],
        None,
    ))
    .unwrap();
    assert_eq!(number_set["value"], 4);
    let bool_set: serde_json::Value = serde_json::from_str(&env.ok(
        &[
            "config",
            "set",
            "audit.source_provenance",
            "false",
            "--json",
        ],
        None,
    ))
    .unwrap();
    assert_eq!(bool_set["value"], false);
    let array_set: serde_json::Value = serde_json::from_str(&env.ok(
        &[
            "config",
            "set",
            "sections.operator.required",
            "[\"overview\", \"checks\"]",
            "--user-approved",
            "--json",
        ],
        None,
    ))
    .unwrap();
    assert_eq!(
        array_set["value"],
        serde_json::json!(["overview", "checks"])
    );
    let string_set: serde_json::Value = serde_json::from_str(&env.ok(
        &["config", "set", "description", "42", "--string", "--json"],
        None,
    ))
    .unwrap();
    assert_eq!(string_set["value"], "42");
    env.ok(&["config", "set", "publish.orphan_policy", "error"], None);
    env.ok(&["config", "set", "publish.output_tokens", "512"], None);

    let effective: serde_json::Value =
        serde_json::from_str(&env.ok(&["config", "show", "--effective", "--json"], None)).unwrap();
    assert_eq!(effective["retrieval"]["search_tokens"], 2500);
    assert_eq!(effective["retrieval"]["search_limit"], 4);
    assert_eq!(effective["audit"]["source_provenance"], false);
    assert_eq!(effective["publish"]["orphan_policy"], "error");
    assert_eq!(effective["publish"]["output_tokens"], 512);

    let stored = std::fs::read_to_string(env.home.join("feature-config/wookie.toml")).unwrap();
    assert!(stored.contains("search_limit = 4"));
    assert!(!stored.contains("search_tokens"));

    let before = stored;
    let (success, _, stderr) = env.run(&["config", "set", "publish.output_tokens", "255"], None);
    assert!(!success);
    assert!(stderr.contains("at least 256"));
    assert_eq!(
        before,
        std::fs::read_to_string(env.home.join("feature-config/wookie.toml")).unwrap()
    );

    let keys = env.ok(&["config", "keys", "--global"], None);
    assert!(keys.contains("defaults.retrieval.search_tokens"));
    assert!(keys.contains("defaults.audit.source_provenance"));
    assert!(keys.contains("defaults.publish.output_tokens"));

    let global_keys: serde_json::Value =
        serde_json::from_str(&env.ok(&["config", "keys", "--global", "--json"], None)).unwrap();
    assert_eq!(global_keys["scope"], "global");
    assert!(global_keys["keys"]
        .as_array()
        .unwrap()
        .iter()
        .any(|key| key == "defaults.publish.output_tokens"));

    let wiki_keys: serde_json::Value =
        serde_json::from_str(&env.ok(&["config", "keys", "--json"], None)).unwrap();
    assert_eq!(wiki_keys["scope"], "wiki");
    assert!(wiki_keys["keys"].is_array());
}

#[test]
fn targeted_idempotent_notifications_filter_and_use_session_env() {
    let env = Env::new();
    env.ok(&["init", "routing"], None);
    let sender = env
        .ok(&["session", "start", "--agent", "codex", "--id-only"], None)
        .trim()
        .to_string();
    let receiver = env
        .ok(
            &["session", "start", "--agent", "claude", "--id-only"],
            None,
        )
        .trim()
        .to_string();
    let bystander = env
        .ok(&["session", "start", "--agent", "other", "--id-only"], None)
        .trim()
        .to_string();

    let args = [
        "notify",
        "--summary",
        "Scheduler changed",
        "--kind",
        "code-change",
        "--paths",
        "src/scheduler.rs",
        "--to",
        receiver.as_str(),
        "--idempotency-key",
        "scheduler-v2",
        "--metadata",
        "subsystem=scheduler",
        "--no-git-context",
        "--json",
    ];
    let first: serde_json::Value = serde_json::from_str(&env.ok_with_env(
        &args,
        Some("Retries now use jitter."),
        &[("WOOKIE_SESSION", sender.as_str())],
    ))
    .unwrap();
    let second: serde_json::Value = serde_json::from_str(&env.ok_with_env(
        &args,
        Some("Retries now use jitter."),
        &[("WOOKIE_SESSION", sender.as_str())],
    ))
    .unwrap();
    assert_eq!(first["notification"]["id"], second["notification"]["id"]);

    let (success, _, stderr) = env.run_with_env(
        &[
            "notify",
            "--summary",
            "Different payload",
            "--to",
            receiver.as_str(),
            "--idempotency-key",
            "scheduler-v2",
            "--no-git-context",
        ],
        None,
        &[("WOOKIE_SESSION", sender.as_str())],
    );
    assert!(!success);
    assert!(stderr.contains("different notification payload"));

    let routed: serde_json::Value = serde_json::from_str(&env.ok_with_env(
        &[
            "notifications",
            "--kind",
            "code-change",
            "--path",
            "src/",
            "--metadata",
            "subsystem=scheduler",
            "--json",
        ],
        None,
        &[("WOOKIE_SESSION", receiver.as_str())],
    ))
    .unwrap();
    assert_eq!(routed["notifications"].as_array().unwrap().len(), 1);
    let bystander_out = env.ok_with_env(
        &["notifications"],
        None,
        &[("WOOKIE_SESSION", bystander.as_str())],
    );
    assert!(bystander_out.contains("No unread"));

    env.ok(&["session", "close", bystander.as_str()], None);
    let preview = env.ok(
        &[
            "session",
            "prune",
            "--inactive-before",
            "2999-01-01T00:00:00Z",
        ],
        None,
    );
    assert!(preview.contains("Would prune"));
    assert!(env.home.join("routing/sessions").join(&bystander).exists());
    env.ok(
        &[
            "session",
            "prune",
            "--inactive-before",
            "2999-01-01T00:00:00Z",
            "--apply",
        ],
        None,
    );
    assert!(!env.home.join("routing/sessions").join(&bystander).exists());
}

#[test]
fn concurrent_notifications_keep_one_truthful_commit_each() {
    let env = Env::new();
    env.ok(&["init", "concurrent-history"], None);
    let sender: serde_json::Value =
        serde_json::from_str(&env.ok(&["session", "start", "--agent", "codex", "--json"], None))
            .unwrap();
    let sender = sender["id"].as_str().unwrap().to_string();
    let wiki_dir = env.home.join("concurrent-history");
    let before = Command::new("git")
        .args(["rev-list", "--count", "HEAD"])
        .current_dir(&wiki_dir)
        .output()
        .unwrap();
    let before: usize = String::from_utf8_lossy(&before.stdout)
        .trim()
        .parse()
        .unwrap();

    let mut workers = vec![];
    for index in 0..8 {
        let home = env.home.clone();
        let project = env.project.clone();
        let sender = sender.clone();
        workers.push(std::thread::spawn(move || {
            Command::new(env!("CARGO_BIN_EXE_wookie"))
                .args([
                    "notify",
                    "--session",
                    &sender,
                    "--summary",
                    &format!("parallel notice {index}"),
                    "--no-git-context",
                ])
                .env("WOOKIE_HOME", home)
                .current_dir(project)
                .output()
                .unwrap()
        }));
    }
    for worker in workers {
        let output = worker.join().unwrap();
        assert!(
            output.status.success(),
            "parallel notify failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            output.stderr.is_empty(),
            "parallel notify warned: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let after = Command::new("git")
        .args(["rev-list", "--count", "HEAD"])
        .current_dir(&wiki_dir)
        .output()
        .unwrap();
    let after: usize = String::from_utf8_lossy(&after.stdout)
        .trim()
        .parse()
        .unwrap();
    assert_eq!(after - before, 8, "each notification needs its own commit");
    let status = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&wiki_dir)
        .output()
        .unwrap();
    assert!(status.stdout.is_empty(), "wiki history left dirty state");
}

#[test]
fn plugin_status_detects_freshness_and_malformed_markers() {
    let env = Env::new();
    let fake_home = env.home.join("user-home");
    std::fs::create_dir_all(&fake_home).unwrap();
    let fake_home_str = fake_home.to_str().unwrap();

    let missing: serde_json::Value = serde_json::from_str(&env.ok_with_env(
        &["plugin", "status", "--json"],
        None,
        &[("HOME", fake_home_str)],
    ))
    .unwrap();
    assert_eq!(missing.as_array().unwrap().len(), 2);
    assert!(missing
        .as_array()
        .unwrap()
        .iter()
        .all(|status| status["state"] == "missing"));
    let (success, _, stderr) = env.run_with_env(
        &["plugin", "status", "--strict"],
        None,
        &[("HOME", fake_home_str)],
    );
    assert!(!success);
    assert!(stderr.contains("stale or missing"));

    env.ok_with_env(
        &["plugin", "install", "codex"],
        None,
        &[("HOME", fake_home_str)],
    );
    env.ok_with_env(
        &["plugin", "install", "claude"],
        None,
        &[("HOME", fake_home_str)],
    );
    let status = env.ok_with_env(
        &["plugin", "status", "--json"],
        None,
        &[("HOME", fake_home_str)],
    );
    let status: serde_json::Value = serde_json::from_str(&status).unwrap();
    assert_eq!(status.as_array().unwrap().len(), 2);
    assert!(status
        .as_array()
        .unwrap()
        .iter()
        .all(|status| status["state"] == "current"));
    env.ok_with_env(
        &["plugin", "status", "--strict"],
        None,
        &[("HOME", fake_home_str)],
    );

    let agents = fake_home.join(".codex/AGENTS.md");
    std::fs::write(
        &agents,
        "<!-- wookie:start -->\n<!-- wookie:version=0.0.0 -->\nstale\n<!-- wookie:end -->",
    )
    .unwrap();
    let stale: serde_json::Value = serde_json::from_str(&env.ok_with_env(
        &["plugin", "status", "codex", "--json"],
        None,
        &[("HOME", fake_home_str)],
    ))
    .unwrap();
    assert_eq!(stale[0]["state"], "stale");
    let (success, _, stderr) = env.run_with_env(
        &["plugin", "status", "codex", "--strict"],
        None,
        &[("HOME", fake_home_str)],
    );
    assert!(!success);
    assert!(stderr.contains("stale or missing"));

    std::fs::write(&agents, "<!-- wookie:start -->\nbroken").unwrap();
    let (success, _, stderr) = env.run_with_env(
        &["plugin", "install", "codex"],
        None,
        &[("HOME", fake_home_str)],
    );
    assert!(!success);
    assert!(stderr.contains("unmatched wookie marker"));
}

#[test]
fn indirect_rewrites_and_doctor_fixes_respect_rules_locks() {
    let env = Env::new();
    env.ok(&["init", "locked-indirect"], None);
    env.ok(&["new", "target"], Some("A target page."));
    env.ok(&["unlock", "style"], None);
    env.ok(
        &["new", "style/checks"],
        Some("**Style checks link to [[target]].** This is a rules page."),
    );
    env.ok(&["lock", "style"], None);

    let (success, _, stderr) = env.run(&["mv", "target", "architecture/target"], None);
    assert!(!success);
    assert!(stderr.contains("section 'style' is locked"));
    assert!(env.ok(&["read", "target"], None).contains("target page"));

    let rules_path = env.home.join("locked-indirect/pages/style/checks.md");
    let mut raw = std::fs::read_to_string(&rules_path).unwrap();
    raw.push('\n');
    std::fs::write(&rules_path, raw).unwrap();
    let (success, _, stderr) = env.run(&["doctor", "--fix"], None);
    assert!(!success);
    assert!(stderr.contains("section 'style' is locked"));

    env.ok(&["unlock", "style"], None);
    env.ok(&["mv", "target", "architecture/target"], None);
    assert!(env
        .ok(&["read", "style/checks"], None)
        .contains("[[architecture/target]]"));
}

#[test]
fn doctor_fix_preflights_all_pages_before_writing_any() {
    let env = Env::new();
    env.ok(&["init", "atomic-doctor"], None);
    env.ok(
        &["new", "architecture/early"],
        Some("**Early page.** This page sorts before the rules section."),
    );
    env.ok(&["unlock", "style"], None);
    env.ok(
        &["new", "style/checks"],
        Some("**Style checks.** This rules page sorts after the early page."),
    );
    env.ok(&["lock", "style"], None);

    let early_path = env.home.join("atomic-doctor/pages/architecture/early.md");
    let rules_path = env.home.join("atomic-doctor/pages/style/checks.md");
    for path in [&early_path, &rules_path] {
        let mut raw = std::fs::read_to_string(path).unwrap();
        raw.push('\n');
        std::fs::write(path, raw).unwrap();
    }
    let early_before = std::fs::read_to_string(&early_path).unwrap();

    let (success, _, stderr) = env.run(&["doctor", "--fix"], None);
    assert!(!success);
    assert!(stderr.contains("section 'style' is locked"));
    assert_eq!(
        std::fs::read_to_string(&early_path).unwrap(),
        early_before,
        "doctor must not partially canonicalize earlier pages before preflight fails"
    );

    env.ok(&["unlock", "style"], None);
    env.ok(&["doctor", "--fix"], None);
    assert_ne!(std::fs::read_to_string(&early_path).unwrap(), early_before);
}

#[test]
fn extreme_time_values_fail_cleanly_without_persisting() {
    let env = Env::new();
    env.ok(&["init", "bounded-time"], None);
    let config_path = env.home.join("bounded-time/wookie.toml");
    let before = std::fs::read_to_string(&config_path).unwrap();
    let maximum = u64::MAX.to_string();
    for key in [
        "sessions.initial_lookback_hours",
        "sessions.stale_after_minutes",
        "sessions.activity_debounce_seconds",
        "sessions.retention_days",
    ] {
        let (success, _, stderr) = env.run(&["config", "set", key, &maximum], None);
        assert!(!success, "extreme value unexpectedly accepted for {key}");
        assert!(
            stderr.contains("too large") || stderr.contains("representable"),
            "unexpected error for {key}: {stderr}"
        );
        assert_eq!(
            before,
            std::fs::read_to_string(&config_path).unwrap(),
            "invalid {key} persisted"
        );
    }

    let (success, _, stderr) = env.run(&["session", "start", "--lookback-hours", &maximum], None);
    assert!(!success);
    assert!(stderr.contains("too large"));

    let session = env
        .ok(&["session", "start", "--id-only"], None)
        .trim()
        .to_string();
    let (success, _, stderr) = env.run(
        &[
            "notifications",
            "--session",
            &session,
            "--max-age-hours",
            &maximum,
        ],
        None,
    );
    assert!(!success);
    assert!(stderr.contains("too large"));

    let (success, _, stderr) = env.run(&["session", "prune", "--older-than-days", &maximum], None);
    assert!(!success);
    assert!(stderr.contains("too large"));
}
