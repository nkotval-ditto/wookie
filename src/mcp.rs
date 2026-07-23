//! MCP server over stdio (newline-delimited JSON-RPC 2.0). A thin mirror of
//! the CLI: every tool resolves a wiki the same way the CLI does, then calls
//! into `commands`. Hand-rolled on purpose — the protocol surface we need is
//! four methods, not worth an async runtime.

use crate::{commands, config, publish, retrieval, sessions, settings, wiki};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, Read, Write};

const MAX_MCP_FRAME_BYTES: u64 = 16 * 1024 * 1024;
const MCP_PROTOCOL_VERSION: &str = "2025-06-18";
const STRUCTURED_RESULT_TEXT: &str = "Structured result available in structuredContent.";
const PUBLISH_RECOVERY_STATUS_SCHEMA: &str = "wookie.publish-recovery-status/v1";
const PUBLISH_RECOVERY_SCHEMA: &str = "wookie.publish-recovery/v1";

fn successful_tool_result(text: String) -> Value {
    match serde_json::from_str::<Value>(&text) {
        Ok(value) if value.is_object() => json!({
            "content": [{ "type": "text", "text": STRUCTURED_RESULT_TEXT }],
            "structuredContent": value,
            "isError": false,
        }),
        Ok(value) => json!({
            "content": [{ "type": "text", "text": text }],
            "structuredContent": { "value": value },
            "isError": false,
        }),
        Err(_) => json!({
            "content": [{ "type": "text", "text": text }],
            "structuredContent": { "message": text },
            "isError": false,
        }),
    }
}

fn add_object_schema(text: &str, schema: &str) -> Result<String> {
    let mut value: Value = serde_json::from_str(text)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| anyhow!("expected an object-valued command result"))?;
    object.insert("schema".into(), Value::String(schema.into()));
    Ok(value.to_string())
}

pub fn serve() -> Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut input = stdin.lock();

    loop {
        let mut line = String::new();
        let bytes = (&mut input)
            .take(MAX_MCP_FRAME_BYTES + 1)
            .read_line(&mut line)?;
        if bytes == 0 {
            break;
        }
        if bytes as u64 > MAX_MCP_FRAME_BYTES {
            return Err(anyhow!(
                "MCP request frame exceeds {MAX_MCP_FRAME_BYTES} bytes"
            ));
        }
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(json!({}));

        let response = match method {
            "initialize" => {
                Some(json!({
                    // structuredContent is part of the version Wookie
                    // implements. Never echo an older/unknown client value:
                    // doing so would claim compatibility while returning only
                    // a short text pointer for object-valued tool results.
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "wookie", "version": env!("CARGO_PKG_VERSION") },
                }))
            }
            "ping" => Some(json!({})),
            "tools/list" => Some(json!({ "tools": tool_defs() })),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                let result = call_tool(name, &args);
                Some(match result {
                    Ok(text) => successful_tool_result(text),
                    Err(e) => json!({
                        "content": [{ "type": "text", "text": format!("Error: {e:#}") }],
                        "isError": true,
                    }),
                })
            }
            _ => None,
        };

        // Notifications (no id) never get a response.
        let Some(id) = id else { continue };
        let payload = match response {
            Some(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            None => json!({
                "jsonrpc": "2.0", "id": id,
                "error": { "code": -32601, "message": format!("method not found: {method}") },
            }),
        };
        writeln!(stdout, "{payload}")?;
        stdout.flush()?;
    }
    Ok(())
}

fn schema(required: &[&str], props: Value) -> Value {
    json!({
        "type": "object",
        "properties": props,
        "required": required,
        "additionalProperties": false,
    })
}

fn wiki_props(mut extra: Value) -> Value {
    let base = json!({
        "wiki": { "type": "string", "description": "Wiki slug. Omit to resolve from cwd." },
        "cwd": { "type": "string", "description": "Directory used to resolve which wiki applies. Defaults to the server's cwd." },
    });
    if let (Some(b), Some(e)) = (base.as_object(), extra.as_object_mut()) {
        for (k, v) in b {
            e.entry(k.clone()).or_insert(v.clone());
        }
    }
    extra
}

fn coordination_enabled(w: &wiki::Wiki) -> Result<()> {
    anyhow::ensure!(
        w.sessions.enabled,
        "session coordination is disabled for wiki '{}' (sessions.enabled=false)",
        w.slug
    );
    Ok(())
}

fn hours_to_seconds(hours: u64) -> Result<u64> {
    let seconds = hours
        .checked_mul(60 * 60)
        .ok_or_else(|| anyhow!("hour value is too large"))?;
    let value = i64::try_from(seconds).map_err(|_| anyhow!("hour value is too large"))?;
    if chrono::Duration::try_seconds(value).is_none() {
        return Err(anyhow!("hour value is too large"));
    }
    Ok(seconds)
}

fn days_to_seconds(days: u64) -> Result<u64> {
    let seconds = days
        .checked_mul(24 * 60 * 60)
        .ok_or_else(|| anyhow!("day value is too large"))?;
    let value = i64::try_from(seconds).map_err(|_| anyhow!("day value is too large"))?;
    if chrono::Duration::try_seconds(value).is_none() {
        return Err(anyhow!("day value is too large"));
    }
    Ok(seconds)
}

fn notification_kind(value: Option<&str>, fallback: &str) -> Result<sessions::NotificationKind> {
    Ok(match value.unwrap_or(fallback) {
        "code-change" => sessions::NotificationKind::CodeChange,
        "decision" => sessions::NotificationKind::Decision,
        "blocker" => sessions::NotificationKind::Blocker,
        "handoff" => sessions::NotificationKind::Handoff,
        "warning" => sessions::NotificationKind::Warning,
        "note" => sessions::NotificationKind::Note,
        value => return Err(anyhow!("invalid notification kind '{value}'")),
    })
}

fn importance(value: Option<&str>, fallback: &str) -> Result<sessions::Importance> {
    Ok(match value.unwrap_or(fallback) {
        "low" => sessions::Importance::Low,
        "normal" => sessions::Importance::Normal,
        "high" => sessions::Importance::High,
        value => return Err(anyhow!("invalid notification importance '{value}'")),
    })
}

fn metadata_arg(args: &Value, key: &str) -> Result<BTreeMap<String, String>> {
    args.get(key)
        .map(|value| {
            let object = value
                .as_object()
                .ok_or_else(|| anyhow!("'{key}' must be an object of string values"))?;
            object
                .iter()
                .map(|(key, value)| {
                    value
                        .as_str()
                        .map(|value| (key.clone(), value.to_string()))
                        .ok_or_else(|| anyhow!("metadata value for '{key}' must be a string"))
                })
                .collect()
        })
        .unwrap_or_else(|| Ok(BTreeMap::new()))
}

fn reject_unknown_args(args: &Value, allowed: &[&str]) -> Result<()> {
    let object = args
        .as_object()
        .ok_or_else(|| anyhow!("tool arguments must be a JSON object"))?;
    for key in object.keys() {
        if key != "wiki" && key != "cwd" && !allowed.contains(&key.as_str()) {
            return Err(anyhow!("unknown tool argument '{key}'"));
        }
    }
    Ok(())
}

