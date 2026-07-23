//! End-to-end coverage for Wookie's bounded retrieval and production controls.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Env {
    home: PathBuf,
    project: PathBuf,
}

impl Env {
    fn new(label: &str) -> Self {
        let sequence = COUNTER.fetch_add(1, Ordering::SeqCst);
        let base = std::env::temp_dir().join(format!(
            "wookie-hardening-{label}-{}-{sequence}",
            std::process::id()
        ));
        let home = base.join("home");
        let project = base.join("project");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        Self { home, project }
    }

    fn run(&self, args: &[&str], stdin: Option<&str>) -> (bool, String, String) {
        let mut command = Command::new(env!("CARGO_BIN_EXE_wookie"));
        command
            .args(args)
            .env("WOOKIE_HOME", &self.home)
            .current_dir(&self.project)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        if let Some(input) = stdin {
            use std::io::Write;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(input.as_bytes())
                .unwrap();
        }
        let output = child.wait_with_output().unwrap();
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).to_string(),
            String::from_utf8_lossy(&output.stderr).to_string(),
        )
    }

    fn ok(&self, args: &[&str], stdin: Option<&str>) -> String {
        let (success, stdout, stderr) = self.run(args, stdin);
        assert!(
            success,
            "wookie {args:?} failed:\nstdout:\n{stdout}\nstderr:\n{stderr}"
        );
        stdout
    }

    fn fails(&self, args: &[&str], stdin: Option<&str>) -> String {
        let (success, stdout, stderr) = self.run(args, stdin);
        assert!(
            !success,
            "wookie {args:?} unexpectedly succeeded:\n{stdout}"
        );
        format!("{stdout}\n{stderr}")
    }

    fn init(&self, slug: &str) {
        self.ok(&["init", slug], None);
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

    fn wiki_dir(&self, slug: &str) -> PathBuf {
        self.home.join(slug)
    }

    fn wiki_revision(&self, slug: &str) -> String {
        command_ok(self.wiki_dir(slug), &["git", "rev-parse", "HEAD"])
            .trim()
            .to_string()
    }

    fn init_project_git(&self) {
        std::fs::create_dir_all(self.project.join("src")).unwrap();
        std::fs::write(
            self.project.join("src/lib.rs"),
            "pub fn answer() -> u32 { 42 }\n",
        )
        .unwrap();
        command_ok(&self.project, &["git", "init", "-q"]);
        command_ok(
            &self.project,
            &["git", "config", "user.name", "Wookie Tests"],
        );
        command_ok(
            &self.project,
            &[
                "git",
                "config",
                "user.email",
                "wookie-tests@example.invalid",
            ],
        );
        command_ok(&self.project, &["git", "add", "src/lib.rs"]);
        command_ok(&self.project, &["git", "commit", "-q", "-m", "initial"]);
    }
}

fn command_ok(cwd: impl AsRef<Path>, args: &[&str]) -> String {
    let output = Command::new(args[0])
        .args(&args[1..])
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "command {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn parse_json(raw: &str) -> Value {
    serde_json::from_str(raw).unwrap_or_else(|error| panic!("invalid JSON ({error}):\n{raw}"))
}

fn publish_manifest(base_revision: &str, change: Value) -> String {
    serde_json::json!({
        "schema": "wookie.changeset/v1",
        "base_revision": base_revision,
        "changes": [change],
    })
    .to_string()
}

fn assert_report(value: &Value, command: &str, slug: &str) {
    assert_eq!(value["schema"], "wookie.report/v1");
    assert_eq!(value["command"], command);
    assert_eq!(value["snapshot"]["wiki"]["slug"], slug);
    assert!(value["generated_at"].is_string());
    assert!(value["summary"]["errors"].is_u64());
    assert!(value["summary"]["warnings"].is_u64());
    assert!(value["summary"]["total"].is_u64());
}

#[test]
fn prime_and_search_are_bounded_and_honor_pin_levels() {
    let env = Env::new("retrieval");
    env.init("retrieval");

    env.ok(
        &[
            "new",
            "guides/standing-order",
            "--pin-level",
            "instruction",
        ],
        Some(
            "**Standing-order reference.**\n\n## Agent instructions\n\nAlways run the focused verifier.\n\n## Rationale\n\nThis long rationale must stay out of prime.",
        ),
    );
    env.ok(
        &[
            "new",
            "guides/summary-reference",
            "--pin-level",
            "summary",
        ],
        Some(
            "**The quasar catalog is discoverable.**\n\nDetailed quasar material must stay out of a summary pin.",
        ),
    );
    env.ok(
        &[
            "new",
            "guides/discoverable",
            "--pin-level",
            "discoverable",
        ],
        Some(
            "**Discoverable runbook.**\n\nThis body must never be inlined into prime standing text.",
        ),
    );
    for suffix in ["alpha", "beta", "gamma"] {
        env.ok(
            &["new", &format!("architecture/quasar-{suffix}")],
            Some(&format!(
                "**Quasar {suffix} architecture.**\n\nThe quasar retrieval path for {suffix} has focused operational details."
            )),
        );
    }

    let prime_raw = env.ok(
        &[
            "--json",
            "prime",
            "--query",
            "quasar retrieval",
            "--tokens",
            "1200",
            "--instruction-tokens",
            "200",
            "--limit",
            "2",
            "--max-per-section",
            "1",
        ],
        None,
    );
    let prime = parse_json(&prime_raw);
    assert_eq!(prime["schema"], "wookie.prime/v1");
    assert_eq!(prime["telemetry"]["budget_tokens"], 1200);
    assert!(prime["suggested_pages"].as_array().unwrap().len() <= 2);
    assert!(
        prime_raw.chars().count().div_ceil(4) <= 1200,
        "prime exceeded its advertised response budget"
    );

    let instructions = prime["instructions"].as_array().unwrap();
    let standing = instructions
        .iter()
        .find(|item| item["id"] == "guides/standing-order")
        .unwrap();
    assert_eq!(standing["level"], "instruction");
    assert!(standing["text"]
        .as_str()
        .unwrap()
        .contains("focused verifier"));
    assert!(!standing["text"]
        .as_str()
        .unwrap()
        .contains("long rationale"));
    let summary = instructions
        .iter()
        .find(|item| item["id"] == "guides/summary-reference")
        .unwrap();
    assert_eq!(summary["level"], "summary");
    assert!(summary["text"].as_str().unwrap().contains("quasar catalog"));
    assert!(!summary["text"]
        .as_str()
        .unwrap()
        .contains("Detailed quasar"));
    assert!(instructions
        .iter()
        .all(|item| item["id"] != "guides/discoverable"));
    let discoverable = prime["discoverable_pages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"] == "guides/discoverable")
        .unwrap();
    assert_eq!(
        discoverable["read_command"],
        "wookie read guides/discoverable"
    );
    assert!(discoverable.get("text").is_none());
    assert!(discoverable.get("body").is_none());
    assert!(!prime_raw.contains("This body must never be inlined"));

    let search_raw = env.ok(
        &[
            "--json",
            "search",
            "quasar",
            "--limit",
            "1",
            "--tokens",
            "500",
            "--excerpt-lines",
            "1",
        ],
        None,
    );
    let search = parse_json(&search_raw);
    assert_eq!(search["schema"], "wookie.search/v1");
    assert_eq!(search["telemetry"]["limit"], 1);
    assert_eq!(search["hits"].as_array().unwrap().len(), 1);
    assert!(search["continuation"].is_u64());
    assert!(search_raw.chars().count().div_ceil(4) <= 500);
}

