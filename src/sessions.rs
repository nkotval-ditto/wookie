//! Session-scoped coordination. Durable session metadata and append-only
//! notifications live beside `pages/`; per-session inbox state is local and
//! gitignored so acknowledging a message never creates wiki history noise.

use crate::wiki::Wiki;
use anyhow::{bail, Context, Result};
use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, clap::ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum NotificationKind {
    CodeChange,
    Decision,
    Blocker,
    Handoff,
    Warning,
    Note,
}

impl std::fmt::Display for NotificationKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            NotificationKind::CodeChange => "code-change",
            NotificationKind::Decision => "decision",
            NotificationKind::Blocker => "blocker",
            NotificationKind::Handoff => "handoff",
            NotificationKind::Warning => "warning",
            NotificationKind::Note => "note",
        };
        f.write_str(value)
    }
}

#[derive(Clone, Copy, Debug, clap::ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Importance {
    Low,
    Normal,
    High,
}

impl std::fmt::Display for Importance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Importance::Low => "low",
            Importance::Normal => "normal",
            Importance::High => "high",
        };
        f.write_str(value)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub status: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NotificationMeta {
    pub id: String,
    pub source_session: String,
    pub summary: String,
    pub kind: NotificationKind,
    pub importance: Importance,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Notification {
    pub meta: NotificationMeta,
    pub body: String,
}

#[derive(Default, Serialize, Deserialize)]
struct Inbox {
    #[serde(default)]
    states: BTreeMap<String, String>,
}

fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn clean_field(name: &str, value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        bail!("{name} cannot be empty");
    }
    if value.contains(['\n', '\r']) {
        bail!("{name} must be one line");
    }
    Ok(value.to_string())
}

fn validate_generated_id(id: &str, prefix: &str) -> Result<()> {
    if !id.starts_with(&format!("{prefix}-"))
        || id.len() > 96
        || !id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        bail!("invalid {prefix} id '{id}'");
    }
    Ok(())
}

fn unique_id(prefix: &str, attempt: u32) -> String {
    let stamp = Utc::now().format("%Y%m%d-%H%M%S");
    let entropy = Utc::now().timestamp_subsec_nanos() as u64
        ^ ((std::process::id() as u64) << 16)
        ^ attempt as u64;
    format!("{prefix}-{stamp}-{entropy:08x}")
}

fn sessions_dir(w: &Wiki) -> PathBuf {
    w.dir.join("sessions")
}

fn session_dir(w: &Wiki, id: &str) -> Result<PathBuf> {
    validate_generated_id(id, "session")?;
    Ok(sessions_dir(w).join(id))
}

fn session_path(w: &Wiki, id: &str) -> Result<PathBuf> {
    Ok(session_dir(w, id)?.join("session.toml"))
}

fn inbox_path(w: &Wiki, id: &str) -> Result<PathBuf> {
    Ok(session_dir(w, id)?.join("inbox.toml"))
}