fn optional_bool(args: &Value, key: &str) -> Result<Option<bool>> {
    args.get(key)
        .map(|value| {
            value
                .as_bool()
                .ok_or_else(|| anyhow!("'{key}' must be a boolean"))
        })
        .transpose()
}

fn bool_arg(args: &Value, key: &str, default: bool) -> Result<bool> {
    Ok(optional_bool(args, key)?.unwrap_or(default))
}

fn optional_u64(args: &Value, key: &str) -> Result<Option<u64>> {
    args.get(key)
        .map(|value| {
            value
                .as_u64()
                .ok_or_else(|| anyhow!("'{key}' must be a non-negative integer"))
        })
        .transpose()
}

fn u64_arg(args: &Value, key: &str, default: u64) -> Result<u64> {
    Ok(optional_u64(args, key)?.unwrap_or(default))
}

fn optional_usize(args: &Value, key: &str) -> Result<Option<usize>> {
    optional_u64(args, key)?
        .map(|value| {
            usize::try_from(value).map_err(|_| anyhow!("'{key}' is too large for this platform"))
        })
        .transpose()
}

fn notification_limits(settings: &config::SessionSettings) -> sessions::NotificationLimits {
    sessions::NotificationLimits {
        max_summary_bytes: settings.max_summary_bytes,
        max_body_bytes: settings.max_body_bytes,
        max_paths: settings.max_paths,
        max_path_bytes: settings.max_path_bytes,
        max_targets: settings.max_targets,
        max_idempotency_key_bytes: settings.max_idempotency_key_bytes,
        max_metadata_entries: settings.max_metadata_entries,
        max_metadata_key_bytes: settings.max_metadata_key_bytes,
        max_metadata_value_bytes: settings.max_metadata_value_bytes,
        max_git_dirty_paths: settings.max_git_dirty_paths,
        max_git_branch_bytes: settings.max_git_branch_bytes,
        max_git_commit_bytes: settings.max_git_commit_bytes,
        max_git_worktree_bytes: settings.max_git_worktree_bytes,
    }
}