#[test]
fn custom_protocols_are_discoverable_and_render_project_scoped_pages() {
    let env = Env::new("protocols");
    env.init("protocols");
    let template = r#"+++
description = "Deployment runbook"
section = "guides"
tags = ["operations", "runbook"]
+++
**{{title}} is the runbook for {{id}}.**

Generated on {{date}}.

## Procedure

Document the safe deployment procedure.
"#;

    env.ok(&["protocol", "write", "operations/deploy"], Some(template));
    let listed = parse_json(&env.ok(&["--json", "protocol", "list"], None));
    assert_eq!(listed["schema"], "wookie.protocol-list/v1");
    assert!(listed["protocols"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["name"] == "operations/deploy"));

    let shown = parse_json(&env.ok(&["--json", "protocol", "show", "operations/deploy"], None));
    assert_eq!(shown["name"], "operations/deploy");
    assert_eq!(shown["header"]["section"], "guides");

    env.ok(
        &[
            "new",
            "guides/payments-deploy",
            "--title",
            "Payments Deploy",
            "--protocol",
            "operations/deploy",
        ],
        None,
    );
    let page = parse_json(&env.ok(&["--json", "read", "guides/payments-deploy"], None));
    assert!(page["body"].as_str().unwrap().contains("Payments Deploy"));
    assert!(page["body"]
        .as_str()
        .unwrap()
        .contains("guides/payments-deploy"));
    assert!(!page["body"].as_str().unwrap().contains("{{"));
    assert_eq!(
        page["frontmatter"]["tags"],
        serde_json::json!(["operations", "runbook"])
    );

    let error = env.fails(
        &[
            "new",
            "architecture/wrong-section",
            "--protocol",
            "operations/deploy",
        ],
        None,
    );
    assert!(error.contains("does not belong to protocol section 'guides'"));

    let custom_section_template = r#"+++
description = "Custom runbook"
section = "runbooks"
+++
**{{title}} is a custom runbook.**
"#;
    let unavailable = env.fails(
        &["protocol", "write", "operations/custom-runbook"],
        Some(custom_section_template),
    );
    assert!(
        unavailable.contains("section 'runbooks', which is not configured"),
        "got: {unavailable}"
    );
    let absent = env.fails(&["protocol", "show", "operations/custom-runbook"], None);
    assert!(absent.contains("no protocol 'operations/custom-runbook'"));

    let custom_section =
        "{ description = \"Operational runbooks\", kind = \"info\", locked = false }";
    env.ok(
        &[
            "config",
            "set",
            "sections.runbooks",
            custom_section,
            "--user-approved",
        ],
        None,
    );
    env.ok(
        &["protocol", "write", "operations/custom-runbook"],
        Some(custom_section_template),
    );

    // Removing a custom section can stale an existing protocol. Rendering it
    // must still fail before page mutation, even though the template itself
    // remains syntactically valid and discoverable.
    env.ok(
        &["config", "unset", "sections.runbooks", "--user-approved"],
        None,
    );
    let stale = env.fails(
        &[
            "new",
            "nightly-deploy",
            "--protocol",
            "operations/custom-runbook",
        ],
        None,
    );
    assert!(
        stale.contains("section 'runbooks', which is not configured"),
        "got: {stale}"
    );
    assert!(env
        .fails(&["read", "runbooks/nightly-deploy"], None)
        .contains("no page"));

    env.ok(
        &[
            "config",
            "set",
            "sections.runbooks",
            custom_section,
            "--user-approved",
        ],
        None,
    );
    env.ok(
        &[
            "new",
            "nightly-deploy",
            "--protocol",
            "operations/custom-runbook",
        ],
        None,
    );
    assert!(env
        .ok(&["read", "runbooks/nightly-deploy"], None)
        .contains("custom runbook"));
}

