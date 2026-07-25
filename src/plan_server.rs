//! Read-only, loopback-only web projection of a session plan.
//!
//! The browser is deliberately not a mutation surface. Agents update plans
//! through the CLI or MCP, while this server repeatedly folds the immutable
//! plan and append-only session events into a fresh snapshot.

use crate::plan::{self, SnapshotOptions};
use crate::wiki::Wiki;
use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write as _;
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
use std::path::Path;
use std::process::Command;
use tiny_http::{Header, Method, Request, Response, ResponseBox, Server, StatusCode};

const INDEX_HTML: &str = include_str!("../assets/plan/index.html");
const PLAN_CSS: &str = include_str!("../assets/plan/plan.css");
const PLAN_JS: &str = include_str!("../assets/plan/plan.js");

const MAX_SNAPSHOT_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_GUIDE_BODY_BYTES: usize = 512 * 1024;
const MAX_GUIDE_RESPONSE_BYTES: usize = 768 * 1024;
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; img-src 'self'; font-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

#[derive(Clone, Copy, Debug)]
pub struct PlanServerOptions {
    /// TCP port on 127.0.0.1. Zero asks the OS to select a free port.
    pub port: u16,
    /// Launch the generated loopback URL in the system browser.
    pub open: bool,
}

impl Default for PlanServerOptions {
    fn default() -> Self {
        Self {
            port: 0,
            open: true,
        }
    }
}

#[derive(Debug, Eq, PartialEq)]
enum Route<'a> {
    Index,
    Styles,
    Script,
    Snapshot,
    Guide(&'a str),
    Health,
    NotFound,
}

#[derive(Serialize)]
struct GuidePageResponse<'a> {
    segment_id: &'a str,
    page: GuidePage<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    warning: Option<&'static str>,
}

#[derive(Serialize)]
struct GuidePage<'a> {
    id: &'a str,
    title: &'a str,
    description: &'a str,
    body: &'a str,
    tags: &'a [String],
}

pub fn serve(wiki: &Wiki, session_id: &str, options: PlanServerOptions) -> Result<()> {
    // Fail before opening a browser if the session or attached plan is invalid.
    plan::snapshot(wiki, session_id, SnapshotOptions::default())?;

    let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, options.port))
        .with_context(|| format!("binding plan board to 127.0.0.1:{}", options.port))?;
    let server = Server::from_listener(listener, None)
        .map_err(|error| anyhow!("starting plan board server: {error}"))?;
    let address = server
        .server_addr()
        .to_ip()
        .context("plan board did not bind an IP socket")?;
    let authority = format!("127.0.0.1:{}", address.port());
    let origin = format!("http://{authority}");
    let url = format!("{origin}/");

    println!("Wookie plan board: {url}");
    std::io::stdout()
        .flush()
        .context("printing plan board URL")?;

    if options.open {
        if let Err(error) = launch_browser(&url) {
            eprintln!("warning: could not open the plan board browser: {error:#}");
            eprintln!("open this URL manually: {url}");
        }
    }

    for request in server.incoming_requests() {
        if handle_request(request, wiki, session_id, &authority, &origin) {
            break;
        }
    }
    Ok(())
}

fn handle_request(
    request: Request,
    wiki: &Wiki,
    session_id: &str,
    authority: &str,
    origin: &str,
) -> bool {
    let response = response_for(&request, wiki, session_id, authority, origin);
    let should_stop = response.status_code() == StatusCode(410);
    if let Err(error) = request.respond(response) {
        eprintln!("warning: plan board response failed: {error}");
    }
    should_stop
}

