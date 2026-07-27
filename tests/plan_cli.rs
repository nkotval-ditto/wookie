//! End-to-end coverage for the session-plan lifecycle and its read-only board.

use serde_json::Value;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct TestEnv {
    base: PathBuf,
    home: PathBuf,
    project: PathBuf,
}

impl TestEnv {
    fn new(name: &str) -> Self {
        let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "wookie-plan-cli-{name}-{}-{sequence}",
            std::process::id()
        ));
        let home = base.join("home");
        let project = base.join("project");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        Self {
            base,
            home,
            project,
        }
    }

    fn command(&self, args: &[&str], session: Option<&str>) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_wookie"));
        command
            .args(args)
            .env("WOOKIE_HOME", &self.home)
            .current_dir(&self.project)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(session) = session {
            command.env("WOOKIE_SESSION", session);
        }
        command
    }

    fn run(&self, args: &[&str], stdin: Option<&str>, session: Option<&str>) -> Output {
        let mut command = self.command(args, session);
        if stdin.is_some() {
            command.stdin(Stdio::piped());
        }
        let mut child = command.spawn().unwrap();
        if let Some(stdin) = stdin {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(stdin.as_bytes())
                .unwrap();
        }
        child.wait_with_output().unwrap()
    }

    fn ok(&self, args: &[&str], stdin: Option<&str>, session: Option<&str>) -> String {
        let output = self.run(args, stdin, session);
        assert!(
            output.status.success(),
            "wookie {args:?} failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn json(&self, args: &[&str], stdin: Option<&str>, session: Option<&str>) -> Value {
        let stdout = self.ok(args, stdin, session);
        serde_json::from_str(&stdout)
            .unwrap_or_else(|error| panic!("invalid JSON from wookie {args:?}: {error}\n{stdout}"))
    }

    fn fail(&self, args: &[&str], stdin: Option<&str>, session: Option<&str>) -> String {
        let output = self.run(args, stdin, session);
        assert!(
            !output.status.success(),
            "wookie {args:?} unexpectedly succeeded:\n{}",
            String::from_utf8_lossy(&output.stdout)
        );
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    fn initialize(&self, slug: &str) {
        self.ok(&["init", slug], None, None);
        self.ok(
            &["new", "guides/plan-test"],
            Some(
                "**The plan test guide defines the implementation workflow.** Agents use it to \
                 make each segment independently reviewable.\n\n\
                 ## Verification\n\nRun the plan lifecycle checks.",
            ),
            None,
        );
    }

    fn start_session(&self) -> String {
        self.ok(
            &[
                "session",
                "start",
                "--agent",
                "codex",
                "--label",
                "plan e2e",
                "--id-only",
            ],
            None,
            None,
        )
        .trim()
        .to_string()
    }
}

impl Drop for TestEnv {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn valid_plan() -> &'static str {
    r#"schema = "wookie.plan/v1"
title = "Exercise the plan lifecycle"

[[segments]]
id = "design"
title = "Confirm the plan boundary"
status = "todo"
guide = "guides/plan-test"
justification = "Implementation needs a small and explicit state boundary."
decisions = ["Keep the immutable plan separate from append-only activity."]
verification = "The design segment reaches done before implementation starts."
depends_on = []

[[segments]]
id = "implementation"
title = "Implement and verify the workflow"
status = "todo"
guide = "guides/plan-test"
justification = "The user needs a live, auditable implementation workflow."
decisions = ["Use the CLI as the only mutation surface."]
verification = "The lifecycle and read-only board checks pass."
depends_on = ["design"]
"#
}

fn linear_link() -> &'static str {
    r#"schema = "wookie.plan-linear-link/v1"

[project]
id = "project-plan-test"
url = "https://linear.app/example/project/plan-test"

[[issues]]
segment_id = "design"
id = "TEST-1"
url = "https://linear.app/example/issue/TEST-1/design"
status = "todo"

[[issues]]
segment_id = "implementation"
id = "TEST-2"
url = "https://linear.app/example/issue/TEST-2/implementation"
status = "todo"
"#
}

fn segment<'a>(snapshot: &'a Value, id: &str) -> &'a Value {
    snapshot["segments"]
        .as_array()
        .unwrap()
        .iter()
        .find(|segment| segment["id"] == id)
        .unwrap_or_else(|| panic!("snapshot has no segment {id}: {snapshot}"))
}