#[test]
fn audit_commands_share_stable_json_reports_and_ingest_reconciliation() {
    let env = Env::new("reports");
    env.init_project_git();
    env.init("reports");
    env.ok(
        &["new", "code/core", "--sources", "src/lib.rs"],
        Some(
            "**The core module owns the answer.**\n\nFile: `src/lib.rs`\n\n## Role\n\nIt provides the public answer API.",
        ),
    );
    let missing_receipt = env.fails(&["ingest", "--mark-reconciled"], None);
    assert!(missing_receipt.contains("requires --expect-worklist"));

    let incomplete = parse_json(&env.ok(&["--json", "ingest", "--full", "--level", "quick"], None));
    let incomplete_receipt = incomplete["data"]["worklist_receipt"].as_str().unwrap();
    let audit_gate_error = env.fails(
        &[
            "ingest",
            "--mark-reconciled",
            "--expect-worklist",
            incomplete_receipt,
            "--full",
            "--level",
            "quick",
        ],
        None,
    );
    assert!(audit_gate_error.contains("doctor found"));
    env.ok(
        &["write", "code/src"],
        Some(
            "**The source tree contains the core module.** See [[code/core]].\n\nFile: `src/`\n\n## Role\n\nIt owns project source files.",
        ),
    );
    env.add_required_audit_pages();
    let ready = parse_json(&env.ok(&["--json", "ingest", "--full", "--level", "quick"], None));
    let stale_wiki_receipt = ready["data"]["worklist_receipt"].as_str().unwrap();
    env.ok(
        &["write", "architecture/overview"],
        Some("**The architecture is documented and reviewed.** See [[index]]."),
    );
    let stale_wiki_error = env.fails(
        &[
            "ingest",
            "--mark-reconciled",
            "--expect-worklist",
            stale_wiki_receipt,
            "--full",
            "--level",
            "quick",
        ],
        None,
    );
    assert!(stale_wiki_error.contains("receipt changed"));

    let before_head_change =
        parse_json(&env.ok(&["--json", "ingest", "--full", "--level", "quick"], None));
    let stale_head_receipt = before_head_change["data"]["worklist_receipt"]
        .as_str()
        .unwrap();
    std::fs::write(
        env.project.join("src/lib.rs"),
        "pub fn answer() -> u32 { 43 }\n",
    )
    .unwrap();
    command_ok(&env.project, &["git", "add", "src/lib.rs"]);
    command_ok(
        &env.project,
        &["git", "commit", "-q", "-m", "change answer"],
    );
    let stale_head_error = env.fails(
        &[
            "ingest",
            "--mark-reconciled",
            "--expect-worklist",
            stale_head_receipt,
            "--full",
            "--level",
            "quick",
        ],
        None,
    );
    assert!(stale_head_error.contains("receipt changed"));

    env.ok(
        &["write", "code/core"],
        Some(
            "**The core module owns the current answer.**\n\nFile: `src/lib.rs`\n\n## Role\n\nIt provides the public answer API after reconciliation.",
        ),
    );
    let ready = parse_json(&env.ok(&["--json", "ingest", "--full", "--level", "quick"], None));
    let receipt = ready["data"]["worklist_receipt"].as_str().unwrap();
    let marked = parse_json(&env.ok(
        &[
            "--json",
            "ingest",
            "--mark-reconciled",
            "--expect-worklist",
            receipt,
            "--full",
            "--level",
            "quick",
        ],
        None,
    ));
    assert_report(&marked, "ingest", "reports");
    assert_eq!(marked["data"]["mode"], "mark");

    std::fs::write(
        env.project.join("src/lib.rs"),
        "pub fn answer() -> u32 { 44 }\n",
    )
    .unwrap();

    let doctor = parse_json(&env.ok(&["--json", "doctor"], None));
    assert_report(&doctor, "doctor", "reports");
    assert!(doctor["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .any(|diagnostic| diagnostic["code"] == "stale_page"));

    let status = parse_json(&env.ok(&["--json", "status"], None));
    assert_report(&status, "status", "reports");
    assert!(status["data"]["page_count"].is_u64());

    let ingest = parse_json(&env.ok(&["--json", "ingest", "--level", "quick"], None));
    assert_report(&ingest, "ingest", "reports");
    assert_eq!(ingest["data"]["mode"], "update");
    let stale = ingest["data"]["stale"].as_array().unwrap();
    let core = stale.iter().find(|item| item["id"] == "code/core").unwrap();
    assert_eq!(core["confidence"], "high");
    assert_eq!(core["suggested_sections"], serde_json::json!(["code"]));
    assert!(ingest["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .any(|diagnostic| diagnostic["code"] == "stale_page"));

    let critique = parse_json(&env.ok(&["--json", "critique", "--paths", "src/lib.rs"], None));
    assert_report(&critique, "critique", "reports");
    assert_eq!(critique["data"]["evaluation"], "not_executed");
    assert_eq!(critique["data"]["files"], serde_json::json!(["src/lib.rs"]));
}

#[test]
fn ingest_projection_is_bounded_but_receipt_covers_the_full_worklist() {
    let env = Env::new("ingest-bounds");
    env.init_project_git();
    env.init("ingest-bounds");
    let base = command_ok(&env.project, &["git", "rev-parse", "HEAD"])
        .trim()
        .to_string();
    for index in 0..30 {
        std::fs::write(
            env.project.join(format!("uncovered-{index:02}.txt")),
            format!("item {index}\n"),
        )
        .unwrap();
    }

    let bounded = parse_json(&env.ok(
        &[
            "--json", "ingest", "--since", &base, "--level", "quick", "--limit", "2",
        ],
        None,
    ));
    assert_report(&bounded, "ingest", "ingest-bounds");
    assert!(bounded["data"]["changed"].as_array().unwrap().len() <= 2);
    assert!(bounded["data"]["uncovered"].as_array().unwrap().len() <= 2);
    assert!(
        bounded["data"]["projection"]["changed"]["omitted"]
            .as_u64()
            .unwrap()
            > 0
    );

    let exhaustive = parse_json(&env.ok(
        &[
            "--json", "ingest", "--since", &base, "--level", "quick", "--all",
        ],
        None,
    ));
    assert_eq!(exhaustive["data"]["changed"].as_array().unwrap().len(), 30);
    assert_eq!(
        bounded["data"]["worklist_receipt"],
        exhaustive["data"]["worklist_receipt"]
    );
}

#[cfg(unix)]
#[test]
fn ingest_mark_commit_failure_restores_config_and_index() {
    use std::os::unix::fs::PermissionsExt as _;

    let env = Env::new("ingest-hook");
    env.init_project_git();
    env.init("ingest-hook");
    env.ok(&["ingest", "--full", "--level", "quick"], None);
    env.ok(
        &["write", "code/src"],
        Some(
            "**The source tree is documented.** See [[index]].\n\nFile: `src/`\n\n## Role\n\nIt contains the project source.",
        ),
    );
    env.add_required_audit_pages();
    let ready = parse_json(&env.ok(&["--json", "ingest", "--full", "--level", "quick"], None));
    let receipt = ready["data"]["worklist_receipt"].as_str().unwrap();
    let wiki = env.wiki_dir("ingest-hook");
    let config_path = wiki.join("wookie.toml");
    let config_before = std::fs::read(&config_path).unwrap();
    let head_before = command_ok(&wiki, &["git", "rev-parse", "HEAD"]);
    let hook = wiki.join(".git/hooks/pre-commit");
    std::fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
    let mut permissions = std::fs::metadata(&hook).unwrap().permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&hook, permissions).unwrap();

    let error = env.fails(
        &[
            "ingest",
            "--mark-reconciled",
            "--expect-worklist",
            receipt,
            "--full",
            "--level",
            "quick",
        ],
        None,
    );
    assert!(error.contains("config and index restored"), "got: {error}");
    assert_eq!(std::fs::read(&config_path).unwrap(), config_before);
    assert_eq!(
        command_ok(&wiki, &["git", "rev-parse", "HEAD"]),
        head_before
    );
    assert!(command_ok(
        &wiki,
        &["git", "status", "--porcelain=v1", "--", "wookie.toml"]
    )
    .trim()
    .is_empty());

    std::fs::remove_file(&hook).unwrap();
    let commit_message_hook = wiki.join(".git/hooks/pre-commit");
    std::fs::write(
        &commit_message_hook,
        "#!/bin/sh\nprintf '%s\\n' '# hook dirtied worktree' >> wookie.toml\nexit 0\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&commit_message_hook)
        .unwrap()
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&commit_message_hook, permissions).unwrap();

    let ambiguous = env.fails(
        &[
            "ingest",
            "--mark-reconciled",
            "--expect-worklist",
            receipt,
            "--full",
            "--level",
            "quick",
        ],
        None,
    );
    assert!(
        ambiguous.contains("could not be verified"),
        "got: {ambiguous}"
    );
    assert!(wiki.join(".ingest-reconciliation-recovery.json").is_file());
    let blocked = env.fails(
        &["new", "guides/blocked"],
        Some("**This write must remain blocked.**"),
    );
    assert!(blocked.contains("unresolved ingest reconciliation marker"));
    let marker_path = wiki.join(".ingest-reconciliation-recovery.json");
    let marker_raw = std::fs::read(&marker_path).unwrap();
    let marker: serde_json::Value = serde_json::from_slice(&marker_raw).unwrap();
    let mut tampered_marker = marker.clone();
    tampered_marker["base_head"] = serde_json::json!("--hard");
    std::fs::write(
        &marker_path,
        serde_json::to_vec_pretty(&tampered_marker).unwrap(),
    )
    .unwrap();
    let head_before_tampered_recovery = command_ok(&wiki, &["git", "rev-parse", "HEAD"]);
    let tampered = env.fails(&["ingest", "--recover", "accept"], None);
    assert!(tampered.contains("exact canonical Git object ID"));
    assert_eq!(
        command_ok(&wiki, &["git", "rev-parse", "HEAD"]),
        head_before_tampered_recovery
    );
    std::fs::write(&marker_path, marker_raw).unwrap();
    std::fs::write(
        &config_path,
        marker["target_config"].as_str().unwrap().as_bytes(),
    )
    .unwrap();
    let recovered = env.ok(&["--json", "ingest", "--recover", "accept"], None);
    let recovered: serde_json::Value = serde_json::from_str(&recovered).unwrap();
    assert_eq!(recovered["schema"], "wookie.ingest-recovery/v1");
    assert_eq!(recovered["action"], "accept");
    assert!(!marker_path.exists());

    std::fs::write(
        env.project.join("src/lib.rs"),
        "pub fn answer() -> u32 { 99 }\n",
    )
    .unwrap();
    command_ok(&env.project, &["git", "add", "src/lib.rs"]);
    command_ok(
        &env.project,
        &["git", "commit", "-q", "-m", "next project revision"],
    );
    let next = parse_json(&env.ok(&["--json", "ingest", "--full", "--level", "quick"], None));
    let next_receipt = next["data"]["worklist_receipt"].as_str().unwrap();
    let ambiguous = env.fails(
        &[
            "ingest",
            "--mark-reconciled",
            "--expect-worklist",
            next_receipt,
            "--full",
            "--level",
            "quick",
        ],
        None,
    );
    assert!(ambiguous.contains("could not be verified"));
    let marker_path = wiki.join(".ingest-reconciliation-recovery.json");
    let marker: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&marker_path).unwrap()).unwrap();
    let previous_config = marker["previous_config"].as_str().unwrap().to_string();
    std::fs::write(
        &config_path,
        marker["target_config"].as_str().unwrap().as_bytes(),
    )
    .unwrap();

    // The hook lets the compensating commit land but dirties the config after
    // staging. Recovery must retain its rolling-back journal instead of
    // guessing, then recognize the exact compensating child on retry.
    let interrupted_rollback = env.fails(&["ingest", "--recover", "rollback"], None);
    assert!(
        interrupted_rollback.contains("exact commit did not land")
            || interrupted_rollback.contains("without an exact commit"),
        "got: {interrupted_rollback}"
    );
    let rolling_back: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&marker_path).unwrap()).unwrap();
    assert_eq!(rolling_back["state"], "rolling_back");
    assert_eq!(
        command_ok(&wiki, &["git", "log", "-1", "--format=%s"]).trim(),
        "wookie: rollback ingest --mark-reconciled"
    );

    std::fs::remove_file(&commit_message_hook).unwrap();
    std::fs::write(&config_path, previous_config.as_bytes()).unwrap();
    let rolled_back = env.ok(&["--json", "ingest", "--recover", "rollback"], None);
    let rolled_back: serde_json::Value = serde_json::from_str(&rolled_back).unwrap();
    assert_eq!(rolled_back["action"], "rollback");
    assert_eq!(
        std::fs::read_to_string(&config_path).unwrap(),
        previous_config
    );
    assert!(!marker_path.exists());
    assert_eq!(
        command_ok(&wiki, &["git", "log", "-1", "--format=%s"]).trim(),
        "wookie: rollback ingest --mark-reconciled"
    );

    // A hook that mutates a page after the config-only commit lands must be
    // caught by the final catalog check. Recovery remains blocked until the
    // exact catalog behind the receipt is restored.
    let ready_after_rollback =
        parse_json(&env.ok(&["--json", "ingest", "--full", "--level", "quick"], None));
    let page_receipt = ready_after_rollback["data"]["worklist_receipt"]
        .as_str()
        .unwrap();
    let overview_path = wiki.join("pages/architecture/overview.md");
    let overview_before = std::fs::read(&overview_path).unwrap();
    std::fs::write(
        &commit_message_hook,
        "#!/bin/sh\nprintf '%s\\n' 'hook catalog mutation' >> pages/architecture/overview.md\nexit 0\n",
    )
    .unwrap();
    let mut permissions = std::fs::metadata(&commit_message_hook)
        .unwrap()
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&commit_message_hook, permissions).unwrap();
    let catalog_race = env.fails(
        &[
            "ingest",
            "--mark-reconciled",
            "--expect-worklist",
            page_receipt,
            "--full",
            "--level",
            "quick",
        ],
        None,
    );
    assert!(catalog_race.contains("state changed during the metadata commit"));
    let recovery_refused = env.fails(&["ingest", "--recover", "accept"], None);
    assert!(recovery_refused.contains("wiki catalog changed"));
    std::fs::remove_file(&commit_message_hook).unwrap();
    std::fs::write(&overview_path, overview_before).unwrap();
    env.ok(&["ingest", "--recover", "accept"], None);
    assert!(!marker_path.exists());
}