fn response_for(
    request: &Request,
    wiki: &Wiki,
    session_id: &str,
    authority: &str,
    origin: &str,
) -> ResponseBox {
    if request
        .remote_addr()
        .is_some_and(|address| !address.ip().is_loopback())
    {
        return json_error(403, "loopback clients only");
    }
    if !valid_host(request, authority) {
        return json_error(403, "invalid host");
    }
    if !valid_origin(request, origin) {
        return json_error(403, "invalid origin");
    }
    if is_cross_site(request) {
        return json_error(403, "cross-site requests are not allowed");
    }
    if !matches!(request.method(), Method::Get | Method::Head) {
        let mut response = json_error(405, "method not allowed");
        response.add_header(header("Allow", "GET, HEAD"));
        return response;
    }

    match classify_route(request.url()) {
        Route::Index => response(200, "text/html; charset=utf-8", INDEX_HTML.as_bytes()),
        Route::Styles => response(200, "text/css; charset=utf-8", PLAN_CSS.as_bytes()),
        Route::Script => response(200, "text/javascript; charset=utf-8", PLAN_JS.as_bytes()),
        Route::Health => response(
            200,
            "application/json; charset=utf-8",
            br#"{"status":"ok"}"#,
        ),
        Route::Snapshot => snapshot_response(request, wiki, session_id),
        Route::Guide(segment_id) => guide_response(wiki, session_id, segment_id),
        Route::NotFound => json_error(404, "not found"),
    }
}

fn snapshot_response(request: &Request, wiki: &Wiki, session_id: &str) -> ResponseBox {
    let snapshot = match plan::snapshot(wiki, session_id, SnapshotOptions::default()) {
        Ok(snapshot) => snapshot,
        Err(_) => {
            return snapshot_load_error(wiki, session_id);
        }
    };
    let body = match serde_json::to_vec(&snapshot) {
        Ok(body) if body.len() <= MAX_SNAPSHOT_RESPONSE_BYTES => body,
        Ok(_) => return json_error(413, "plan snapshot exceeds the board response limit"),
        Err(_) => {
            eprintln!("warning: plan board could not encode its snapshot");
            return json_error(500, "plan snapshot is unavailable");
        }
    };
    let etag = format!("\"{:x}\"", Sha256::digest(&body));
    if header_values(request, "If-None-Match").any(|value| etag_matches(value, &etag)) {
        let mut response = response(304, "application/json; charset=utf-8", &[]);
        response.add_header(header("ETag", &etag));
        return response;
    }
    let mut response = response(200, "application/json; charset=utf-8", &body);
    response.add_header(header("ETag", &etag));
    response
}

fn guide_response(wiki: &Wiki, session_id: &str, segment_id: &str) -> ResponseBox {
    if !valid_segment_id(segment_id) {
        return json_error(404, "unknown plan segment");
    }
    let snapshot = match plan::snapshot(wiki, session_id, SnapshotOptions::default()) {
        Ok(snapshot) => snapshot,
        Err(_) => return snapshot_load_error(wiki, session_id),
    };
    let Some(segment) = snapshot
        .segments
        .iter()
        .find(|segment| segment.id == segment_id)
    else {
        return json_error(404, "unknown plan segment");
    };
    let page = match wiki.load_page(&segment.guide) {
        Ok(page) => page,
        Err(_) if page_is_definitively_missing(wiki, &segment.guide) => {
            return json_error(404, "guide page is unavailable");
        }
        Err(_) => {
            eprintln!("warning: plan board could not load a guide page");
            return json_error(500, "guide page is temporarily unavailable");
        }
    };
    if page.body.len() > MAX_GUIDE_BODY_BYTES {
        return json_error(
            413,
            "guide page is too large for the board; use `wookie read`",
        );
    }
    let payload = GuidePageResponse {
        segment_id,
        page: GuidePage {
            id: &page.id,
            title: &page.fm.title,
            description: &page.fm.description,
            body: &page.body,
            tags: &page.fm.tags,
        },
        warning: (page.fm.status.as_deref() == Some("stub"))
            .then_some("This guide is currently a stub."),
    };
    match serde_json::to_vec(&payload) {
        Ok(body) if body.len() <= MAX_GUIDE_RESPONSE_BYTES => {
            response(200, "application/json; charset=utf-8", &body)
        }
        Ok(_) => json_error(
            413,
            "guide response is too large for the board; use `wookie read`",
        ),
        Err(_) => json_error(500, "guide page is temporarily unavailable"),
    }
}

fn snapshot_load_error(wiki: &Wiki, session_id: &str) -> ResponseBox {
    if required_plan_file_is_definitively_missing(wiki, session_id, "session.toml")
        || required_plan_file_is_definitively_missing(wiki, session_id, "plan.toml")
    {
        eprintln!("warning: plan board session or plan was removed");
        json_error(410, "plan session is no longer available")
    } else {
        eprintln!("warning: plan board could not load its snapshot");
        json_error(500, "plan snapshot is temporarily unavailable")
    }
}