#[test]
fn plan_cli_validates_tracks_and_archives_a_session() {
    let env = TestEnv::new("lifecycle");
    env.initialize("plan-lifecycle");
    let session = env.start_session();

    let guide = env.json(
        &[
            "--json",
            "plan",
            "guide",
            "--query",
            "implement a live plan",
        ],
        None,
        None,
    );
    assert_eq!(guide["schema"], "wookie.plan-guide/v1");
    assert_eq!(guide["query"], "implement a live plan");
    assert!(guide["guide"].as_str().unwrap().contains("wookie.plan/v1"));

    let checked = env.json(&["--json", "plan", "check"], Some(valid_plan()), None);
    assert_eq!(checked["schema"], "wookie.plan/v1");
    assert_eq!(checked["segment_count"], 2);
    assert_eq!(
        checked["definition"]["segments"][1]["depends_on"][0],
        "design"
    );

    let attached = env.json(
        &["--json", "plan", "attach"],
        Some(valid_plan()),
        Some(&session),
    );
    assert_eq!(attached["schema"], "wookie.plan-snapshot/v1");
    assert_eq!(attached["session"]["id"], session);
    assert_eq!(segment(&attached, "design")["status"], "todo");
    assert_eq!(
        segment(&attached, "implementation")["blocked_by"][0],
        "design"
    );

    // A retry with an equivalent definition is idempotent, including its
    // append-only attachment activity.
    let plan_file = env.project.join("plan.toml");
    std::fs::write(&plan_file, valid_plan()).unwrap();
    let reattached = env.json(
        &["--json", "plan", "attach", plan_file.to_str().unwrap()],
        None,
        Some(&session),
    );
    assert_eq!(reattached["plan_hash"], attached["plan_hash"]);
    assert_eq!(reattached["events_total"], attached["events_total"]);
    let attachment_events = reattached["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["plan"]["kind"] == "attached")
        .count();
    assert_eq!(attachment_events, 1);

    let shown = env.json(&["--json", "plan", "show"], None, Some(&session));
    assert_eq!(shown["plan_hash"], attached["plan_hash"]);
    assert_eq!(shown["session"]["status"], "active");

    let dependency_error = env.fail(
        &["plan", "update", "implementation", "doing"],
        None,
        Some(&session),
    );
    assert!(
        dependency_error.contains("incomplete dependencies: design"),
        "{dependency_error}"
    );

    let design_doing = env.json(
        &[
            "--json",
            "plan",
            "update",
            "design",
            "doing",
            "--note",
            "Boundary review started",
        ],
        None,
        Some(&session),
    );
    assert_eq!(segment(&design_doing, "design")["status"], "doing");
    env.ok(
        &[
            "plan",
            "update",
            "design",
            "done",
            "--note",
            "Boundary accepted",
        ],
        None,
        Some(&session),
    );
    env.ok(
        &["plan", "update", "implementation", "doing"],
        None,
        Some(&session),
    );

    let logged = env.json(
        &[
            "--json",
            "plan",
            "log",
            "--segment",
            "implementation",
            "--kind",
            "decision",
            "--summary",
            "Kept the browser projection read-only",
        ],
        None,
        Some(&session),
    );
    assert!(logged["events"].as_array().unwrap().iter().any(|event| {
        event["action"] == "plan-log"
            && event["plan"]["segment_id"] == "implementation"
            && event["plan"]["log_kind"] == "decision"
    }));

    let archive_error = env.fail(&["plan", "archive"], None, Some(&session));
    assert!(
        archive_error.contains("1 incomplete segment"),
        "{archive_error}"
    );
    let still_active = env.json(&["--json", "plan", "show"], None, Some(&session));
    assert_eq!(still_active["session"]["status"], "active");

    env.ok(
        &[
            "plan",
            "update",
            "implementation",
            "done",
            "--note",
            "End-to-end verification passed",
        ],
        None,
        Some(&session),
    );
    let archived = env.json(
        &[
            "--json",
            "plan",
            "archive",
            "--summary",
            "Plan lifecycle completed and verified.",
        ],
        None,
        Some(&session),
    );
    assert_eq!(archived["receipt"]["schema"], "wookie.plan-archive/v1");
    assert_eq!(archived["receipt"]["done_segments"], 2);
    assert_eq!(archived["receipt"]["incomplete_segments"], 0);
    assert_eq!(archived["snapshot"]["session"]["status"], "closed");
    assert_eq!(
        archived["archive_path"],
        format!("sessions/{session}/archive.md")
    );

    let closed = env.json(&["--json", "plan", "show"], None, Some(&session));
    assert_eq!(closed["session"]["status"], "closed");
    assert_eq!(segment(&closed, "design")["status"], "done");
    assert_eq!(segment(&closed, "implementation")["status"], "done");

    let archive_path = env
        .home
        .join("plan-lifecycle")
        .join("sessions")
        .join(&session)
        .join("archive.md");
    let archive = std::fs::read_to_string(archive_path).unwrap();
    assert!(archive.contains("# Exercise the plan lifecycle"));
    assert!(archive.contains("Plan lifecycle completed and verified."));
    assert!(archive.contains("## Final plan"));
    assert!(archive.contains("## Session activity"));
    assert!(archive.contains("Kept the browser projection read-only"));
}

#[test]
fn plan_check_rejects_unknown_fields_missing_guides_and_cycles() {
    let env = TestEnv::new("validation");
    env.initialize("plan-validation");

    let unknown_field = valid_plan().replacen(
        "title = \"Exercise the plan lifecycle\"",
        "title = \"Exercise the plan lifecycle\"\nunexpected = true",
        1,
    );
    let error = env.fail(&["plan", "check"], Some(&unknown_field), None);
    assert!(error.contains("unknown field"), "{error}");

    let missing_guide = valid_plan().replace(
        "guide = \"guides/plan-test\"",
        "guide = \"guides/does-not-exist\"",
    );
    let error = env.fail(&["plan", "check"], Some(&missing_guide), None);
    assert!(error.contains("does not exist"), "{error}");

    let cyclic = r#"schema = "wookie.plan/v1"
title = "Reject cyclic dependencies"

[[segments]]
id = "first"
title = "First"
status = "todo"
guide = "guides/plan-test"
justification = "Exercise one side of the cycle."
decisions = ["Do not accept cyclic plans."]
verification = "Validation rejects the plan."
depends_on = ["second"]

[[segments]]
id = "second"
title = "Second"
status = "todo"
guide = "guides/plan-test"
justification = "Exercise the other side of the cycle."
decisions = ["Keep dependency evaluation deterministic."]
verification = "Validation rejects the plan."
depends_on = ["first"]
"#;
    let error = env.fail(&["plan", "check"], Some(cyclic), None);
    assert!(error.contains("dependencies contain a cycle"), "{error}");
}

struct RunningBoard {
    child: Child,
}

impl Drop for RunningBoard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Debug)]
struct HttpResponse {
    status: u16,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

fn raw_http(authority: &str, request: &str) -> std::io::Result<HttpResponse> {
    let mut stream = TcpStream::connect(authority)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(request.as_bytes())?;
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes)?;