#[test]
fn publish_check_is_dry_and_apply_commits_the_validated_plan() {
    let env = Env::new("publish");
    env.init("publish");
    let revision = env.wiki_revision("publish");
    let manifest = serde_json::json!({
        "schema": "wookie.changeset/v1",
        "base_revision": revision,
        "message": "publish architecture page",
        "changes": [{
            "op": "create",
            "id": "architecture/published",
            "body": "**The published page records validated state.**\n\n## Role\n\nIt proves transactional publication."
        }]
    })
    .to_string();

    let checked = parse_json(&env.ok(
        &["--json", "publish", "--check", "--full-diff"],
        Some(&manifest),
    ));
    assert_eq!(checked["report"]["schema"], "wookie.report/v1");
    assert_eq!(checked["report"]["command"], "publish-check");
    assert_eq!(checked["plan"]["schema"], "wookie.publish-plan/v1");
    assert_eq!(checked["applied"], false);
    assert_eq!(
        checked["report"]["data"]["expected_doctor"]["command"],
        "publish-doctor"
    );
    assert_eq!(
        checked["report"]["data"]["expected_critique"]["command"],
        "publish-critique"
    );
    assert_eq!(
        checked["report"]["data"]["expected_critique"]["data"]["status"],
        "not_required"
    );
    let human = env.ok(&["publish", "--check"], Some(&manifest));
    assert!(human.contains("Expected checks:"), "{human}");
    assert!(human.contains("- Doctor: "), "{human}");
    assert!(human.contains("- Critique: not required"), "{human}");
    assert!(
        human.contains("Machine-readable expected reports:"),
        "{human}"
    );
    let full_human = env.ok(&["publish", "--check", "--full-diff"], Some(&manifest));
    assert!(full_human.contains("Expected checks:"), "{full_human}");
    assert!(
        full_human.contains("- Critique: not required"),
        "{full_human}"
    );
    let broken_manifest = serde_json::json!({
        "schema": "wookie.changeset/v1",
        "base_revision": revision,
        "message": "check broken publication",
        "changes": [{
            "op": "create",
            "id": "architecture/broken-preview",
            "body": "**Broken preview.** See [[missing-preview-target]]."
        }]
    })
    .to_string();
    let broken_human = env.ok(
        &["publish", "--check", "--full-diff"],
        Some(&broken_manifest),
    );
    assert!(broken_human.contains("Diagnostics:"), "{broken_human}");
    assert!(broken_human.contains("broken_link"), "{broken_human}");
    env.fails(&["read", "architecture/published"], None);

    let applied = parse_json(&env.ok(&["--json", "publish", "--apply"], Some(&manifest)));
    assert_eq!(applied["applied"], true);
    assert_eq!(applied["plan"]["operations"].as_array().unwrap().len(), 1);
    let page = env.ok(&["read", "architecture/published"], None);
    assert!(page.contains("validated state"));
}