fn required_plan_file_is_definitively_missing(
    wiki: &Wiki,
    session_id: &str,
    file_name: &str,
) -> bool {
    crate::sessions::session_file_path(wiki, session_id, file_name)
        .ok()
        .is_some_and(|path| path_is_definitively_missing(&path))
}

fn page_is_definitively_missing(wiki: &Wiki, page_id: &str) -> bool {
    wiki.page_path(page_id)
        .ok()
        .is_some_and(|path| path_is_definitively_missing(&path))
}

fn path_is_definitively_missing(path: &Path) -> bool {
    fs::symlink_metadata(path).is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
}

fn classify_route(url: &str) -> Route<'_> {
    let path = url.split_once('?').map_or(url, |(path, _)| path);
    match path {
        "/" | "/index.html" => Route::Index,
        "/plan.css" => Route::Styles,
        "/plan.js" => Route::Script,
        "/api/snapshot" => Route::Snapshot,
        "/healthz" => Route::Health,
        _ => path
            .strip_prefix("/api/guides/")
            .filter(|segment| !segment.is_empty() && !segment.contains('/'))
            .map_or(Route::NotFound, Route::Guide),
    }
}

fn valid_segment_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
}

fn valid_host(request: &Request, authority: &str) -> bool {
    let mut hosts = header_values(request, "Host");
    matches!(hosts.next(), Some(host) if host.eq_ignore_ascii_case(authority))
        && hosts.next().is_none()
}

fn valid_origin(request: &Request, origin: &str) -> bool {
    let mut origins = header_values(request, "Origin");
    match origins.next() {
        None => true,
        Some(value) => value.eq_ignore_ascii_case(origin) && origins.next().is_none(),
    }
}

fn is_cross_site(request: &Request) -> bool {
    header_values(request, "Sec-Fetch-Site").any(|value| value.eq_ignore_ascii_case("cross-site"))
}

fn header_values<'a>(request: &'a Request, name: &'static str) -> impl Iterator<Item = &'a str> {
    request
        .headers()
        .iter()
        .filter(move |header| header.field.equiv(name))
        .map(|header| header.value.as_str())
}

fn etag_matches(header_value: &str, etag: &str) -> bool {
    header_value.split(',').any(|candidate| {
        let candidate = candidate.trim();
        candidate == "*" || candidate == etag || candidate.strip_prefix("W/") == Some(etag)
    })
}

fn response(status: u16, content_type: &str, body: &[u8]) -> ResponseBox {
    let mut response = Response::from_data(body.to_vec())
        .with_status_code(StatusCode(status))
        .boxed();
    response.add_header(header("Content-Type", content_type));
    for (name, value) in [
        ("Cache-Control", "no-store"),
        ("X-Content-Type-Options", "nosniff"),
        ("X-Frame-Options", "DENY"),
        ("Referrer-Policy", "no-referrer"),
        (
            "Permissions-Policy",
            "camera=(), microphone=(), geolocation=()",
        ),
        ("Cross-Origin-Resource-Policy", "same-origin"),
        ("Cross-Origin-Opener-Policy", "same-origin"),
        ("Content-Security-Policy", CSP),
    ] {
        response.add_header(header(name, value));
    }
    response
}

fn json_error(status: u16, message: &str) -> ResponseBox {
    let body = serde_json::json!({ "error": message }).to_string();
    response(status, "application/json; charset=utf-8", body.as_bytes())
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes())
        .expect("static plan-board headers must be valid ASCII")
}

#[cfg(target_os = "macos")]
fn launch_browser(url: &str) -> Result<()> {
    Command::new("open")
        .arg(url)
        .spawn()
        .context("launching `open`")?;
    Ok(())
}

#[cfg(target_os = "windows")]
fn launch_browser(url: &str) -> Result<()> {
    Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn()
        .context("launching the default browser")?;
    Ok(())
}