    let separator = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| std::io::Error::other("HTTP response has no header terminator"))?;
    let head = std::str::from_utf8(&bytes[..separator])
        .map_err(|_| std::io::Error::other("HTTP response headers are not UTF-8"))?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|status| status.parse::<u16>().ok())
        .ok_or_else(|| std::io::Error::other("HTTP response has no status"))?;
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    Ok(HttpResponse {
        status,
        headers,
        body: bytes[(separator + 4)..].to_vec(),
    })
}

fn http_with_deadline(authority: &str, request: &str) -> HttpResponse {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match raw_http(authority, request) {
            Ok(response) => return response,
            Err(error) if Instant::now() < deadline => {
                let _ = error;
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => panic!("plan board did not answer before deadline: {error}"),
        }
    }
}

fn get(authority: &str, path: &str, extra_headers: &str) -> HttpResponse {
    http_with_deadline(
        authority,
        &format!(
            "GET {path} HTTP/1.1\r\nHost: {authority}\r\n{extra_headers}Connection: close\r\n\r\n"
        ),
    )
}

fn assert_security_headers(response: &HttpResponse) {
    assert_eq!(
        response.headers.get("cache-control").map(String::as_str),
        Some("no-store")
    );
    assert_eq!(
        response
            .headers
            .get("x-content-type-options")
            .map(String::as_str),
        Some("nosniff")
    );
    assert_eq!(
        response.headers.get("x-frame-options").map(String::as_str),
        Some("DENY")
    );
    assert_eq!(
        response
            .headers
            .get("cross-origin-resource-policy")
            .map(String::as_str),
        Some("same-origin")
    );
    let csp = response.headers.get("content-security-policy").unwrap();
    assert!(csp.contains("default-src 'none'"), "{csp}");
    assert!(csp.contains("connect-src 'self'"), "{csp}");
    assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
}