fn write_atomic(path: &Path, content: &str) -> Result<()> {
    let parent = path
        .parent()
        .context("state file has no parent directory")?;
    fs::create_dir_all(parent)?;
    let temp = parent.join(format!(
        ".tmp-{}-{}",
        std::process::id(),
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    fs::write(&temp, content).with_context(|| format!("writing {}", temp.display()))?;
    fs::rename(&temp, path).with_context(|| format!("replacing {}", path.display()))
}

fn load_session(w: &Wiki, id: &str) -> Result<Session> {
    let path = session_path(w, id)?;
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("no session '{id}' (looked at {})", path.display()))?;
    toml::from_str(&raw).with_context(|| format!("parsing session '{id}'"))
}

fn save_session(w: &Wiki, session: &Session) -> Result<()> {
    write_atomic(
        &session_path(w, &session.id)?,
        &toml::to_string_pretty(session)?,
    )
}

fn load_inbox(w: &Wiki, id: &str) -> Result<Inbox> {
    load_session(w, id)?;
    let path = inbox_path(w, id)?;
    if !path.exists() {
        return Ok(Inbox::default());
    }
    let raw = fs::read_to_string(&path)?;
    toml::from_str(&raw).with_context(|| format!("parsing inbox for session '{id}'"))
}

fn save_inbox(w: &Wiki, id: &str, inbox: &Inbox) -> Result<()> {
    write_atomic(&inbox_path(w, id)?, &toml::to_string_pretty(inbox)?)
}

fn notification_paths(w: &Wiki) -> Vec<PathBuf> {
    let mut paths = vec![];
    let Ok(entries) = fs::read_dir(sessions_dir(w)) else {
        return paths;
    };
    for entry in entries.flatten() {
        let dir = entry.path().join("notifications");
        let Ok(notifications) = fs::read_dir(dir) else {
            continue;
        };
        paths.extend(
            notifications
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md")),
        );
    }
    paths.sort();
    paths
}

fn render_notification(notification: &Notification) -> Result<String> {
    Ok(format!(
        "+++\n{}+++\n\n{}\n",
        toml::to_string_pretty(&notification.meta)?,
        notification.body.trim_end()
    ))
}

fn parse_notification(path: &Path) -> Result<Notification> {
    let raw = fs::read_to_string(path)?;
    let rest = raw
        .strip_prefix("+++\n")
        .context("notification is missing TOML frontmatter")?;
    let (frontmatter, body) = rest
        .split_once("+++\n")
        .context("notification frontmatter is not closed")?;
    Ok(Notification {
        meta: toml::from_str(frontmatter)?,
        body: body.trim_start_matches('\n').trim_end().to_string(),
    })
}

fn all_notifications(w: &Wiki) -> Result<Vec<Notification>> {
    let mut notifications = vec![];
    for path in notification_paths(w) {
        notifications.push(
            parse_notification(&path)
                .with_context(|| format!("reading notification {}", path.display()))?,
        );
    }
    notifications.sort_by(|a, b| {
        a.meta
            .created_at
            .cmp(&b.meta.created_at)
            .then(a.meta.id.cmp(&b.meta.id))
    });
    Ok(notifications)
}

pub fn start(w: &Wiki, agent: Option<String>, label: Option<String>, json: bool) -> Result<String> {
    let agent = clean_field("agent", agent.as_deref().unwrap_or("unknown"))?;
    let label = label.map(|v| clean_field("label", &v)).transpose()?;
    w.ensure_gitignore()?;
    fs::create_dir_all(sessions_dir(w))?;

    let mut created = None;
    for attempt in 0..100 {
        let id = unique_id("session", attempt);
        let dir = session_dir(w, &id)?;
        match fs::create_dir(&dir) {
            Ok(()) => {
                created = Some((id, dir));
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    let (id, dir) = created.context("could not allocate a unique session id")?;
    fs::create_dir(dir.join("notifications"))?;
    let timestamp = now();
    let session = Session {
        id: id.clone(),
        agent,
        label,
        created_at: timestamp.clone(),
        updated_at: timestamp,
        status: "active".into(),
    };
    save_session(w, &session)?;

    // A new session starts caught up; only notifications created afterward
    // appear as unread. Full history remains available with --all.
    let inbox = Inbox {
        states: all_notifications(w)?
            .into_iter()
            .map(|n| (n.meta.id, "existing".into()))
            .collect(),
    };
    save_inbox(w, &id, &inbox)?;
    w.commit(&format!("wookie: start session {id}"));

    if json {
        Ok(serde_json::to_string(&session)?)
    } else {
        Ok(format!(
            "Started session '{id}'. Keep this id for notify and inbox commands."
        ))
    }
}

pub fn list(w: &Wiki, json: bool) -> Result<String> {
    let mut sessions = vec![];
    if let Ok(entries) = fs::read_dir(sessions_dir(w)) {
        for entry in entries.flatten() {
            let Some(id) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if let Ok(session) = load_session(w, &id) {
                sessions.push(session);
            }
        }
    }
    sessions.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    if json {
        return Ok(serde_json::json!({"sessions": sessions}).to_string());
    }
    if sessions.is_empty() {
        return Ok("No sessions yet. Start one with `wookie session start`.".into());
    }
    Ok(sessions
        .iter()
        .map(|s| {
            format!(
                "{}  {}  {}  {}{}",
                s.id,
                s.status,
                s.agent,
                s.created_at,
                s.label
                    .as_ref()
                    .map(|l| format!("  {l}"))
                    .unwrap_or_default()
            )
        })
        .collect::<Vec<_>>()
        .join("\n"))
}

pub fn show(w: &Wiki, id: &str, json: bool) -> Result<String> {
    let session = load_session(w, id)?;
    let sent: Vec<Notification> = all_notifications(w)?
        .into_iter()
        .filter(|n| n.meta.source_session == id)
        .collect();
    if json {
        return Ok(serde_json::json!({
            "session": session,
            "notifications_sent": sent.iter().map(|n| &n.meta).collect::<Vec<_>>()
        })
        .to_string());
    }
    let mut output = format!(
        "Session: {}\nAgent: {}\nLabel: {}\nStatus: {}\nCreated: {}\nUpdated: {}\nNotifications sent: {}",
        session.id,
        session.agent,
        session.label.as_deref().unwrap_or("-"),
        session.status,
        session.created_at,
        session.updated_at,
        sent.len()
    );
    for notification in sent {
        output.push_str(&format!(
            "\n  {}  [{} / {}] {}",
            notification.meta.id,
            notification.meta.kind,
            notification.meta.importance,
            notification.meta.summary
        ));
    }
    Ok(output)
}

pub fn close(w: &Wiki, id: &str, json: bool) -> Result<String> {
    let mut session = load_session(w, id)?;
    session.status = "closed".into();
    session.updated_at = now();
    save_session(w, &session)?;
    w.commit(&format!("wookie: close session {id}"));
    if json {
        Ok(serde_json::json!({"session": id, "status": "closed"}).to_string())
    } else {
        Ok(format!("Closed session '{id}'."))
    }
}

#[allow(clippy::too_many_arguments)]
pub fn notify(
    w: &Wiki,
    source_session: &str,
    summary: &str,
    kind: NotificationKind,
    importance: Importance,
    paths: Vec<String>,
    body: Option<String>,
    json: bool,
) -> Result<String> {
    let session = load_session(w, source_session)?;
    if session.status != "active" {
        bail!("session '{source_session}' is closed; start a new session before notifying");
    }
    let summary = clean_field("summary", summary)?;
    let paths = paths
        .into_iter()
        .map(|p| clean_field("path", &p))
        .collect::<Result<Vec<_>>>()?;
    let body = body
        .filter(|b| !b.trim().is_empty())
        .unwrap_or_else(|| summary.clone());

    let dir = session_dir(w, source_session)?.join("notifications");
    fs::create_dir_all(&dir)?;
    let mut created = None;
    for attempt in 0..100 {
        let id = unique_id("notify", attempt);
        let path = dir.join(format!("{id}.md"));
        let notification = Notification {
            meta: NotificationMeta {
                id: id.clone(),
                source_session: source_session.to_string(),
                summary: summary.clone(),
                kind,
                importance,
                created_at: now(),
                paths: paths.clone(),
            },
            body: body.trim().to_string(),
        };
        let rendered = render_notification(&notification)?;
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                file.write_all(rendered.as_bytes())?;
                created = Some(notification);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    let notification = created.context("could not allocate a unique notification id")?;
    w.commit(&format!("wookie: notify {}", notification.meta.id));
    if json {
        Ok(serde_json::json!({"notification": notification.meta}).to_string())
    } else {
        Ok(format!(
            "Published notification '{}' from session '{}'.",
            notification.meta.id, source_session
        ))
    }
}

pub fn inbox(w: &Wiki, session_id: &str, all: bool, json: bool) -> Result<String> {
    let inbox = load_inbox(w, session_id)?;
    let notifications: Vec<Notification> = all_notifications(w)?
        .into_iter()
        .filter(|n| n.meta.source_session != session_id)
        .filter(|n| all || !inbox.states.contains_key(&n.meta.id))
        .collect();
    if json {
        return Ok(serde_json::json!({
            "session": session_id,
            "unread_only": !all,
            "notifications": notifications.iter().map(|n| &n.meta).collect::<Vec<_>>()
        })
        .to_string());
    }
    if notifications.is_empty() {
        return Ok(if all {
            "No notifications from other sessions.".into()
        } else {
            "No unread notifications.".into()
        });
    }
    Ok(notifications
        .iter()
        .map(|n| {
            let paths = if n.meta.paths.is_empty() {
                String::new()
            } else {
                format!("\n  Paths: {}", n.meta.paths.join(", "))
            };
            format!(
                "{}\n  From: {}\n  Summary: {}\n  Kind: {}\n  Importance: {}{}",
                n.meta.id,
                n.meta.source_session,
                n.meta.summary,
                n.meta.kind,
                n.meta.importance,
                paths
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n"))
}

fn find_notification(w: &Wiki, id: &str) -> Result<Notification> {
    validate_generated_id(id, "notify")?;
    all_notifications(w)?
        .into_iter()
        .find(|n| n.meta.id == id)
        .with_context(|| format!("no notification '{id}'"))
}

pub fn read_notification(w: &Wiki, session_id: &str, id: &str, json: bool) -> Result<String> {
    let notification = find_notification(w, id)?;
    let mut inbox = load_inbox(w, session_id)?;
    inbox.states.insert(id.to_string(), "read".into());
    save_inbox(w, session_id, &inbox)?;
    if json {
        return Ok(serde_json::json!({
            "notification": notification.meta,
            "body": notification.body,
            "state": "read"
        })
        .to_string());
    }
    Ok(format!(
        "Notification: {}\nFrom: {}\nKind: {}\nImportance: {}\nSummary: {}\nPaths: {}\n\n{}",
        notification.meta.id,
        notification.meta.source_session,
        notification.meta.kind,
        notification.meta.importance,
        notification.meta.summary,
        if notification.meta.paths.is_empty() {
            "-".into()
        } else {
            notification.meta.paths.join(", ")
        },
        notification.body
    ))
}

pub fn dismiss_notification(w: &Wiki, session_id: &str, id: &str, json: bool) -> Result<String> {
    find_notification(w, id)?;
    let mut inbox = load_inbox(w, session_id)?;
    inbox.states.insert(id.to_string(), "dismissed".into());
    save_inbox(w, session_id, &inbox)?;
    if json {
        Ok(serde_json::json!({"notification": id, "state": "dismissed"}).to_string())
    } else {
        Ok(format!(
            "Dismissed notification '{id}' for session '{session_id}'."
        ))
    }
}