#[test]
fn publish_preview_bounds_large_pages_and_full_diff_is_explicit() {
    let env = Env::new("publish-budget");
    env.init("publish-budget");
    let revision = env.wiki_revision("publish-budget");
    let mut body = String::from("**Large preview fixture.**\n\n");
    for index in 0..1_000 {
        body.push_str(&format!(
            "line {index}: deterministic publication detail that must not flood the compact preview\n"
        ));
    }
    body.push_str("UNIQUE_EXHAUSTIVE_TAIL_MARKER\n");
    let manifest = serde_json::json!({
        "schema": "wookie.changeset/v1",
        "base_revision": revision,
        "changes": [{
            "op": "create",
            "id": "architecture/large-preview",
            "body": body,
        }]
    })
    .to_string();

    let bounded_raw = env.ok(
        &["--json", "publish", "--check", "--tokens", "1200"],
        Some(&manifest),
    );
    let bounded = parse_json(&bounded_raw);
    assert_eq!(bounded["schema"], "wookie.publish-preview/v1");
    assert_eq!(bounded["diff_mode"], "compact");
    assert_eq!(bounded["telemetry"]["budget_tokens"], 1200);
    assert!(bounded["diffs"][0]["after_excerpt"].is_array());
    assert!(
        bounded["diffs"][0]["omitted_after_changed_lines"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(!bounded_raw.contains("UNIQUE_EXHAUSTIVE_TAIL_MARKER"));
    assert!(
        bounded_raw.len().div_ceil(3) <= 1200,
        "bounded preview exceeded its advertised token estimate"
    );

    let full_raw = env.ok(
        &["--json", "publish", "--check", "--full-diff"],
        Some(&manifest),
    );
    let full = parse_json(&full_raw);
    assert_eq!(full["diff_mode"], "full");
    assert!(full["diffs"][0]["after"]
        .as_str()
        .unwrap()
        .contains("UNIQUE_EXHAUSTIVE_TAIL_MARKER"));
    assert!(full_raw.len() > bounded_raw.len());
}

#[test]
fn publish_apply_response_is_bounded_without_hiding_success() {
    let env = Env::new("publish-apply-budget");
    env.init("publish-apply-budget");
    env.ok(&["config", "set", "publish.output_tokens", "600"], None);
    let revision = env.wiki_revision("publish-apply-budget");
    let changes = (0..80)
        .map(|index| {
            serde_json::json!({
                "op": "create",
                "id": format!("architecture/bulk-{index:03}"),
                "body": format!("**Bulk page {index}.**\n\nA bounded apply fixture."),
            })
        })
        .collect::<Vec<_>>();
    let manifest = serde_json::json!({
        "schema": "wookie.changeset/v1",
        "base_revision": revision,
        "changes": changes,
    })
    .to_string();

    let raw = env.ok(&["--json", "publish", "--apply"], Some(&manifest));
    let result = parse_json(&raw);
    assert_eq!(result["schema"], "wookie.publish-result/v1");
    assert_eq!(result["applied"], true);
    assert_eq!(result["summary"]["operations"], 80);
    assert!(result["omissions"]["operations"].as_u64().unwrap() > 0);
    assert!(raw.len().div_ceil(3) <= 600);
    assert!(env
        .ok(&["read", "architecture/bulk-079"], None)
        .contains("Bulk page 79"));
}

#[test]
fn publish_apply_rejects_preexisting_staged_unstaged_and_untracked_targets() {
    for state in ["unstaged", "staged", "untracked"] {
        let env = Env::new(&format!("publish-dirty-{state}"));
        env.init("dirty");
        let wiki_dir = env.wiki_dir("dirty");
        let revision = env.wiki_revision("dirty");
        let (id, change) = if state == "untracked" {
            let id = "guides/untracked";
            let path = wiki_dir.join("pages/guides/untracked.md");
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(
                &path,
                "---\ntitle: Untracked\ndescription: Local page\ntags: []\ncreated: 2026-01-01\nupdated: 2026-01-01\nsources: []\npin: false\naliases: []\n---\n\n**Local untracked page.**\n",
            )
            .unwrap();
            (id, serde_json::json!({"op": "delete", "id": id}))
        } else {
            let id = "index";
            let path = wiki_dir.join("pages/index.md");
            let mut raw = std::fs::read_to_string(&path).unwrap();
            raw.push_str("\nLocal edit that publish must not absorb.\n");
            std::fs::write(&path, raw).unwrap();
            if state == "staged" {
                command_ok(&wiki_dir, &["git", "add", "--", "pages/index.md"]);
            }
            (
                id,
                serde_json::json!({
                    "op": "update",
                    "id": id,
                    "body": "**A reviewed replacement.**"
                }),
            )
        };
        let manifest = publish_manifest(&revision, change);
        let error = env.fails(&["publish", "--apply"], Some(&manifest));
        assert!(
            error.contains("pre-existing staged, unstaged, or untracked"),
            "state={state}, id={id}, error={error}"
        );
    }
}

#[test]
fn publish_rejects_no_op_manifest_and_supports_review_token_guard() {
    let env = Env::new("publish-review-token");
    env.init("review-token");
    let revision = env.wiki_revision("review-token");
    // A metadata patch to the already-default pin value is guaranteed to be a
    // semantic no-op without depending on the human read rendering.
    let no_op = publish_manifest(
        &revision,
        serde_json::json!({
            "op": "update",
            "id": "index",
            "metadata": {"pin": false}
        }),
    );
    let no_op_report = env.ok(&["publish", "--check"], Some(&no_op));
    assert!(
        no_op_report.contains("no effective page operations"),
        "{no_op_report}"
    );

    let manifest = publish_manifest(
        &revision,
        serde_json::json!({
            "op": "create",
            "id": "guides/token-bound",
            "body": "**The token binds this exact reviewed plan.**"
        }),
    );
    let checked = parse_json(&env.ok(&["--json", "publish", "--check"], Some(&manifest)));
    let token = checked["review_token"].as_str().unwrap();
    assert!(token.starts_with("sha256:"));
    let wrong = format!("sha256:{}", "0".repeat(64));
    let mismatch = env.fails(
        &["publish", "--apply", "--expect-plan", &wrong],
        Some(&manifest),
    );
    assert!(
        mismatch.contains("review token does not match"),
        "{mismatch}"
    );
    let applied = parse_json(&env.ok(
        &["--json", "publish", "--apply", "--expect-plan", token],
        Some(&manifest),
    ));
    assert_eq!(applied["applied"], true);
}

#[cfg(unix)]
#[test]
fn failed_publish_commit_restores_page_metadata_and_content() {
    use std::os::unix::fs::PermissionsExt;

    let env = Env::new("rollback");
    env.init("rollback");
    env.ok(
        &["new", "architecture/rollback-target"],
        Some("**The original publication state.**\n\nOriginal details."),
    );
    let wiki_dir = env.wiki_dir("rollback");
    let page_path = wiki_dir.join("pages/architecture/rollback-target.md");
    let before = std::fs::read(&page_path).unwrap();
    let revision = env.wiki_revision("rollback");
    let manifest = serde_json::json!({
        "schema": "wookie.changeset/v1",
        "base_revision": revision,
        "changes": [{
            "op": "update",
            "id": "architecture/rollback-target",
            "body": "**A replacement that must roll back.**\n\nReplacement details.",
            "metadata": {"tags": ["replacement"]}
        }]
    })
    .to_string();

    let hook = wiki_dir.join(".git/hooks/pre-commit");
    std::fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let error = env.fails(&["--json", "publish", "--apply"], Some(&manifest));
    assert!(
        error.contains("git commit failed"),
        "unexpected error: {error}"
    );
    assert_eq!(std::fs::read(&page_path).unwrap(), before);
    assert!(!wiki_dir.join(".publish-journal.json").exists());
    assert!(!wiki_dir.join(".publish.lock").exists());
}

#[cfg(unix)]
#[test]
fn ambiguous_hook_commit_retains_journal_instead_of_guessing_rollback() {
    use std::os::unix::fs::PermissionsExt;

    let env = Env::new("publish-ambiguous-hook");
    env.init("publish-ambiguous-hook");
    let wiki_dir = env.wiki_dir("publish-ambiguous-hook");
    let revision = env.wiki_revision("publish-ambiguous-hook");
    let manifest = publish_manifest(
        &revision,
        serde_json::json!({
            "op": "create",
            "id": "architecture/hook-advanced-head",
            "body": "**The hook advanced HEAD before reporting failure.**"
        }),
    );
    let hook = wiki_dir.join(".git/hooks/pre-commit");
    std::fs::write(
        &hook,
        "#!/bin/sh\ngit commit --no-verify -q -m 'unexpected hook commit'\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

    let error = env.fails(&["publish", "--apply"], Some(&manifest));

    assert!(
        error.contains("history is uncertain") || error.contains("journal retained"),
        "{error}"
    );
    assert!(wiki_dir.join(".publish-journal.json").is_file());
    assert!(!wiki_dir.join(".publish.lock").exists());
    assert!(wiki_dir
        .join("pages/architecture/hook-advanced-head.md")
        .is_file());
}

#[cfg(unix)]
#[test]
fn hook_staged_unreviewed_path_never_reports_publish_success() {
    use std::os::unix::fs::PermissionsExt;

    let env = Env::new("publish-hook-extra-path");
    env.init("publish-hook-extra-path");
    let wiki_dir = env.wiki_dir("publish-hook-extra-path");
    let revision = env.wiki_revision("publish-hook-extra-path");
    let manifest = publish_manifest(
        &revision,
        serde_json::json!({
            "op": "create",
            "id": "architecture/reviewed-page",
            "body": "**Only this page was reviewed.**"
        }),
    );
    std::fs::write(wiki_dir.join("hook-unreviewed.txt"), "hook-controlled\n").unwrap();
    let hook = wiki_dir.join(".git/hooks/pre-commit");
    std::fs::write(&hook, "#!/bin/sh\ngit add -- hook-unreviewed.txt\nexit 0\n").unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

    let error = env.fails(&["publish", "--apply"], Some(&manifest));

    assert!(
        error.contains("changed paths outside") || error.contains("journal retained"),
        "{error}"
    );
    assert!(wiki_dir.join(".publish-journal.json").is_file());
    assert!(!wiki_dir.join(".publish.lock").exists());
    command_ok(&wiki_dir, &["git", "show", "HEAD:hook-unreviewed.txt"]);
}

#[cfg(unix)]
#[test]
fn hook_changed_tree_mode_never_reports_publish_success() {
    use std::os::unix::fs::PermissionsExt;

    let env = Env::new("publish-hook-mode");
    env.init("publish-hook-mode");
    let wiki_dir = env.wiki_dir("publish-hook-mode");
    command_ok(&wiki_dir, &["git", "config", "core.filemode", "false"]);
    let revision = env.wiki_revision("publish-hook-mode");
    let manifest = publish_manifest(
        &revision,
        serde_json::json!({
            "op": "create",
            "id": "architecture/reviewed-mode",
            "body": "**This page is reviewed as non-executable.**"
        }),
    );
    let hook = wiki_dir.join(".git/hooks/pre-commit");
    std::fs::write(
        &hook,
        "#!/bin/sh\ngit update-index --chmod=+x -- pages/architecture/reviewed-mode.md\nexit 0\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

    let error = env.fails(&["publish", "--apply"], Some(&manifest));

    assert!(
        error.contains("tree mode differs") || error.contains("journal retained"),
        "{error}"
    );
    assert!(wiki_dir.join(".publish-journal.json").is_file());
    let tree = command_ok(
        &wiki_dir,
        &[
            "git",
            "ls-tree",
            "HEAD",
            "--",
            "pages/architecture/reviewed-mode.md",
        ],
    );
    assert!(tree.starts_with("100755 blob"), "{tree}");
}

#[cfg(unix)]
#[test]
fn hook_mutating_unrelated_page_is_caught_by_full_catalog_verification() {
    use std::os::unix::fs::PermissionsExt;

    let env = Env::new("publish-hook-unrelated-page");
    env.init("publish-hook-unrelated-page");
    let wiki_dir = env.wiki_dir("publish-hook-unrelated-page");
    let revision = env.wiki_revision("publish-hook-unrelated-page");
    let manifest = publish_manifest(
        &revision,
        serde_json::json!({
            "op": "create",
            "id": "architecture/reviewed-catalog",
            "body": "**Only the planned catalog change is acceptable.**"
        }),
    );
    let hook = wiki_dir.join(".git/hooks/pre-commit");
    std::fs::write(
        &hook,
        "#!/bin/sh\nprintf '%s\\n' 'hook changed unrelated page' >> pages/index.md\nexit 0\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

    let error = env.fails(&["publish", "--apply"], Some(&manifest));

    assert!(
        error.contains("full state") || error.contains("catalog"),
        "{error}"
    );
    assert!(wiki_dir.join(".publish-journal.json").is_file());
    assert!(!wiki_dir.join(".publish.lock").exists());
    let recovery = env.fails(&["publish", "--recover", "rollback"], None);
    assert!(
        recovery.contains("unrelated catalog") || recovery.contains("unrelated wiki page"),
        "{recovery}"
    );
    assert!(wiki_dir.join(".publish-journal.json").is_file());
}

#[cfg(unix)]
#[test]
fn hook_mutating_effective_config_is_caught_after_commit() {
    use std::os::unix::fs::PermissionsExt;

    let env = Env::new("publish-hook-config-policy");
    env.init("publish-hook-config-policy");
    let wiki_dir = env.wiki_dir("publish-hook-config-policy");
    let revision = env.wiki_revision("publish-hook-config-policy");
    let manifest = publish_manifest(
        &revision,
        serde_json::json!({
            "op": "create",
            "id": "architecture/reviewed-policy",
            "body": "**The publish policy is part of the reviewed environment.**"
        }),
    );
    let hook = wiki_dir.join(".git/hooks/pre-commit");
    std::fs::write(
        &hook,
        "#!/bin/sh\nprintf '\\n[publish]\\nrequire_base_revision = false\\n' >> wookie.toml\nexit 0\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

    let error = env.fails(&["publish", "--apply"], Some(&manifest));

    assert!(
        error.contains("full state") || error.contains("configuration") || error.contains("policy"),
        "{error}"
    );
    assert!(wiki_dir.join(".publish-journal.json").is_file());
    assert!(!wiki_dir.join(".publish.lock").exists());
}

#[cfg(unix)]
#[test]
fn hook_reopening_relocked_rules_section_is_caught_after_commit() {
    use std::os::unix::fs::PermissionsExt;

    let env = Env::new("publish-hook-unlock");
    env.init("publish-hook-unlock");
    env.add_required_audit_pages();
    env.ok(&["unlock", "style"], None);
    let wiki_dir = env.wiki_dir("publish-hook-unlock");
    let revision = env.wiki_revision("publish-hook-unlock");
    let manifest = publish_manifest(
        &revision,
        serde_json::json!({
            "op": "create",
            "id": "style/reviewed-hook-control",
            "body": "**Reviewed rule changes must finish with their section relocked.**"
        }),
    );
    let hook = wiki_dir.join(".git/hooks/pre-commit");
    std::fs::write(
        &hook,
        "#!/bin/sh\nprintf 'locked = false\\nexpires_at = \"2999-01-01T00:00:00+00:00\"\\n' > .unlocks/style.toml\nexit 0\n",
    )
    .unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();

    let error = env.fails(&["publish", "--apply", "--user-approved"], Some(&manifest));

    assert!(
        error.contains("full state")
            || error.contains("lock-control")
            || error.contains("lock controls")
            || error.contains("relocked"),
        "{error}"
    );
    assert!(wiki_dir.join(".publish-journal.json").is_file());
    assert!(!wiki_dir.join(".publish.lock").exists());
    assert!(
        std::fs::read_to_string(wiki_dir.join(".unlocks/style.toml"))
            .unwrap()
            .contains("locked = false")
    );
}

#[test]
fn rules_workflow_requires_explicit_apply_approval_and_relocks() {
    let env = Env::new("rules");
    env.init("rules");
    env.ok(&["config", "set", "auto_commit", "false"], None);
    let revision = env.wiki_revision("rules");
    let manifest = serde_json::json!({
        "schema": "wookie.changeset/v1",
        "base_revision": revision,
        "changes": [{
            "op": "create",
            "id": "style/checks",
            "body": "**Style checks define the review gate.**\n\n## Scope\n\nAll source files.\n\n## Procedure\n\nRun the formatter.\n\n## Violations\n\nUnformatted code.\n\n## Exceptions\n\nNone."
        }]
    })
    .to_string();

    let proposed = parse_json(&env.ok(&["--json", "rules", "propose"], Some(&manifest)));
    let proposal = proposed["proposal"].as_str().unwrap();
    assert!(proposal.starts_with("rule-"));

    let unreviewed = env.fails(&["rules", "apply", proposal, "--user-approved"], None);
    assert!(unreviewed.contains("review receipt"), "{unreviewed}");

    let reviewed = parse_json(&env.ok(&["--json", "rules", "review", proposal], None));
    assert_eq!(reviewed["report"]["schema"], "wookie.report/v1");
    assert_eq!(reviewed["applied"], false);
    for field in [
        "manifest_sha256",
        "catalog_sha256",
        "config_sha256",
        "effective_policy_sha256",
        "plan_sha256",
    ] {
        assert!(reviewed["review_receipt"][field]
            .as_str()
            .is_some_and(|value| value.starts_with("sha256:")));
    }

    let receipt_path = env
        .wiki_dir("rules")
        .join(format!("proposals/rules/{proposal}.review.json"));
    let mut tampered: Value =
        serde_json::from_str(&std::fs::read_to_string(&receipt_path).unwrap()).unwrap();
    tampered
        .as_object_mut()
        .unwrap()
        .insert("unknown".to_string(), Value::Bool(true));
    std::fs::write(&receipt_path, serde_json::to_vec(&tampered).unwrap()).unwrap();
    let invalid_receipt = env.fails(&["rules", "apply", proposal, "--user-approved"], None);
    assert!(invalid_receipt.contains("invalid rule review receipt"));
    env.ok(&["rules", "review", proposal], None);

    let denied = env.fails(&["rules", "apply", proposal], None);
    assert!(denied.contains("requires --user-approved"));

    // Simulate an Obsidian edit while automatic history is disabled. The Git
    // revision is unchanged, so only the exact raw catalog receipt catches it.
    let index_path = env.wiki_dir("rules").join("pages/index.md");
    let mut index = std::fs::read_to_string(&index_path).unwrap();
    index.push_str("\nEdited outside Wookie after review.\n");
    std::fs::write(&index_path, index).unwrap();
    let stale = env.fails(&["rules", "apply", proposal, "--user-approved"], None);
    assert!(stale.contains("receipt is stale"), "{stale}");

    env.ok(&["rules", "review", proposal], None);
    env.ok(&["unlock", "style"], None);

    let applied = parse_json(&env.ok(
        &["--json", "rules", "apply", proposal, "--user-approved"],
        None,
    ));
    assert_eq!(applied["applied"], true);
    assert!(env
        .ok(&["read", "style/checks"], None)
        .contains("review gate"));

    let relocked = env.fails(
        &["write", "style/checks"],
        Some("**A direct edit should be rejected.**"),
    );
    assert!(relocked.contains("locked"), "unexpected error: {relocked}");
}

#[test]
fn approved_publish_supports_custom_locked_info_without_rule_relock() {
    let env = Env::new("locked-info");
    env.init("locked-info");
    env.ok(
        &[
            "config",
            "set",
            "sections.secure",
            "{ description = \"Protected reference\", kind = \"info\", locked = true }",
            "--user-approved",
        ],
        None,
    );
    let revision = env.wiki_revision("locked-info");
    let manifest = publish_manifest(
        &revision,
        serde_json::json!({
            "op": "create",
            "id": "secure/reference",
            "body": "**The protected reference is exact-page approved.**"
        }),
    );

    let applied = parse_json(&env.ok(
        &["--json", "publish", "--apply", "--user-approved"],
        Some(&manifest),
    ));
    assert_eq!(applied["applied"], true);
    assert!(env
        .ok(&["read", "secure/reference"], None)
        .contains("exact-page approved"));
}

#[test]
fn rule_change_requires_the_affected_sections_checks_page() {
    let env = Env::new("rules-missing-checks");
    env.init("rules-missing-checks");
    let revision = env.wiki_revision("rules-missing-checks");
    let manifest = publish_manifest(
        &revision,
        serde_json::json!({
            "op": "create",
            "id": "style/new-rule",
            "body": "**The new rule has no verification workflow yet.**"
        }),
    );
    let error = env.fails(&["rules", "propose"], Some(&manifest));
    assert!(
        error.contains("missing checks") || error.contains("no checks page"),
        "{error}"
    );
}
