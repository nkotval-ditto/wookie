//! MCP server over stdio (newline-delimited JSON-RPC 2.0). A thin mirror of
//! the CLI: every tool resolves a wiki the same way the CLI does, then calls
//! into `commands`. Hand-rolled on purpose — the protocol surface we need is
//! four methods, not worth an async runtime.

use crate::{commands, config, sessions, wiki};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::io::{BufRead, Write};

pub fn serve() -> Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    for line in stdin.lock().lines() {
        let line = line?;
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
                let pv = params
                    .get("protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or("2025-06-18");
                Some(json!({
                    "protocolVersion": pv,
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
                    Ok(text) => json!({
                        "content": [{ "type": "text", "text": text }],
                        "isError": false,
                    }),
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
    json!({ "type": "object", "properties": props, "required": required })
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

fn tool_defs() -> Vec<Value> {
    vec![
        json!({
            "name": "wiki_list",
            "description": "List all wookie wikis with page counts and project roots.",
            "inputSchema": schema(&[], json!({})),
        }),
        json!({
            "name": "wiki_context",
            "description": "Compact digest of a wiki: description, page list with one-line summaries, stub count. Call this first when starting work on a project.",
            "inputSchema": schema(&[], wiki_props(json!({}))),
        }),
        json!({
            "name": "session_start",
            "description": "Start an agent coordination session. Retain the returned id for polling and publishing notifications during the task.",
            "inputSchema": schema(&[], wiki_props(json!({
                "agent": { "type": "string", "description": "Agent host/type, such as codex or claude." },
                "label": { "type": "string", "description": "Optional short purpose for the session." },
            }))),
        }),
        json!({
            "name": "session_list",
            "description": "List agent coordination sessions for this wiki.",
            "inputSchema": schema(&[], wiki_props(json!({}))),
        }),
        json!({
            "name": "session_show",
            "description": "Show one agent coordination session.",
            "inputSchema": schema(&["session"], wiki_props(json!({
                "session": { "type": "string" },
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
            "name": "notify",
            "description": "Publish an append-only notification so other active sessions can judge whether your work affects them.",
            "inputSchema": schema(&["session", "summary"], wiki_props(json!({
                "session": { "type": "string", "description": "Source session id." },
                "summary": { "type": "string", "description": "One-line relevance summary." },
                "kind": { "type": "string", "enum": ["code-change", "decision", "blocker", "handoff", "warning", "note"] },
                "importance": { "type": "string", "enum": ["low", "normal", "high"] },
                "paths": { "type": "array", "items": { "type": "string" }, "description": "Affected project paths." },
                "body": { "type": "string", "description": "Optional fuller Markdown details." },
            }))),
        }),
        json!({
            "name": "notifications",
            "description": "Poll compact notification metadata. Defaults to unread notices from other sessions; all=true includes history.",
            "inputSchema": schema(&["session"], wiki_props(json!({
                "session": { "type": "string" },
                "all": { "type": "boolean" },
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
                "expand": { "type": "integer", "description": "Depth of linked context to inline (default 0)." },
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
                "description": { "type": "string", "description": "Replace the one-line description." },
            }))),
        }),
        json!({
            "name": "ingest",
            "description": "Sync the wiki with the codebase. First run: inventories the project, seeds module stubs, returns a documentation worklist at the chosen level (quick|standard|deep). Later runs: diffs the code since the recorded sync point and returns stale pages. Call with mark=true after completing a worklist.",
            "inputSchema": schema(&[], wiki_props(json!({
                "level": { "type": "string", "enum": ["quick", "standard", "deep"] },
                "mark": { "type": "boolean", "description": "Record the current project commit as the sync point." },
                "full": { "type": "boolean", "description": "Force a fresh ingest even if a sync point exists." },
                "since": { "type": "string", "description": "Diff against this commit instead of the recorded sync point." },
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
            "description": "Grow the wiki: create stub pages for every broken [[wikilink]] (on one page, or wiki-wide) and return the worklist of stubs to fill.",
            "inputSchema": schema(&[], wiki_props(json!({
                "id": { "type": "string", "description": "Limit to one page's broken links. Omit for wiki-wide." },
            }))),
        }),
        json!({
            "name": "search",
            "description": "Search page ids, titles, tags and bodies (case-insensitive regex).",
            "inputSchema": schema(&["query"], wiki_props(json!({
                "query": { "type": "string" },
                "tag": { "type": "string", "description": "Only pages with this tag." },
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
            "description": "Wiki health check: broken links, orphans, stubs, missing summaries. fix=true repairs frontmatter mechanically.",
            "inputSchema": schema(&[], wiki_props(json!({ "fix": { "type": "boolean" } }))),
        }),
    ]
}

fn call_tool(name: &str, args: &Value) -> Result<String> {
    let home = config::wookie_home();
    let str_arg = |k: &str| args.get(k).and_then(Value::as_str).map(str::to_string);
    let cwd = str_arg("cwd")
        .map(std::path::PathBuf::from)
        .unwrap_or(std::env::current_dir()?);
    let wiki_flag = str_arg("wiki");
    let resolve = || wiki::resolve(&home, wiki_flag.as_deref(), &cwd);
    let require = |k: &str| str_arg(k).ok_or_else(|| anyhow!("missing required argument '{k}'"));
    let list_arg = |k: &str| -> Vec<String> {
        args.get(k)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };

    match name {
        "wiki_list" => commands::list(&home, false),
        "wiki_context" => commands::context(&resolve()?, false),
        "session_start" => sessions::start(&resolve()?, str_arg("agent"), str_arg("label"), false),
        "session_list" => sessions::list(&resolve()?, false),
        "session_show" => sessions::show(&resolve()?, &require("session")?, false),
        "session_close" => sessions::close(&resolve()?, &require("session")?, false),
        "notify" => {
            let kind = match str_arg("kind").as_deref() {
                Some("code-change") => sessions::NotificationKind::CodeChange,
                Some("decision") => sessions::NotificationKind::Decision,
                Some("blocker") => sessions::NotificationKind::Blocker,
                Some("handoff") => sessions::NotificationKind::Handoff,
                Some("warning") => sessions::NotificationKind::Warning,
                Some("note") | None => sessions::NotificationKind::Note,
                Some(value) => return Err(anyhow!("invalid notification kind '{value}'")),
            };
            let importance = match str_arg("importance").as_deref() {
                Some("low") => sessions::Importance::Low,
                Some("high") => sessions::Importance::High,
                Some("normal") | None => sessions::Importance::Normal,
                Some(value) => return Err(anyhow!("invalid notification importance '{value}'")),
            };
            sessions::notify(
                &resolve()?,
                &require("session")?,
                &require("summary")?,
                kind,
                importance,
                list_arg("paths"),
                str_arg("body"),
                false,
            )
        }
        "notifications" => sessions::inbox(
            &resolve()?,
            &require("session")?,
            args.get("all").and_then(Value::as_bool).unwrap_or(false),
            false,
        ),
        "notification_read" => {
            sessions::read_notification(&resolve()?, &require("session")?, &require("id")?, false)
        }
        "notification_dismiss" => sessions::dismiss_notification(
            &resolve()?,
            &require("session")?,
            &require("id")?,
            false,
        ),
        "wiki_toc" => commands::toc(&resolve()?, false),
        "page_read" => {
            let expand = args.get("expand").and_then(Value::as_u64).unwrap_or(0) as usize;
            commands::read(&resolve()?, &require("id")?, expand, false)
        }
        "page_new" => commands::new_page(
            &resolve()?,
            &require("id")?,
            str_arg("title"),
            str_arg("description"),
            list_arg("tags"),
            list_arg("sources"),
            args.get("pin").and_then(Value::as_bool).unwrap_or(false),
            str_arg("body"),
            false,
        ),
        "page_write" => {
            let append = args.get("append").and_then(Value::as_bool).unwrap_or(false);
            let sources = args.get("sources").and_then(Value::as_array).map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            });
            let pin = args.get("pin").and_then(Value::as_bool);
            commands::write(
                &resolve()?,
                &require("id")?,
                &require("body")?,
                append,
                sources,
                pin,
                str_arg("description"),
                false,
            )
        }
        "ingest" => {
            let level = match str_arg("level").as_deref() {
                Some("quick") => commands::IngestLevel::Quick,
                Some("deep") => commands::IngestLevel::Deep,
                _ => commands::IngestLevel::Standard,
            };
            let mark = args.get("mark").and_then(Value::as_bool).unwrap_or(false);
            let full = args.get("full").and_then(Value::as_bool).unwrap_or(false);
            let mut w = resolve()?;
            commands::ingest(
                &mut w,
                &cwd,
                level,
                mark,
                full,
                str_arg("since").as_deref(),
                false,
            )
        }
        "page_move" => commands::mv(&resolve()?, &require("from")?, &require("to")?, false),
        "page_remove" => commands::rm(&resolve()?, &require("id")?, false),
        "wiki_expand" => commands::expand(&resolve()?, str_arg("id").as_deref(), false),
        "search" => commands::search(
            &resolve()?,
            &require("query")?,
            str_arg("tag").as_deref(),
            false,
        ),
        "links" => commands::links(&resolve()?, &require("id")?, false),
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
            let staged = args.get("staged").and_then(Value::as_bool).unwrap_or(false);
            commands::critique(
                &resolve()?,
                &cwd,
                str_arg("section").as_deref(),
                str_arg("since").as_deref(),
                staged,
                &paths,
                false,
            )
        }
        "unlock_section" => {
            if !args
                .get("user_approved")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                return Err(anyhow!(
                    "refusing to unlock: pass user_approved=true only after the user explicitly approved editing this section in the current conversation"
                ));
            }
            let minutes = args.get("minutes").and_then(Value::as_u64).unwrap_or(15);
            commands::unlock(&resolve()?, &require("section")?, minutes, false)
        }
        "lock_section" => commands::lock(&resolve()?, &require("section")?, false),
        "doctor" => {
            let fix = args.get("fix").and_then(Value::as_bool).unwrap_or(false);
            commands::doctor(&resolve()?, fix, false).map(|(report, _)| report)
        }
        _ => Err(anyhow!("unknown tool: {name}")),
    }
}