#[test]
fn plan_board_is_loopback_read_only_and_sends_security_headers() {
    let env = TestEnv::new("board");
    env.initialize("plan-board");
    let session = env.start_session();
    env.ok(&["plan", "attach"], Some(valid_plan()), Some(&session));
    env.ok(
        &["plan", "linear", "link"],
        Some(linear_link()),
        Some(&session),
    );

    let mut command = env.command(&["plan", "--no-open", "--port", "0"], Some(&session));
    command.stderr(Stdio::inherit());
    let mut child = command.spawn().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (sender, receiver) = mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut line = String::new();
        let result = BufReader::new(stdout).read_line(&mut line).map(|_| line);
        let _ = sender.send(result);
    });
    let mut board = RunningBoard { child };
    let line = receiver
        .recv_timeout(Duration::from_secs(5))
        .unwrap_or_else(|_| {
            let _ = board.child.kill();
            panic!("timed out waiting for the plan board URL")
        })
        .unwrap();
    let authority = line
        .trim()
        .strip_prefix("Wookie plan board: http://")
        .and_then(|url| url.strip_suffix('/'))
        .unwrap_or_else(|| panic!("unexpected plan board startup output: {line:?}"));
    assert!(authority.starts_with("127.0.0.1:"), "{authority}");

    let index = get(authority, "/", "");
    assert_eq!(index.status, 200);
    assert_security_headers(&index);
    let index_html = String::from_utf8(index.body).unwrap();
    assert!(index_html.contains("Wookie"));
    assert!(index_html.contains("linear-project"));
    assert!(index_html.contains(r#"name="color-scheme""#));
    assert!(index_html.contains(r#"content="light""#));
    assert!(!index_html.contains("connection-status"));

    let stylesheet = get(authority, "/plan.css", "");
    assert_eq!(stylesheet.status, 200);
    assert_security_headers(&stylesheet);
    let stylesheet = String::from_utf8(stylesheet.body).unwrap();
    assert!(stylesheet.contains(r#""SFMono-Regular""#));
    assert!(stylesheet.contains(r#".plan-card[data-status="todo"]"#));
    assert!(stylesheet.contains(r#".plan-card[data-status="doing"]"#));
    assert!(stylesheet.contains(r#".plan-card[data-status="blocked"]"#));
    assert!(stylesheet.contains(r#".plan-card[data-status="done"]"#));

    let snapshot = get(authority, "/api/snapshot", "");
    assert_eq!(snapshot.status, 200);
    assert_security_headers(&snapshot);
    let snapshot_json: Value = serde_json::from_slice(&snapshot.body).unwrap();
    assert_eq!(snapshot_json["schema"], "wookie.plan-snapshot/v1");
    assert_eq!(snapshot_json["session"]["id"], session);
    assert_eq!(
        snapshot_json["linear"]["project"]["id"],
        "project-plan-test"
    );
    assert_eq!(snapshot_json["linear"]["issues"][0]["id"], "TEST-1");
    let etag = snapshot.headers.get("etag").unwrap();

    let not_modified = get(
        authority,
        "/api/snapshot",
        &format!("If-None-Match: {etag}\r\n"),
    );
    assert_eq!(not_modified.status, 304);
    assert_security_headers(&not_modified);

    let guide = get(authority, "/api/guides/design", "");
    assert_eq!(guide.status, 200);
    let guide_json: Value = serde_json::from_slice(&guide.body).unwrap();
    assert_eq!(guide_json["segment_id"], "design");
    assert_eq!(guide_json["page"]["id"], "guides/plan-test");

    let post = http_with_deadline(
        authority,
        &format!(
            "POST /api/snapshot HTTP/1.1\r\nHost: {authority}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        ),
    );
    assert_eq!(post.status, 405);
    assert_eq!(
        post.headers.get("allow").map(String::as_str),
        Some("GET, HEAD")
    );

    let foreign_host = http_with_deadline(
        authority,
        "GET /api/snapshot HTTP/1.1\r\nHost: example.test\r\nConnection: close\r\n\r\n",
    );
    assert_eq!(foreign_host.status, 403);
    assert_security_headers(&foreign_host);

    let foreign_origin = get(
        authority,
        "/api/snapshot",
        "Origin: https://example.test\r\n",
    );
    assert_eq!(foreign_origin.status, 403);

    board.child.kill().unwrap();
    assert!(!board.child.wait().unwrap().success());
}