fn tool_defs() -> Vec<Value> {
    vec![
        json!({
            "name": "wiki_list",
            "description": "List all wookie wikis with page counts and project roots.",
            "inputSchema": schema(&[], json!({})),
        }),
        json!({
            "name": "wiki_context",
            "description": "Explicit exhaustive wiki catalog. Prefer wiki_prime at task start.",
            "inputSchema": schema(&[], wiki_props(json!({}))),
        }),
        json!({
            "name": "wiki_prime",
            "description": "Bounded, task-aware standing instructions and ranked page map. Use this at task start.",
            "inputSchema": schema(&["query"], wiki_props(json!({
                "query": { "type": "string", "maxLength": retrieval::MAX_QUERY_BYTES },
                "tokens": { "type": "integer", "minimum": 1, "maximum": config::MAX_RETRIEVAL_TOKENS },
                "instruction_tokens": { "type": "integer", "minimum": 1, "maximum": config::MAX_RETRIEVAL_TOKENS },
                "limit": { "type": "integer", "minimum": 1, "maximum": config::MAX_SEARCH_LIMIT },
                "max_per_section": { "type": "integer", "minimum": 1, "maximum": config::MAX_SEARCH_LIMIT },
                "since": { "type": "string", "description": "Prior state_hash; matching state omits unchanged section structure." },
                "context_hash": { "type": "string", "description": "Exact query/options/state hash required for a nonzero cursor." },
                "cursor": { "type": "integer", "minimum": 0 },
            }))),
        }),
        json!({
            "name": "session_start",
            "description": "Start an agent coordination session. Retain the returned id for polling and publishing notifications during the task.",
            "inputSchema": schema(&[], wiki_props(json!({
                "agent": { "type": "string", "description": "Agent host/type, such as codex or claude." },
                "label": { "type": "string", "description": "Optional short purpose for the session." },
                "lookback_hours": { "type": "integer", "minimum": 0, "maximum": config::MAX_SESSION_LOOKBACK_HOURS, "description": "Include notifications this many hours before session creation." },
                "heartbeat_on_activity": { "type": "boolean", "description": "Override automatic activity heartbeats for this session." },
            }))),
        }),
        json!({
            "name": "session_list",
            "description": "List a bounded page of agent coordination sessions for this wiki.",
            "inputSchema": schema(&[], wiki_props(json!({
                "statuses": { "type": "array", "items": { "type": "string" } },
                "agents": { "type": "array", "items": { "type": "string" } },
                "label_contains": { "type": "string" },
                "created_after": { "type": "string", "description": "RFC3339 timestamp." },
                "active_after": { "type": "string", "description": "RFC3339 timestamp." },
                "active_before": { "type": "string", "description": "RFC3339 timestamp." },
                "limit": { "type": "integer", "minimum": 1, "maximum": sessions::MAX_SESSION_RESPONSE_LIMIT },
                "cursor": { "type": "integer", "minimum": 0, "description": "Continuation offset returned by a previous session_list call." },
                "newest_first": { "type": "boolean", "default": true },
            }))),
        }),
        json!({
            "name": "session_show",
            "description": "Show one agent coordination session and a bounded page of its newest notification summaries.",
            "inputSchema": schema(&["session"], wiki_props(json!({
                "session": { "type": "string" },
                "limit": { "type": "integer", "minimum": 1, "maximum": sessions::MAX_SESSION_RESPONSE_LIMIT },
                "cursor": { "type": "integer", "minimum": 0, "description": "Continuation offset returned by a previous session_show call." },
            }))),
        }),
        json!({
            "name": "session_heartbeat",
            "description": "Record activity for a long-running coordination session.",
            "inputSchema": schema(&["session"], wiki_props(json!({
                "session": { "type": "string" },
                "force": { "type": "boolean", "description": "Bypass the session debounce." },
            }))),
        }),
        json!({
            "name": "session_close",
            "description": "Mark an agent coordination session closed.",
            "inputSchema": schema(&["session"], wiki_props(json!({
                "session": { "type": "string" },
            }))),
        }),
        json!({
            "name": "session_prune",
            "description": "Preview or remove old sessions. dry_run defaults true; closed_only defaults true.",
            "inputSchema": schema(&[], wiki_props(json!({
                "older_than_days": { "type": "integer", "minimum": 1 },
                "inactive_before": { "type": "string", "description": "RFC3339 cutoff." },
                "keep_latest": { "type": "integer", "minimum": 0 },
                "closed_only": { "type": "boolean" },
                "dry_run": { "type": "boolean" },
            }))),
        }),
        json!({
            "name": "notify",
            "description": "Publish an append-only notification so other active sessions can judge whether your work affects them.",
            "inputSchema": schema(&["session", "summary"], wiki_props(json!({
                "session": { "type": "string", "description": "Source session id." },
                "summary": { "type": "string", "description": "One-line relevance summary." },
                "kind": { "type": "string", "enum": ["code-change", "decision", "blocker", "handoff", "warning", "note"] },
                "importance": { "type": "string", "enum": ["low", "normal", "high"] },
                "paths": { "type": "array", "items": { "type": "string" }, "description": "Affected project paths." },
                "body": { "type": "string", "description": "Optional fuller Markdown details." },
                "targets": { "type": "array", "items": { "type": "string" }, "description": "Receiving session ids; omit to broadcast." },
                "idempotency_key": { "type": "string", "description": "Stable retry key scoped to the source session." },
                "metadata": { "type": "object", "additionalProperties": { "type": "string" } },
                "include_git_context": { "type": "boolean", "description": "Attach branch, commit, worktree and dirty paths; defaults to configuration." },
            }))),
        }),
        json!({
            "name": "notifications",
            "description": "Poll compact notification metadata newest-first. Defaults to unread notices from other sessions; all=true includes history.",
            "inputSchema": schema(&["session"], wiki_props(json!({
                "session": { "type": "string" },
                "all": { "type": "boolean" },
                "source_sessions": { "type": "array", "items": { "type": "string" } },
                "kinds": { "type": "array", "items": { "type": "string", "enum": ["code-change", "decision", "blocker", "handoff", "warning", "note"] } },
                "min_importance": { "type": "string", "enum": ["low", "normal", "high"] },
                "path_prefixes": { "type": "array", "items": { "type": "string" } },
                "branches": { "type": "array", "items": { "type": "string" } },
                "metadata": { "type": "object", "additionalProperties": { "type": "string" } },
                "created_after": { "type": "string" },
                "created_before": { "type": "string" },
                "max_age_hours": { "type": "integer", "minimum": 0 },
                "lookback_hours": { "type": "integer", "minimum": 0, "maximum": config::MAX_SESSION_LOOKBACK_HOURS },
                "text": { "type": "string" },
                "limit": { "type": "integer", "minimum": 1, "description": "Maximum results; cannot exceed configured sessions.poll_limit." },
                "offset": { "type": "integer", "minimum": 0, "description": "Continuation offset returned by a previous poll using the same filters and order." },
                "newest_first": { "type": "boolean", "default": true, "description": "Return fresh notifications first. Set false for oldest-first history traversal." },
            }))),
        }),
        json!({
            "name": "notification_read",
            "description": "Read a notification's full body and mark it read for the receiving session.",
            "inputSchema": schema(&["session", "id"], wiki_props(json!({
                "session": { "type": "string", "description": "Receiving session id." },
                "id": { "type": "string", "description": "Notification id." },
            }))),
        }),
        json!({
            "name": "notification_dismiss",
            "description": "Mark a notification irrelevant for the receiving session without reading its full body.",
            "inputSchema": schema(&["session", "id"], wiki_props(json!({
                "session": { "type": "string", "description": "Receiving session id." },
                "id": { "type": "string", "description": "Notification id." },
            }))),
        }),
        json!({
            "name": "wiki_toc",
            "description": "Table of contents: every page id with its one-line description.",
            "inputSchema": schema(&[], wiki_props(json!({}))),
        }),
        json!({
            "name": "page_read",
            "description": "Read a wiki page. Set expand=1 (or deeper) to inline the summary of every [[wikilinked]] page.",
            "inputSchema": schema(&["id"], wiki_props(json!({
                "id": { "type": "string", "description": "Page id, e.g. 'scheduler' or 'internals/retry-policy'." },
                "expand": { "type": "integer", "minimum": 0, "maximum": commands::MAX_READ_EXPAND_DEPTH, "description": "Depth of linked context to inline (default 0)." },
            }))),
        }),
        json!({
            "name": "page_new",
            "description": "Create a wiki page. Body markdown may contain [[wikilinks]]; the first paragraph must be a standalone summary. Without a body this creates a stub.",
            "inputSchema": schema(&["id"], wiki_props(json!({
                "id": { "type": "string" },
                "title": { "type": "string" },
                "tags": { "type": "array", "items": { "type": "string" } },
                "sources": { "type": "array", "items": { "type": "string" }, "description": "Project-relative paths this page documents (used by ingest to detect staleness)." },
                "pin": { "type": "boolean", "description": "Pin as always-on instructions: wiki_context inlines the full body. Reserve for rules every session must follow." },
                "pin_level": { "type": "string", "enum": ["instruction", "summary", "discoverable"] },
                "protocol": { "type": "string", "description": "Project-local page protocol to render." },
                "description": { "type": "string", "description": "One-line description shown in tocs (default: derived from the body)." },
                "body": { "type": "string" },
            }))),
        }),
        json!({
            "name": "page_write",
            "description": "Replace (or append to) a page's body. Clears stub status. First paragraph must be a standalone summary.",
            "inputSchema": schema(&["id", "body"], wiki_props(json!({
                "id": { "type": "string" },
                "body": { "type": "string" },
                "append": { "type": "boolean" },
                "sources": { "type": "array", "items": { "type": "string" }, "description": "Replace the page's documented project paths." },
                "pin": { "type": "boolean", "description": "Set or clear the page's pinned (always-on instructions) status." },
                "pin_level": { "type": "string", "enum": ["instruction", "summary", "discoverable"] },
                "description": { "type": "string", "description": "Replace the one-line description." },
            }))),
        }),
        json!({
            "name": "ingest",
            "description": "Sync the wiki with the codebase. Returns a bounded worklist and SHA-256 reconciliation receipt. To mark a completed worklist, call with mark=true and the exact expect_worklist receipt; Wookie recomputes it, runs the audit error gate, and rejects changed project/wiki state.",
            "inputSchema": schema(&[], wiki_props(json!({
                "level": { "type": "string", "enum": ["quick", "standard", "deep"] },
                "mark": { "type": "boolean", "description": "Record a validated reconciliation as the sync point." },
                "recover": { "type": "string", "enum": ["accept", "rollback"], "description": "Resolve a retained ambiguous ingest metadata transaction." },
                "expect_worklist": { "type": "string", "description": "Exact sha256: receipt returned by the worklist being marked." },
                "full": { "type": "boolean", "description": "Force a fresh ingest even if a sync point exists." },
                "since": { "type": "string", "description": "Diff against this commit instead of the recorded sync point." },
                "project_root": { "type": "string", "description": "Explicit project checkout." },
                "limit": { "type": "integer", "minimum": 1, "maximum": 1000, "description": "Maximum displayed items per worklist category." },
                "tokens": { "type": "integer", "minimum": 256, "description": "Approximate response token budget." },
                "all": { "type": "boolean", "description": "Return the exhaustive worklist; cannot be combined with limit or tokens." },
            }))),
        }),
        json!({
            "name": "page_move",
            "description": "Rename/move a page and rewrite all inbound [[wikilinks]].",
            "inputSchema": schema(&["from", "to"], wiki_props(json!({
                "from": { "type": "string" },
                "to": { "type": "string" },
            }))),
        }),
        json!({
            "name": "page_remove",
            "description": "Delete a page. Reports any pages left with dangling links.",
            "inputSchema": schema(&["id"], wiki_props(json!({ "id": { "type": "string" } }))),
        }),
        json!({
            "name": "wiki_expand",
            "description": "Grow the wiki: create every eligible stub for broken [[wikilinks]], then return a bounded worklist. Set all=true only for exhaustive output.",
            "inputSchema": schema(&[], wiki_props(json!({
                "id": { "type": "string", "description": "Limit to one page's broken links. Omit for wiki-wide." },
                "limit": { "type": "integer", "minimum": 1, "maximum": config::MAX_SEARCH_LIMIT, "description": "Maximum returned IDs per category." },
                "tokens": { "type": "integer", "minimum": commands::MIN_EXPAND_TOKENS, "maximum": config::MAX_RETRIEVAL_TOKENS, "description": "Maximum estimated response tokens." },
                "all": { "type": "boolean", "description": "Return every created, current-stub, and locked-target ID." },
            }))),
        }),
        json!({
            "name": "search",
            "description": "Bounded deterministic page search. Set all=true for the legacy exhaustive regex dump.",
            "inputSchema": schema(&["query"], wiki_props(json!({
                "query": { "type": "string", "maxLength": retrieval::MAX_QUERY_BYTES },
                "tag": { "type": "string", "description": "Only pages with this tag." },
                "limit": { "type": "integer", "minimum": 1, "maximum": config::MAX_SEARCH_LIMIT },
                "tokens": { "type": "integer", "minimum": 1, "maximum": config::MAX_RETRIEVAL_TOKENS },
                "excerpt_lines": { "type": "integer", "minimum": 1, "maximum": config::MAX_EXCERPT_LINES },
                "cursor": { "type": "integer", "minimum": 0 },
                "context_hash": { "type": "string" },
                "regex": { "type": "boolean" },
                "all": { "type": "boolean" },
            }))),
        }),
        json!({
            "name": "links",
            "description": "Outlinks and backlinks of a page.",
            "inputSchema": schema(&["id"], wiki_props(json!({ "id": { "type": "string" } }))),
        }),
        json!({
            "name": "critique",
            "description": "Get a critique briefing: the project's rules sections (each with its checks page) plus the current changes to check them against. EXECUTE the briefing it returns and report violations per its output contract.",
            "inputSchema": schema(&[], wiki_props(json!({
                "section": { "type": "string", "description": "Only this rules section." },
                "since": { "type": "string", "description": "Critique changes since this git ref instead of uncommitted changes." },
                "staged": { "type": "boolean", "description": "Critique staged changes." },
                "paths": { "type": "array", "items": { "type": "string" }, "description": "Critique explicit paths instead of a git diff." },
                "project_root": { "type": "string" },
                "revision": { "type": "string", "description": "Exact target revision; combine with since for BASE..TARGET." },
                "tokens": { "type": "integer", "minimum": 256, "maximum": 1000000, "description": "Maximum estimated tokens for the compact briefing." },
                "all": { "type": "boolean", "description": "Explicitly include complete checks and rule bodies." },
            }))),
        }),
        json!({
            "name": "unlock_section",
            "description": "Temporarily unlock a locked (rules) section so its pages can be edited. NEVER call this without the user's explicit permission in the current conversation. Relocks automatically.",
            "inputSchema": schema(&["section", "user_approved"], wiki_props(json!({
                "section": { "type": "string" },
                "minutes": { "type": "integer", "description": "Minutes until auto-relock (default 15)." },
                "user_approved": { "type": "boolean", "description": "Set true ONLY if the user explicitly approved editing this section in the current conversation. Anything else is a violation." },
            }))),
        }),
        json!({
            "name": "lock_section",
            "description": "Relock a section immediately after finishing approved edits.",
            "inputSchema": schema(&["section"], wiki_props(json!({ "section": { "type": "string" } }))),
        }),
        json!({
            "name": "doctor",
            "description": "Versioned health and provenance report. fix=true repairs frontmatter mechanically first.",
            "inputSchema": schema(&[], wiki_props(json!({
                "fix": { "type": "boolean" },
                "project_root": { "type": "string" },
                "revision": { "type": "string" },
            }))),
        }),
        json!({
            "name": "wiki_status",
            "description": "Compact operator dashboard backed by the same diagnostics as doctor.",
            "inputSchema": schema(&[], wiki_props(json!({
                "project_root": { "type": "string" },
                "revision": { "type": "string" },
            }))),
        }),
        json!({
            "name": "protocol_list",
            "description": "List project-local page protocols.",
            "inputSchema": schema(&[], wiki_props(json!({}))),
        }),
        json!({
            "name": "protocol_show",
            "description": "Read one project-local page protocol.",
            "inputSchema": schema(&["name"], wiki_props(json!({"name": {"type": "string"}}))),
        }),
        json!({
            "name": "protocol_write",
            "description": "Validate and create or replace one Markdown-only page protocol.",
            "inputSchema": schema(&["name", "template"], wiki_props(json!({
                "name": {"type": "string"},
                "template": {"type": "string"},
            }))),
        }),
        json!({
            "name": "protocol_remove",
            "description": "Remove one project-local page protocol.",
            "inputSchema": schema(&["name"], wiki_props(json!({"name": {"type": "string"}}))),
        }),
        json!({
            "name": "publish",
            "description": "Validate a wookie.changeset/v1 manifest; apply=false is side-effect-free. Rule changes require explicit approval.",
            "inputSchema": schema(&["manifest"], wiki_props(json!({
                "manifest": {"type": "string", "description": "TOML or JSON change set."},
                "apply": {"type": "boolean"},
                "user_approved": {"type": "boolean"},
                "expect_plan": {"type": "string", "description": "Optional review_token returned by a prior dry run; apply rejects any drift."},
                "tokens": {"type": "integer", "minimum": 256, "description": "Maximum estimated response tokens for a dry run."},
                "full_diff": {"type": "boolean", "description": "Explicitly return exhaustive before/after page images; incompatible with tokens or apply."},
            }))),
        }),
        json!({
            "name": "publish_recovery_status",
            "description": "Inspect compact interrupted-publish recovery metadata without exposing page bodies or changing wiki state.",
            "inputSchema": schema(&[], wiki_props(json!({}))),
        }),
        json!({
            "name": "publish_recover",
            "description": "Explicitly roll back or accept an interrupted publish. Recovery refuses live or unverifiable locks; force_stale_lock only permits removal of a demonstrably dead owner.",
            "inputSchema": schema(&["action"], wiki_props(json!({
                "action": {
                    "type": "string",
                    "enum": ["rollback", "accept"],
                    "description": "rollback restores recorded before-images; accept verifies and retains the complete after-images."
                },
                "force_stale_lock": {
                    "type": "boolean",
                    "description": "Allow removal of a demonstrably stale lock after independently confirming no publisher is running."
                }
            }))),
        }),
        json!({
            "name": "rules_propose",
            "description": "Store and preflight an exact rule-change manifest without unlocking a rules section.",
            "inputSchema": schema(&["manifest"], wiki_props(json!({
                "manifest": {"type": "string", "description": "TOML or JSON wookie.changeset/v1 manifest."},
                "tokens": {"type": "integer", "minimum": 256},
                "full_diff": {"type": "boolean", "description": "Explicitly return exhaustive before/after page images; incompatible with tokens."}
            }))),
        }),
        json!({
            "name": "rules_review",
            "description": "Re-run preflight for a stored rule-change proposal.",
            "inputSchema": schema(&["id"], wiki_props(json!({
                "id": {"type": "string", "description": "Proposal id returned by rules_propose."},
                "tokens": {"type": "integer", "minimum": 256},
                "full_diff": {"type": "boolean", "description": "Explicitly return exhaustive before/after page images; incompatible with tokens."}
            }))),
        }),
        json!({
            "name": "rules_apply",
            "description": "Apply a reviewed rule proposal and relock affected sections. Never call without explicit user approval for this exact proposal.",
            "inputSchema": schema(&["id", "user_approved"], wiki_props(json!({
                "id": {"type": "string"},
                "user_approved": {"type": "boolean", "description": "Set true only after the user explicitly approved this exact rule proposal in the current conversation."}
            }))),
        }),
        json!({
            "name": "config_show",
            "description": "Show per-wiki overrides/effective settings or global defaults.",
            "inputSchema": schema(&[], wiki_props(json!({
                "global": { "type": "boolean" },
                "effective": { "type": "boolean", "description": "Resolve global defaults plus per-wiki overrides." },
            }))),
        }),
        json!({
            "name": "config_get",
            "description": "Read one dotted configuration key.",
            "inputSchema": schema(&["key"], wiki_props(json!({
                "key": { "type": "string" },
                "global": { "type": "boolean" },
                "effective": { "type": "boolean" },
            }))),
        }),
        json!({
            "name": "config_set",
            "description": "Set one validated dotted TOML configuration value. sections.* requires explicit user approval.",
            "inputSchema": schema(&["key", "value"], wiki_props(json!({
                "key": { "type": "string" },
                "value": { "description": "TOML scalar/array/table, or a string with string=true." },
                "global": { "type": "boolean" },
                "string": { "type": "boolean" },
                "user_approved": { "type": "boolean", "description": "Set true for sections.* ONLY after explicit user approval." },
            }))),
        }),
        json!({
            "name": "config_unset",
            "description": "Remove a per-wiki override or reset a global default.",
            "inputSchema": schema(&["key"], wiki_props(json!({
                "key": { "type": "string" },
                "global": { "type": "boolean" },
                "user_approved": { "type": "boolean", "description": "Set true for sections.* ONLY after explicit user approval." },
            }))),
        }),
        json!({
            "name": "config_keys",
            "description": "List supported dotted configuration keys.",
            "inputSchema": schema(&[], json!({
                "global": { "type": "boolean" },
            })),
        }),
    ]
}