#[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
fn launch_browser(url: &str) -> Result<()> {
    Command::new("xdg-open")
        .arg(url)
        .spawn()
        .context("launching `xdg-open`")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::{Frontmatter, Page};
    use crate::sessions::{self, StartOptions};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use tiny_http::TestRequest;

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        home: PathBuf,
        wiki: Wiki,
        session_id: String,
    }

    impl Fixture {
        fn new() -> Self {
            let home = std::env::temp_dir().join(format!(
                "wookie-plan-server-test-{}-{}",
                std::process::id(),
                TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            let wiki_dir = home.join("test");
            fs::create_dir_all(wiki_dir.join("pages/architecture")).unwrap();
            fs::write(
                wiki_dir.join("wookie.toml"),
                "name = \"test\"\nauto_commit = false\nproject_roots = []\n",
            )
            .unwrap();
            let wiki = crate::wiki::open(&home, "test").unwrap();
            let mut guide = Page {
                id: "architecture/guide".into(),
                fm: Frontmatter {
                    title: "Guide".into(),
                    description: "A real implementation guide.".into(),
                    created: "2026-07-25".into(),
                    updated: "2026-07-25".into(),
                    ..Frontmatter::default()
                },
                body: "**Guide** explains the implementation boundary.".into(),
            };
            wiki.save_page_raw(&mut guide, false).unwrap();
            let session = sessions::start_with_options(
                &wiki,
                StartOptions {
                    agent: Some("test".into()),
                    activity_debounce_seconds: 60,
                    ..StartOptions::default()
                },
            )
            .unwrap();
            plan::attach(&wiki, &session.id, Self::plan()).unwrap();
            Self {
                home,
                wiki,
                session_id: session.id,
            }
        }

        fn plan() -> &'static str {
            r#"schema = "wookie.plan/v1"
title = "Test plan board"

[[segments]]
id = "build"
title = "Build the feature"
status = "todo"
guide = "architecture/guide"
justification = "The feature needs an implementation."
decisions = ["Keep the board read-only."]
verification = "Run the focused tests."
"#
        }

        fn request() -> Request {
            TestRequest::new().into()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.home);
        }
    }

    fn request_with_headers(headers: &[(&str, &str)]) -> Request {
        headers
            .iter()
            .fold(TestRequest::new(), |request, (name, value)| {
                request.with_header(header(name, value))
            })
            .into()
    }

    #[test]
    fn route_allowlist_does_not_decode_or_map_paths() {
        assert_eq!(classify_route("/"), Route::Index);
        assert_eq!(classify_route("/api/snapshot?poll=1"), Route::Snapshot);
        assert_eq!(
            classify_route("/api/guides/implementation"),
            Route::Guide("implementation")
        );
        assert_eq!(classify_route("/api/guides/a/b"), Route::NotFound);
        assert_eq!(
            classify_route("/api/guides/%2e%2e%2fsecret"),
            Route::Guide("%2e%2e%2fsecret")
        );
        assert!(!valid_segment_id("%2e%2e%2fsecret"));
        assert_eq!(classify_route("/../wookie.toml"), Route::NotFound);
    }

    #[test]
    fn segment_ids_are_small_lowercase_kebab_case_values() {
        assert!(valid_segment_id("implement-core"));
        assert!(valid_segment_id("phase-2"));
        assert!(valid_segment_id("phase--retry"));
        for invalid in ["", "-phase", "phase-", "Phase", "phase_two", "../a"] {
            assert!(!valid_segment_id(invalid), "{invalid}");
        }
    }

    #[test]
    fn host_and_origin_are_exact_and_singular() {
        let authority = "127.0.0.1:4317";
        let origin = "http://127.0.0.1:4317";
        let valid = request_with_headers(&[("Host", authority), ("Origin", origin)]);
        assert!(valid_host(&valid, authority));
        assert!(valid_origin(&valid, origin));

        let missing_host = request_with_headers(&[]);
        assert!(!valid_host(&missing_host, authority));
        assert!(valid_origin(&missing_host, origin));

        let duplicate_host = request_with_headers(&[("Host", authority), ("Host", authority)]);
        assert!(!valid_host(&duplicate_host, authority));

        let hostile =
            request_with_headers(&[("Host", "example.test"), ("Origin", "https://example.test")]);
        assert!(!valid_host(&hostile, authority));
        assert!(!valid_origin(&hostile, origin));

        let duplicate_origin =
            request_with_headers(&[("Host", authority), ("Origin", origin), ("Origin", origin)]);
        assert!(!valid_origin(&duplicate_origin, origin));
    }

    #[test]
    fn cross_site_fetch_metadata_is_rejected() {
        let cross_site = request_with_headers(&[("Sec-Fetch-Site", "cross-site")]);
        assert!(is_cross_site(&cross_site));
        let same_origin = request_with_headers(&[("Sec-Fetch-Site", "same-origin")]);
        assert!(!is_cross_site(&same_origin));
    }

    #[test]
    fn etag_matching_accepts_lists_and_weak_validators() {
        let etag = "\"abc\"";
        assert!(etag_matches("\"abc\"", etag));
        assert!(etag_matches("\"other\", W/\"abc\"", etag));
        assert!(etag_matches("*", etag));
        assert!(!etag_matches("\"other\"", etag));
    }

    #[test]
    fn every_response_has_read_only_security_headers() {
        let response = response(200, "text/plain", b"ok");
        for expected in [
            "Cache-Control",
            "X-Content-Type-Options",
            "X-Frame-Options",
            "Referrer-Policy",
            "Permissions-Policy",
            "Cross-Origin-Resource-Policy",
            "Cross-Origin-Opener-Policy",
            "Content-Security-Policy",
        ] {
            assert!(
                response
                    .headers()
                    .iter()
                    .any(|header| header.field.equiv(expected)),
                "missing {expected}"
            );
        }
    }

    #[test]
    fn missing_plan_is_terminal_but_corrupt_plan_is_retryable() {
        let fixture = Fixture::new();
        let plan_path =
            sessions::session_file_path(&fixture.wiki, &fixture.session_id, "plan.toml").unwrap();

        fs::remove_file(&plan_path).unwrap();
        let missing = snapshot_response(&Fixture::request(), &fixture.wiki, &fixture.session_id);
        assert_eq!(missing.status_code(), StatusCode(410));

        fs::write(&plan_path, "not valid plan TOML").unwrap();
        let corrupt = snapshot_response(&Fixture::request(), &fixture.wiki, &fixture.session_id);
        assert_eq!(corrupt.status_code(), StatusCode(500));
    }

    #[test]
    fn guide_endpoint_uses_the_same_missing_vs_corrupt_plan_classification() {
        let fixture = Fixture::new();
        let plan_path =
            sessions::session_file_path(&fixture.wiki, &fixture.session_id, "plan.toml").unwrap();

        fs::remove_file(&plan_path).unwrap();
        let missing = guide_response(&fixture.wiki, &fixture.session_id, "build");
        assert_eq!(missing.status_code(), StatusCode(410));

        fs::write(&plan_path, "not valid plan TOML").unwrap();
        let corrupt = guide_response(&fixture.wiki, &fixture.session_id, "build");
        assert_eq!(corrupt.status_code(), StatusCode(500));
    }

    #[test]
    fn missing_guide_is_not_found_but_invalid_storage_is_retryable() {
        let fixture = Fixture::new();
        let guide_path = fixture.wiki.page_path("architecture/guide").unwrap();

        fs::remove_file(&guide_path).unwrap();
        let missing = guide_response(&fixture.wiki, &fixture.session_id, "build");
        assert_eq!(missing.status_code(), StatusCode(404));

        fs::create_dir(&guide_path).unwrap();
        let invalid = guide_response(&fixture.wiki, &fixture.session_id, "build");
        assert_eq!(invalid.status_code(), StatusCode(500));
    }

    #[test]
    fn final_serialized_guide_payload_has_a_hard_limit() {
        let fixture = Fixture::new();
        let mut page = fixture.wiki.load_page("architecture/guide").unwrap();
        page.body = "\"".repeat(MAX_GUIDE_BODY_BYTES);
        fixture.wiki.save_page_raw(&mut page, false).unwrap();

        let response = guide_response(&fixture.wiki, &fixture.session_id, "build");
        assert_eq!(response.status_code(), StatusCode(413));
    }
}