fn validate_schema_value(value: &Value, schema: &Value, path: &str) -> Result<()> {
    if let Some(expected) = schema.get("type").and_then(Value::as_str) {
        let valid = match expected {
            "string" => value.is_string(),
            "boolean" => value.is_boolean(),
            "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
            "array" => value.is_array(),
            "object" => value.is_object(),
            _ => true,
        };
        if !valid {
            bail!("'{path}' must be a {expected}");
        }
    }
    if let Some(allowed) = schema.get("enum").and_then(Value::as_array) {
        if !allowed.contains(value) {
            bail!("'{path}' has an unsupported value");
        }
    }
    if let Some(maximum) = schema.get("maxLength").and_then(Value::as_u64) {
        let string = value
            .as_str()
            .ok_or_else(|| anyhow!("'{path}' must be a string"))?;
        if string.len() as u64 > maximum {
            bail!("'{path}' must be at most {maximum} bytes");
        }
    }
    if let Some(minimum) = schema.get("minimum").and_then(Value::as_i64) {
        let below_minimum = if let Some(actual) = value.as_i64() {
            actual < minimum
        } else if let Some(actual) = value.as_u64() {
            minimum >= 0 && actual < minimum as u64
        } else {
            bail!("'{path}' must be an integer");
        };
        if below_minimum {
            bail!("'{path}' must be at least {minimum}");
        }
    }
    if let Some(maximum) = schema.get("maximum").and_then(Value::as_u64) {
        let above_maximum = if let Some(actual) = value.as_u64() {
            actual > maximum
        } else if let Some(actual) = value.as_i64() {
            actual >= 0 && actual as u64 > maximum
        } else {
            bail!("'{path}' must be an integer");
        };
        if above_maximum {
            bail!("'{path}' must be at most {maximum}");
        }
    }
    if let Some(items) = schema.get("items") {
        for (index, item) in value.as_array().into_iter().flatten().enumerate() {
            validate_schema_value(item, items, &format!("{path}[{index}]"))?;
        }
    }
    if let Some(object) = value.as_object() {
        let properties = schema
            .get("properties")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for key in required.iter().filter_map(Value::as_str) {
                if !object.contains_key(key) {
                    bail!("missing required argument '{key}'");
                }
            }
        }
        let additional = schema.get("additionalProperties");
        for (key, child) in object {
            if let Some(child_schema) = properties.get(key) {
                validate_schema_value(child, child_schema, key)?;
            } else if additional == Some(&Value::Bool(false)) {
                bail!("unknown tool argument '{key}'");
            } else if let Some(child_schema) = additional.filter(|value| value.is_object()) {
                validate_schema_value(child, child_schema, key)?;
            }
        }
    }
    Ok(())
}

fn validate_tool_args(name: &str, args: &Value) -> Result<()> {
    let Some(definition) = tool_defs()
        .into_iter()
        .find(|definition| definition["name"].as_str() == Some(name))
    else {
        return Ok(());
    };
    validate_schema_value(args, &definition["inputSchema"], "arguments")
}

fn call_tool(name: &str, args: &Value) -> Result<String> {
    validate_tool_args(name, args)?;
    let home = config::wookie_home()?;
    let str_arg = |k: &str| args.get(k).and_then(Value::as_str).map(str::to_string);
    let cwd = str_arg("cwd")
        .map(std::path::PathBuf::from)
        .unwrap_or(std::env::current_dir()?);
    let wiki_flag = str_arg("wiki");
    let resolve = || wiki::resolve(&home, wiki_flag.as_deref(), &cwd);
    let require = |k: &str| str_arg(k).ok_or_else(|| anyhow!("missing required argument '{k}'"));
    let list_arg = |k: &str| -> Result<Vec<String>> {
        let Some(value) = args.get(k) else {
            return Ok(vec![]);
        };
        let values = value
            .as_array()
            .ok_or_else(|| anyhow!("'{k}' must be an array of strings"))?;
        values
            .iter()
            .enumerate()
            .map(|(index, value)| {
                value
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| anyhow!("'{k}[{index}]' must be a string"))
            })
            .collect()
    };

    match name {
        "wiki_list" => commands::list(&home, true),
        "wiki_context" => commands::context(&resolve()?, true),
        "wiki_prime" => commands::prime(
            &resolve()?,
            &commands::PrimeOptions {
                query: require("query")?,
                tokens: optional_usize(args, "tokens")?,
                instruction_tokens: optional_usize(args, "instruction_tokens")?,
                limit: optional_usize(args, "limit")?,
                max_per_section: optional_usize(args, "max_per_section")?,
                since: str_arg("since"),
                cursor: optional_usize(args, "cursor")?.unwrap_or(0),
                context_hash: str_arg("context_hash"),
                cwd: Some(cwd.clone()),
            },
            true,
        ),
        "session_start" => {
            let w = resolve()?;
            coordination_enabled(&w)?;
            if w.sessions.auto_prune_on_start {
                if let Some(days) = w.sessions.retention_days {
                    sessions::prune_sessions(
                        &w,
                        &sessions::PruneRequest {
                            older_than_seconds: Some(days_to_seconds(days)?),
                            dry_run: false,
                            ..sessions::PruneRequest::default()
                        },
                    )?;
                }
            }
            let lookback_hours =
                u64_arg(args, "lookback_hours", w.sessions.initial_lookback_hours)?;
            if lookback_hours > config::MAX_SESSION_LOOKBACK_HOURS {
                bail!(
                    "session lookback is too large; it must not exceed {} hours",
                    config::MAX_SESSION_LOOKBACK_HOURS
                );
            }
            let session = sessions::start_with_options(
                &w,
                sessions::StartOptions {
                    agent: str_arg("agent"),
                    label: str_arg("label"),
                    notification_lookback_seconds: hours_to_seconds(lookback_hours)?,
                    activity_debounce_seconds: w.sessions.activity_debounce_seconds,
                    heartbeat_on_activity: bool_arg(
                        args,
                        "heartbeat_on_activity",
                        w.sessions.heartbeat_on_activity,
                    )?,
                    max_agent_bytes: w.sessions.max_agent_bytes,
                    max_label_bytes: w.sessions.max_label_bytes,
                },
            )?;
            Ok(json!({
                "message": format!("Started session '{}'.", session.id),
                "session": session,
            })
            .to_string())
        }
        "session_list" => {
            let w = resolve()?;
            coordination_enabled(&w)?;
            let result = sessions::list_with_options(
                &w,
                &sessions::SessionListRequest {
                    statuses: list_arg("statuses")?,
                    agents: list_arg("agents")?,
                    label_contains: str_arg("label_contains"),
                    created_after: str_arg("created_after"),
                    active_after: str_arg("active_after"),
                    active_before: str_arg("active_before"),
                    limit: optional_usize(args, "limit")?,
                    cursor: optional_usize(args, "cursor")?.unwrap_or_default(),
                    newest_first: bool_arg(args, "newest_first", true)?,
                },
            )?;
            sessions::format_session_list(&result, true)
        }
        "session_show" => {
            let w = resolve()?;
            coordination_enabled(&w)?;
            sessions::show_with_options(
                &w,
                &require("session")?,
                &sessions::SessionShowRequest {
                    limit: optional_usize(args, "limit")?,
                    cursor: optional_usize(args, "cursor")?.unwrap_or_default(),
                },
                true,
            )
        }
        "session_heartbeat" => {
            let w = resolve()?;
            coordination_enabled(&w)?;
            let session =
                sessions::heartbeat(&w, &require("session")?, bool_arg(args, "force", false)?)?;
            Ok(serde_json::to_string(&session)?)
        }
        "session_close" => {
            let w = resolve()?;
            coordination_enabled(&w)?;
            sessions::close(&w, &require("session")?, true)
        }
        "session_prune" => {
            let w = resolve()?;
            coordination_enabled(&w)?;
            let inactive_before = str_arg("inactive_before");
            let older_than_seconds = if let Some(days) = optional_u64(args, "older_than_days")? {
                Some(days_to_seconds(days)?)
            } else if inactive_before.is_some() {
                None
            } else {
                Some(days_to_seconds(w.sessions.retention_days.unwrap_or(30))?)
            };
            let result = sessions::prune_sessions(
                &w,
                &sessions::PruneRequest {
                    closed_only: bool_arg(args, "closed_only", true)?,
                    older_than_seconds,
                    inactive_before,
                    keep_latest: optional_usize(args, "keep_latest")?.unwrap_or(0),
                    dry_run: bool_arg(args, "dry_run", true)?,
                },
            )?;
            sessions::format_prune(&result, true)
        }
        "notify" => {
            reject_unknown_args(
                args,
                &[
                    "session",
                    "summary",
                    "kind",
                    "importance",
                    "paths",
                    "body",
                    "targets",
                    "idempotency_key",
                    "metadata",
                    "include_git_context",
                ],
            )?;
            let w = resolve()?;
            coordination_enabled(&w)?;
            let include_git =
                bool_arg(args, "include_git_context", w.sessions.include_git_context)?;
            let notification = sessions::notify_with_request(
                &w,
                sessions::NotifyRequest {
                    source_session: require("session")?,
                    summary: require("summary")?,
                    kind: notification_kind(str_arg("kind").as_deref(), &w.sessions.default_kind)?,
                    importance: importance(
                        str_arg("importance").as_deref(),
                        &w.sessions.default_importance,
                    )?,
                    paths: list_arg("paths")?,
                    body: str_arg("body"),
                    targets: list_arg("targets")?,
                    idempotency_key: str_arg("idempotency_key"),
                    git: include_git
                        .then(|| sessions::capture_git_context(&w, &cwd))
                        .flatten(),
                    metadata: metadata_arg(args, "metadata")?,
                    limits: notification_limits(&w.sessions),
                },
            )?;
            Ok(json!({"notification": notification.meta}).to_string())
        }
        "notifications" => {
            let w = resolve()?;
            coordination_enabled(&w)?;
            let lookback_hours = optional_u64(args, "lookback_hours")?;
            if lookback_hours.is_some_and(|hours| hours > config::MAX_SESSION_LOOKBACK_HOURS) {
                bail!(
                    "notification lookback is too large; it must not exceed {} hours",
                    config::MAX_SESSION_LOOKBACK_HOURS
                );
            }
            let requested_limit = optional_usize(args, "limit")?.unwrap_or(w.sessions.poll_limit);
            anyhow::ensure!(
                (1..=w.sessions.poll_limit).contains(&requested_limit),
                "requested limit {} must be between 1 and configured sessions.poll_limit {}",
                requested_limit,
                w.sessions.poll_limit
            );
            let kinds = list_arg("kinds")?
                .iter()
                .map(|value| notification_kind(Some(value), "note"))
                .collect::<Result<Vec<_>>>()?;
            let min_importance = str_arg("min_importance")
                .as_deref()
                .map(|value| importance(Some(value), "normal"))
                .transpose()?;
            let result = sessions::inbox_with_request(
                &w,
                &sessions::InboxRequest {
                    session_id: require("session")?,
                    include_acknowledged: bool_arg(args, "all", false)?,
                    lookback_seconds: lookback_hours.map(hours_to_seconds).transpose()?,
                    filter: sessions::NotificationFilter {
                        source_sessions: list_arg("source_sessions")?,
                        kinds,
                        min_importance,
                        path_prefixes: list_arg("path_prefixes")?,
                        branches: list_arg("branches")?,
                        metadata: metadata_arg(args, "metadata")?,
                        created_after: str_arg("created_after"),
                        created_before: str_arg("created_before"),
                        max_age_seconds: optional_u64(args, "max_age_hours")?
                            .map(hours_to_seconds)
                            .transpose()?,
                        text: str_arg("text"),
                    },
                    limit: Some(requested_limit),
                    offset: optional_usize(args, "offset")?.unwrap_or(0),
                    newest_first: bool_arg(args, "newest_first", true)?,
                },
            )?;
            sessions::format_inbox(&result, true)
        }
        "notification_read" => {
            let w = resolve()?;
            coordination_enabled(&w)?;
            sessions::read_notification(&w, &require("session")?, &require("id")?, true)
        }
        "notification_dismiss" => {
            let w = resolve()?;
            coordination_enabled(&w)?;
            sessions::dismiss_notification(&w, &require("session")?, &require("id")?, true)
        }
        "wiki_toc" => commands::toc(&resolve()?, true),
        "page_read" => {
            let expand = optional_usize(args, "expand")?.unwrap_or(0);
            commands::read(&resolve()?, &require("id")?, expand, true)
        }
        "page_new" => commands::new_page(
            &resolve()?,
            &require("id")?,
            str_arg("title"),
            str_arg("description"),
            list_arg("tags")?,
            list_arg("sources")?,
            match str_arg("pin_level").as_deref() {
                Some("summary") => Some(crate::page::PinLevel::Summary),
                Some("instruction") => Some(crate::page::PinLevel::Instruction),
                Some("discoverable") => Some(crate::page::PinLevel::Discoverable),
                Some(value) => return Err(anyhow!("invalid pin_level '{value}'")),
                None => bool_arg(args, "pin", false)?.then_some(crate::page::PinLevel::Instruction),
            },
            str_arg("protocol").as_deref(),
            str_arg("body"),
            true,
        ),
        "page_write" => {
            let append = bool_arg(args, "append", false)?;
            let sources = args.get("sources").and_then(Value::as_array).map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            });
            let pin = match str_arg("pin_level").as_deref() {
                Some("summary") => commands::PinChange::Set(crate::page::PinLevel::Summary),
                Some("instruction") => commands::PinChange::Set(crate::page::PinLevel::Instruction),
                Some("discoverable") => {
                    commands::PinChange::Set(crate::page::PinLevel::Discoverable)
                }
                Some(value) => return Err(anyhow!("invalid pin_level '{value}'")),
                None => match optional_bool(args, "pin")? {
                    Some(true) => commands::PinChange::Set(crate::page::PinLevel::Instruction),
                    Some(false) => commands::PinChange::Clear,
                    None => commands::PinChange::Keep,
                },
            };
            commands::write(
                &resolve()?,
                &require("id")?,
                &require("body")?,
                append,
                sources,
                pin,
                str_arg("description"),
                true,
            )
        }
        "ingest" => {
            let level = match str_arg("level").as_deref() {
                Some("quick") => commands::IngestLevel::Quick,
                Some("deep") => commands::IngestLevel::Deep,
                _ => commands::IngestLevel::Standard,
            };
            let mark = bool_arg(args, "mark", false)?;
            let recover = match str_arg("recover").as_deref() {
                Some("accept") => Some(commands::IngestRecoveryAction::Accept),
                Some("rollback") => Some(commands::IngestRecoveryAction::Rollback),
                Some(value) => return Err(anyhow!("invalid ingest recovery action '{value}'")),
                None => None,
            };
            let full = bool_arg(args, "full", false)?;
            let all = bool_arg(args, "all", false)?;
            let mut w = resolve()?;
            commands::ingest(
                &mut w,
                &cwd,
                &commands::IngestOptions {
                    project_root: str_arg("project_root").as_deref().map(std::path::Path::new),
                    level,
                    mark,
                    recover,
                    expect_worklist: str_arg("expect_worklist").as_deref(),
                    full,
                    since: str_arg("since").as_deref(),
                    limit: optional_usize(args, "limit")?,
                    tokens: optional_usize(args, "tokens")?,
                    all,
                    json: true,
                },
            )
        }
        "page_move" => commands::mv(&resolve()?, &require("from")?, &require("to")?, true),
        "page_remove" => commands::rm(&resolve()?, &require("id")?, true),
        "wiki_expand" => commands::expand(
            &resolve()?,
            &commands::ExpandOptions {
                id: str_arg("id").as_deref(),
                limit: optional_usize(args, "limit")?,
                tokens: optional_usize(args, "tokens")?,
                all: bool_arg(args, "all", false)?,
            },
            true,
        ),
        "search" => commands::search_with_options(
            &resolve()?,
            &commands::SearchOptions {
                query: require("query")?,
                tag: str_arg("tag"),
                limit: optional_usize(args, "limit")?,
                tokens: optional_usize(args, "tokens")?,
                excerpt_lines: optional_usize(args, "excerpt_lines")?,
                cursor: optional_usize(args, "cursor")?.unwrap_or(0),
                context_hash: str_arg("context_hash"),
                regex: bool_arg(args, "regex", false)?,
                all: bool_arg(args, "all", false)?,
                cwd: Some(cwd.clone()),
            },
            true,
        ),
        "links" => commands::links(&resolve()?, &require("id")?, true),
        "critique" => {
            let paths: Vec<String> = args
                .get("paths")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let staged = bool_arg(args, "staged", false)?;
            commands::critique(
                &resolve()?,
                &cwd,
                &commands::CritiqueOptions {
                    project_root: str_arg("project_root").as_deref().map(std::path::Path::new),
                    revision: str_arg("revision").as_deref(),
                    section: str_arg("section").as_deref(),
                    since: str_arg("since").as_deref(),
                    staged,
                    paths: &paths,
                    tokens: optional_usize(args, "tokens")?,
                    all: bool_arg(args, "all", false)?,
                    json: true,
                },
            )
        }
        "unlock_section" => {
            if !bool_arg(args, "user_approved", false)? {
                return Err(anyhow!(
                    "refusing to unlock: pass user_approved=true only after the user explicitly approved editing this section in the current conversation"
                ));
            }
            let minutes = u64_arg(args, "minutes", 15)?;
            commands::unlock(&resolve()?, &require("section")?, minutes, false)
        }
        "lock_section" => commands::lock(&resolve()?, &require("section")?, false),
        "doctor" => {
            let fix = bool_arg(args, "fix", false)?;
            commands::doctor_with_options(
                &resolve()?,
                fix,
                &crate::audit::AuditOptions {
                    project_root: str_arg("project_root").map(std::path::PathBuf::from),
                    project_revision: str_arg("revision"),
                },
                true,
            )
            .map(|(report, _)| report)
        }
        "wiki_status" => commands::status(
            &resolve()?,
            &crate::audit::AuditOptions {
                project_root: str_arg("project_root").map(std::path::PathBuf::from),
                project_revision: str_arg("revision"),
            },
            true,
        )
        .map(|(report, _)| report),
        "protocol_list" => commands::protocol_list(&resolve()?, true),
        "protocol_show" => commands::protocol_show(&resolve()?, &require("name")?, true),
        "protocol_write" => {
            commands::protocol_write(&resolve()?, &require("name")?, &require("template")?, true)
        }
        "protocol_remove" => commands::protocol_remove(&resolve()?, &require("name")?, true),
        "publish" => commands::publish_changes(
            &resolve()?,
            &require("manifest")?,
            bool_arg(args, "apply", false)?,
            bool_arg(args, "user_approved", false)?,
            str_arg("expect_plan").as_deref(),
            &commands::PublishOutputOptions {
                tokens: optional_usize(args, "tokens")?,
                full_diff: bool_arg(args, "full_diff", false)?,
            },
            true,
        ),
        "publish_recovery_status" => {
            let recovery = publish::recovery_status(&resolve()?)?;
            Ok(json!({
                "schema": PUBLISH_RECOVERY_STATUS_SCHEMA,
                "recovery_required": recovery.is_some(),
                "recovery": recovery,
            })
            .to_string())
        }
        "publish_recover" => {
            let action = match require("action")?.as_str() {
                "rollback" => publish::RecoveryAction::Rollback,
                "accept" => publish::RecoveryAction::Accept,
                value => return Err(anyhow!("invalid publish recovery action '{value}'")),
            };
            let result = commands::publish_recover(
                &resolve()?,
                action,
                bool_arg(args, "force_stale_lock", false)?,
                true,
            )?;
            add_object_schema(&result, PUBLISH_RECOVERY_SCHEMA)
        }
        "rules_propose" => commands::rules_propose(
            &resolve()?,
            &require("manifest")?,
            &commands::PublishOutputOptions {
                tokens: optional_usize(args, "tokens")?,
                full_diff: bool_arg(args, "full_diff", false)?,
            },
            true,
        ),
        "rules_review" => commands::rules_review(
            &resolve()?,
            &require("id")?,
            &commands::PublishOutputOptions {
                tokens: optional_usize(args, "tokens")?,
                full_diff: bool_arg(args, "full_diff", false)?,
            },
            true,
        ),
        "rules_apply" => {
            if !bool_arg(args, "user_approved", false)? {
                return Err(anyhow!(
                    "refusing to apply rules proposal: user_approved=true is required after explicit approval of this exact proposal"
                ));
            }
            commands::rules_apply(&resolve()?, &require("id")?, true, true)
        }
        "config_show" => {
            let global = bool_arg(args, "global", false)?;
            let effective = bool_arg(args, "effective", false)?;
            if global {
                anyhow::ensure!(!effective, "effective applies only to a resolved wiki");
                settings::show_global(&home, true)
            } else {
                settings::show_wiki(&resolve()?, effective, true)
            }
        }
        "config_get" => {
            let global = bool_arg(args, "global", false)?;
            let effective = bool_arg(args, "effective", false)?;
            if global {
                anyhow::ensure!(!effective, "effective applies only to a resolved wiki");
                settings::get_global(&home, &require("key")?, true)
            } else {
                settings::get_wiki(&resolve()?, &require("key")?, effective, true)
            }
        }
        "config_set" => {
            let global = bool_arg(args, "global", false)?;
            let force_string = bool_arg(args, "string", false)?;
            let approved = bool_arg(args, "user_approved", false)?;
            let value = args
                .get("value")
                .ok_or_else(|| anyhow!("missing required argument 'value'"))?;
            let raw = value
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| value.to_string());
            if global {
                anyhow::ensure!(
                    !approved,
                    "user_approved applies only to per-wiki sections.* settings"
                );
                settings::set_global(&home, &require("key")?, &raw, force_string, true)
            } else {
                let mut w = resolve()?;
                settings::set_wiki(&mut w, &require("key")?, &raw, force_string, approved, true)
            }
        }
        "config_unset" => {
            let global = bool_arg(args, "global", false)?;
            let approved = bool_arg(args, "user_approved", false)?;
            if global {
                anyhow::ensure!(
                    !approved,
                    "user_approved applies only to per-wiki sections.* settings"
                );
                settings::unset_global(&home, &require("key")?, true)
            } else {
                let mut w = resolve()?;
                settings::unset_wiki(&mut w, &require("key")?, approved, true)
            }
        }
        "config_keys" => {
            let global = bool_arg(args, "global", false)?;
            Ok(json!({"keys": settings::keys(global).lines().collect::<Vec<_>>()}).to_string())
        }
        _ => Err(anyhow!("unknown tool: {name}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_results_are_not_duplicated_in_text_content() {
        let result = successful_tool_result(r#"{"schema":"example/v1","items":[1,2]}"#.into());
        assert_eq!(result["content"][0]["text"], STRUCTURED_RESULT_TEXT);
        assert_eq!(result["structuredContent"]["schema"], "example/v1");
        assert_ne!(
            result["content"][0]["text"],
            result["structuredContent"].to_string()
        );
    }

    #[test]
    fn non_json_text_remains_human_readable() {
        let result = successful_tool_result("plain command result".into());
        assert_eq!(result["content"][0]["text"], "plain command result");
        assert_eq!(
            result["structuredContent"]["message"],
            "plain command result"
        );
    }

    #[test]
    fn recovery_tool_schemas_are_strict() {
        validate_tool_args(
            "publish_recover",
            &json!({"action": "rollback", "force_stale_lock": false}),
        )
        .unwrap();
        assert!(validate_tool_args("publish_recover", &json!({})).is_err());
        assert!(validate_tool_args("publish_recover", &json!({"action": "discard"})).is_err());
        assert!(validate_tool_args(
            "publish_recovery_status",
            &json!({"force_stale_lock": false})
        )
        .is_err());
    }

    #[test]
    fn retrieval_tool_schemas_enforce_materialization_ceilings() {
        validate_tool_args(
            "search",
            &json!({
                "query": "cache",
                "limit": config::MAX_SEARCH_LIMIT,
                "excerpt_lines": config::MAX_EXCERPT_LINES,
                "tokens": config::MAX_RETRIEVAL_TOKENS
            }),
        )
        .unwrap();
        validate_tool_args(
            "wiki_prime",
            &json!({
                "query": "cache",
                "tokens": config::MAX_RETRIEVAL_TOKENS,
                "instruction_tokens": config::MAX_RETRIEVAL_TOKENS,
                "max_per_section": config::MAX_SEARCH_LIMIT,
                "context_hash": "sha256:test"
            }),
        )
        .unwrap();
        validate_tool_args(
            "page_new",
            &json!({
                "id": "reference",
                "body": "**Reference.** Read it on demand.",
                "pin_level": "discoverable"
            }),
        )
        .unwrap();
        assert!(validate_tool_args(
            "search",
            &json!({"query": "cache", "limit": config::MAX_SEARCH_LIMIT + 1})
        )
        .is_err());
        assert!(validate_tool_args(
            "search",
            &json!({"query": "cache", "excerpt_lines": config::MAX_EXCERPT_LINES + 1})
        )
        .is_err());
        assert!(validate_tool_args(
            "wiki_prime",
            &json!({"query": "cache", "limit": config::MAX_SEARCH_LIMIT + 1})
        )
        .is_err());
        for tool in ["search", "wiki_prime"] {
            assert!(validate_tool_args(
                tool,
                &json!({"query": "cache", "tokens": config::MAX_RETRIEVAL_TOKENS + 1})
            )
            .is_err());
        }
        assert!(validate_tool_args(
            "wiki_prime",
            &json!({"query": "cache", "max_per_section": config::MAX_SEARCH_LIMIT + 1})
        )
        .is_err());
        assert!(validate_tool_args(
            "search",
            &json!({"query": "x".repeat(retrieval::MAX_QUERY_BYTES + 1)})
        )
        .is_err());

        validate_tool_args(
            "wiki_expand",
            &json!({
                "limit": config::MAX_SEARCH_LIMIT,
                "tokens": commands::MIN_EXPAND_TOKENS,
                "all": false
            }),
        )
        .unwrap();
        assert!(validate_tool_args(
            "wiki_expand",
            &json!({"limit": config::MAX_SEARCH_LIMIT + 1})
        )
        .is_err());
        assert!(validate_tool_args(
            "wiki_expand",
            &json!({"tokens": commands::MIN_EXPAND_TOKENS - 1})
        )
        .is_err());
        assert!(validate_tool_args(
            "wiki_expand",
            &json!({"tokens": config::MAX_RETRIEVAL_TOKENS + 1})
        )
        .is_err());
    }

    #[test]
    fn mutating_recovery_result_gets_a_versioned_schema() {
        let result = add_object_schema(
            r#"{"recovered":true,"previous":null}"#,
            PUBLISH_RECOVERY_SCHEMA,
        )
        .unwrap();
        let result: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(result["schema"], PUBLISH_RECOVERY_SCHEMA);
        assert_eq!(result["recovered"], true);
    }
}
