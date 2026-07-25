//! Session plans: an immutable, validated plan definition folded with
//! append-only session activity into a deterministic board snapshot.

use crate::sessions::{
    self, ActivityEvent, PlanActivityData, Session, SessionNotificationSummary, SessionShowRequest,
    StorageWarning,
};
use crate::wiki::{validate_id, Wiki};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Write as _;
use std::fs;
use std::io::ErrorKind;

pub const PLAN_SCHEMA: &str = "wookie.plan/v1";
pub const PLAN_EVENT_SCHEMA: &str = "wookie.plan-event/v1";
pub const PLAN_SNAPSHOT_SCHEMA: &str = "wookie.plan-snapshot/v1";
pub const PLAN_ARCHIVE_SCHEMA: &str = "wookie.plan-archive/v1";

pub const MAX_PLAN_BYTES: usize = 256 * 1024;
pub const MAX_PLAN_SEGMENTS: usize = 128;
pub const MAX_SEGMENT_ID_BYTES: usize = 64;
pub const MAX_SEGMENT_DECISIONS: usize = 32;
pub const MAX_SEGMENT_DEPENDENCIES: usize = 32;
pub const MAX_PLAN_TEXT_BYTES: usize = 256 * 1024;
pub const MAX_SNAPSHOT_EVENTS: usize = 1_000;
pub const MAX_SNAPSHOT_NOTIFICATIONS: usize = 1_000;
const MAX_TITLE_BYTES: usize = 1024;
const MAX_GUIDE_BYTES: usize = 1024;
const MAX_SEGMENT_TEXT_BYTES: usize = 8 * 1024;
pub const MAX_LOG_SUMMARY_BYTES: usize = 8 * 1024;
pub const MAX_UPDATE_NOTE_BYTES: usize = 8 * 1024;
pub const MAX_TASK_QUERY_BYTES: usize = crate::retrieval::MAX_QUERY_BYTES;
const MAX_PLAN_GUIDE_BYTES: usize = 512 * 1024;
const MAX_PLAN_GUIDE_AGGREGATE_BYTES: usize = 4 * 1024 * 1024;
const MAX_ARCHIVE_MARKDOWN_BYTES: usize = 10 * 1024 * 1024;
const PLAN_FILE: &str = "plan.toml";
const ARCHIVE_FILE: &str = "archive.md";

#[derive(
    Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum PlanStatus {
    Todo,
    Doing,
    Blocked,
    Done,
}

impl std::fmt::Display for PlanStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Todo => "todo",
            Self::Doing => "doing",
            Self::Blocked => "blocked",
            Self::Done => "done",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum PlanLogKind {
    Decision,
    Blocker,
    Progress,
    Note,
}

impl std::fmt::Display for PlanLogKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Decision => "decision",
            Self::Blocker => "blocker",
            Self::Progress => "progress",
            Self::Note => "note",
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanDefinition {
    pub schema: String,
    pub title: String,
    pub segments: Vec<PlanSegment>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanSegment {
    pub id: String,
    pub title: String,
    pub status: PlanStatus,
    pub guide: String,
    pub justification: String,
    pub decisions: Vec<String>,
    pub verification: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PlanCheck {
    pub schema: &'static str,
    pub plan_hash: String,
    pub title: String,
    pub segment_count: usize,
    pub definition: PlanDefinition,
    /// Canonical persisted representation. Useful to attach callers but
    /// intentionally omitted from machine output to avoid duplicating data.
    #[serde(skip)]
    pub canonical_toml: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct PlanSegmentSnapshot {
    pub id: String,
    pub title: String,
    pub status: PlanStatus,
    pub guide: String,
    pub justification: String,
    pub decisions: Vec<String>,
    pub verification: String,
    pub depends_on: Vec<String>,
    pub ready: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_by: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PlanSnapshot {
    pub schema: &'static str,
    pub plan_schema: String,
    pub plan_hash: String,
    pub title: String,
    pub session: Session,
    pub segments: Vec<PlanSegmentSnapshot>,
    /// Chronological tail of the complete session activity stream.
    pub events: Vec<ActivityEvent>,
    pub events_total: usize,
    pub events_omitted: usize,
    /// Newest outgoing notifications first, matching `session show`.
    pub notifications: Vec<SessionNotificationSummary>,
    pub notifications_total: usize,
    pub notifications_omitted: usize,
    pub notifications_scan_complete: bool,
    pub notification_warnings_total: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notification_warnings: Vec<StorageWarning>,
}

#[derive(Clone, Debug)]
pub struct SnapshotOptions {
    pub event_limit: usize,
    pub notification_limit: usize,
}

impl Default for SnapshotOptions {
    fn default() -> Self {
        Self {
            event_limit: 200,
            notification_limit: 50,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct PlanArchiveReceipt {
    pub schema: &'static str,
    pub session_id: String,
    pub title: String,
    pub summary: String,
    pub plan_hash: String,
    pub receipt_sha256: String,
    pub total_segments: usize,
    pub done_segments: usize,
    pub incomplete_segments: usize,
    pub allow_incomplete: bool,
    pub activity_events: usize,
    pub notifications: usize,
    pub notifications_omitted: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct PlanArchive {
    pub receipt: PlanArchiveReceipt,
    pub snapshot: PlanSnapshot,
    pub archive_path: String,
}

#[derive(Serialize)]
struct ArchiveReceiptMaterial<'a> {
    schema: &'static str,
    session_id: &'a str,
    title: &'a str,
    summary: &'a str,
    plan_hash: &'a str,
    statuses: &'a BTreeMap<String, PlanStatus>,
    events: &'a [ActivityEvent],
    notifications: &'a [SessionNotificationSummary],
    notifications_total: usize,
    allow_incomplete: bool,
}

pub fn guide_text() -> &'static str {
    r#"Use the host's native planning workflow before implementation when one is
available: Codex Plan mode (`/plan` or Shift+Tab), Claude's native planning
workflow, or the equivalent. Use it for exploration, questions, and review,
then serialize the approved result as strict `wookie.plan/v1` TOML. Wookie
validates and tracks the approved plan; it does not replace the host planner.

Each segment must be independently understandable and include:
- a safe lowercase `id`
- a concise `title`
- an initial `status`: `todo`, `doing`, `blocked`, or `done`
- a `guide` page id that already exists and is not a stub
- a concrete `justification`
- one or more key architectural `decisions`
- an objective `verification`
- optional `depends_on` segment ids

Keep the plan small. A segment should describe one reviewable outcome, not an
individual command. Dependencies must form an acyclic graph. Example:

schema = "wookie.plan/v1"
title = "Implement bounded retries"

[[segments]]
id = "design"
title = "Confirm retry ownership"
status = "todo"
guide = "architecture/retry-policy"
justification = "The implementation depends on stable ownership boundaries."
decisions = ["Keep policy separate from execution state."]
verification = "Review the boundary and run the architecture checks."
depends_on = []
"#
}

fn is_bidi_control(character: char) -> bool {
    matches!(
        character,
        '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    )
}

fn clean_text(name: &str, value: &str, max_bytes: usize) -> Result<String> {
    let value = value.trim();
    if value.is_empty() {
        bail!("{name} cannot be empty");
    }
    if value.len() > max_bytes {
        bail!("{name} exceeds {max_bytes} bytes");
    }
    if value
        .chars()
        .any(|character| character.is_control() || is_bidi_control(character))
    {
        bail!("{name} must be one line and contain no control or bidi formatting characters");
    }
    Ok(value.to_string())
}

pub fn clean_task_query(value: &str) -> Result<String> {
    clean_text("plan task query", value, MAX_TASK_QUERY_BYTES)
}

fn valid_segment_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_SEGMENT_ID_BYTES
        && bytes[0].is_ascii_lowercase()
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

fn add_text_bytes(total: &mut usize, value: &str) -> Result<()> {
    *total = total
        .checked_add(value.len())
        .context("plan text size overflow")?;
    if *total > MAX_PLAN_TEXT_BYTES {
        bail!("plan text exceeds the {MAX_PLAN_TEXT_BYTES}-byte aggregate ceiling");
    }
    Ok(())
}

fn validate_graph(definition: &PlanDefinition) -> Result<()> {
    let ids = definition
        .segments
        .iter()
        .map(|segment| segment.id.as_str())
        .collect::<BTreeSet<_>>();
    let mut indegree = definition
        .segments
        .iter()
        .map(|segment| (segment.id.as_str(), segment.depends_on.len()))
        .collect::<BTreeMap<_, _>>();
    let mut dependents: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for segment in &definition.segments {
        for dependency in &segment.depends_on {
            if dependency == &segment.id {
                bail!("plan segment '{}' cannot depend on itself", segment.id);
            }
            if !ids.contains(dependency.as_str()) {
                bail!(
                    "plan segment '{}' depends on missing segment '{}'",
                    segment.id,
                    dependency
                );
            }
            dependents.entry(dependency).or_default().push(&segment.id);
        }
    }
    let mut ready = indegree
        .iter()
        .filter_map(|(id, degree)| (*degree == 0).then_some(*id))
        .collect::<VecDeque<_>>();
    let mut visited = 0_usize;
    while let Some(id) = ready.pop_front() {
        visited += 1;
        for dependent in dependents.get(id).into_iter().flatten() {
            let degree = indegree
                .get_mut(dependent)
                .expect("validated dependent belongs to graph");
            *degree -= 1;
            if *degree == 0 {
                ready.push_back(dependent);
            }
        }
    }
    if visited != definition.segments.len() {
        bail!("plan dependencies contain a cycle");
    }
    Ok(())
}

fn validate_definition(
    w: &Wiki,
    definition: &mut PlanDefinition,
    require_live_guides: bool,
) -> Result<()> {
    if definition.schema != PLAN_SCHEMA {
        bail!("unsupported plan schema (expected {PLAN_SCHEMA})");
    }
    definition.title = clean_text("plan title", &definition.title, MAX_TITLE_BYTES)?;
    if definition.segments.is_empty() {
        bail!("plan must contain at least one segment");
    }
    if definition.segments.len() > MAX_PLAN_SEGMENTS {
        bail!(
            "plan contains {} segments; maximum is {MAX_PLAN_SEGMENTS}",
            definition.segments.len()
        );
    }

    let mut ids = BTreeSet::new();
    let mut validated_guides = BTreeSet::new();
    let mut validated_guide_bytes = 0_usize;
    let mut text_bytes = definition.title.len();
    for segment in &mut definition.segments {
        if !valid_segment_id(&segment.id) {
            bail!(
                "plan segment id must match [a-z][a-z0-9-]*, end alphanumerically, and be at most {MAX_SEGMENT_ID_BYTES} bytes"
            );
        }
        if !ids.insert(segment.id.clone()) {
            bail!("duplicate plan segment id '{}'", segment.id);
        }
        segment.title = clean_text(
            &format!("title for segment '{}'", segment.id),
            &segment.title,
            MAX_TITLE_BYTES,
        )?;
        segment.guide = clean_text(
            &format!("guide for segment '{}'", segment.id),
            &segment.guide,
            MAX_GUIDE_BYTES,
        )?;
        validate_id(&segment.guide)
            .with_context(|| format!("invalid guide for segment '{}'", segment.id))?;
        if require_live_guides && validated_guides.insert(segment.guide.clone()) {
            let page = w.load_page(&segment.guide).with_context(|| {
                format!(
                    "guide page '{}' for segment '{}' does not exist",
                    segment.guide, segment.id
                )
            })?;
            if page.is_stub() {
                bail!(
                    "guide page '{}' for segment '{}' is still a stub",
                    segment.guide,
                    segment.id
                );
            }
            let page_bytes = serde_json::to_vec(&page)
                .context("measuring serialized plan guide")?
                .len();
            if page_bytes > MAX_PLAN_GUIDE_BYTES {
                bail!(
                    "guide page '{}' exceeds the {MAX_PLAN_GUIDE_BYTES}-byte plan validation ceiling",
                    segment.guide
                );
            }
            validated_guide_bytes = validated_guide_bytes
                .checked_add(page_bytes)
                .context("aggregate plan guide size overflow")?;
            if validated_guide_bytes > MAX_PLAN_GUIDE_AGGREGATE_BYTES {
                bail!(
                    "plan guides exceed the {MAX_PLAN_GUIDE_AGGREGATE_BYTES}-byte aggregate validation ceiling"
                );
            }
        }
        segment.justification = clean_text(
            &format!("justification for segment '{}'", segment.id),
            &segment.justification,
            MAX_SEGMENT_TEXT_BYTES,
        )?;
        segment.verification = clean_text(
            &format!("verification for segment '{}'", segment.id),
            &segment.verification,
            MAX_SEGMENT_TEXT_BYTES,
        )?;
        if segment.decisions.is_empty() {
            bail!(
                "plan segment '{}' must include at least one architectural decision",
                segment.id
            );
        }
        if segment.decisions.len() > MAX_SEGMENT_DECISIONS {
            bail!(
                "plan segment '{}' has too many decisions; maximum is {MAX_SEGMENT_DECISIONS}",
                segment.id
            );
        }
        for (index, decision) in segment.decisions.iter_mut().enumerate() {
            *decision = clean_text(
                &format!("decision {index} for segment '{}'", segment.id),
                decision,
                MAX_SEGMENT_TEXT_BYTES,
            )?;
        }
        if segment.depends_on.len() > MAX_SEGMENT_DEPENDENCIES {
            bail!(
                "plan segment '{}' has too many dependencies; maximum is {MAX_SEGMENT_DEPENDENCIES}",
                segment.id
            );
        }
        let mut dependencies = BTreeSet::new();
        for dependency in &mut segment.depends_on {
            *dependency = clean_text(
                &format!("dependency for segment '{}'", segment.id),
                dependency,
                MAX_SEGMENT_ID_BYTES,
            )?;
            if !valid_segment_id(dependency) {
                bail!(
                    "plan segment '{}' has invalid dependency id '{}'",
                    segment.id,
                    dependency
                );
            }
            if !dependencies.insert(dependency.clone()) {
                bail!(
                    "plan segment '{}' repeats dependency '{}'",
                    segment.id,
                    dependency
                );
            }
        }
        for value in [
            &segment.id,
            &segment.title,
            &segment.guide,
            &segment.justification,
            &segment.verification,
        ] {
            add_text_bytes(&mut text_bytes, value)?;
        }
        for value in segment.decisions.iter().chain(&segment.depends_on) {
            add_text_bytes(&mut text_bytes, value)?;
        }
    }
    validate_graph(definition)?;

    let initial = definition
        .segments
        .iter()
        .map(|segment| (segment.id.as_str(), segment.status))
        .collect::<BTreeMap<_, _>>();
    for segment in &definition.segments {
        if matches!(segment.status, PlanStatus::Doing | PlanStatus::Done) {
            let incomplete = segment
                .depends_on
                .iter()
                .filter(|dependency| initial.get(dependency.as_str()) != Some(&PlanStatus::Done))
                .collect::<Vec<_>>();
            if !incomplete.is_empty() {
                bail!(
                    "plan segment '{}' starts {} while dependencies are incomplete: {}",
                    segment.id,
                    segment.status,
                    incomplete
                        .iter()
                        .map(|value| value.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn parse_and_check(w: &Wiki, raw: &str, require_live_guides: bool) -> Result<PlanCheck> {
    if raw.is_empty() {
        bail!("plan input is empty");
    }
    if raw.len() > MAX_PLAN_BYTES {
        bail!("plan exceeds the {MAX_PLAN_BYTES}-byte input ceiling");
    }
    if raw.chars().any(|character| {
        (character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
            || is_bidi_control(character)
    }) {
        bail!("plan contains an unsupported control or bidi formatting character");
    }
    let mut definition: PlanDefinition = toml::from_str(raw).context("parsing wookie plan TOML")?;
    validate_definition(w, &mut definition, require_live_guides)?;
    let canonical_toml =
        toml::to_string_pretty(&definition).context("rendering canonical wookie plan")?;
    if canonical_toml.len() > MAX_PLAN_BYTES {
        bail!("canonical plan exceeds the {MAX_PLAN_BYTES}-byte storage ceiling");
    }
    let plan_hash = sha256_hex(canonical_toml.as_bytes());
    Ok(PlanCheck {
        schema: PLAN_SCHEMA,
        plan_hash,
        title: definition.title.clone(),
        segment_count: definition.segments.len(),
        definition,
        canonical_toml,
    })
}

pub fn check(w: &Wiki, raw: &str) -> Result<PlanCheck> {
    parse_and_check(w, raw, true)
}

fn load_plan(w: &Wiki, session_id: &str) -> Result<PlanCheck> {
    let path = sessions::session_file_path(w, session_id, PLAN_FILE)?;
    let raw = sessions::read_bounded_regular_utf8(
        &path,
        u64::try_from(MAX_PLAN_BYTES).unwrap_or(u64::MAX),
        "session plan",
    )
    .with_context(|| format!("session '{session_id}' has no attached plan"))?;
    // Guide availability is an attachment-time invariant. Snapshots retain
    // the board when a guide is later renamed or stubbed so the read-only UI
    // can report that live guide error on the affected card.
    parse_and_check(w, &raw, false)
}

fn status_map(definition: &PlanDefinition) -> BTreeMap<String, PlanStatus> {
    definition
        .segments
        .iter()
        .map(|segment| (segment.id.clone(), segment.status))
        .collect()
}

fn fold_statuses(
    definition: &PlanDefinition,
    plan_hash: &str,
    events: &[ActivityEvent],
) -> Result<BTreeMap<String, PlanStatus>> {
    let mut statuses = status_map(definition);
    let mut attached = 0_usize;
    let mut archived = false;
    let segments = definition
        .segments
        .iter()
        .map(|segment| (segment.id.as_str(), segment))
        .collect::<BTreeMap<_, _>>();
    for event in events {
        let Some(plan) = &event.plan else {
            continue;
        };
        if plan.schema != PLAN_EVENT_SCHEMA {
            bail!(
                "activity '{}' has an unsupported plan event schema",
                event.id
            );
        }
        let event_hash = plan
            .plan_sha256
            .as_deref()
            .with_context(|| format!("plan activity '{}' is missing its plan hash", event.id))?;
        if event_hash != plan_hash {
            bail!(
                "plan activity '{}' belongs to a different immutable plan",
                event.id
            );
        }
        if archived {
            bail!(
                "typed plan activity '{}' appears after the terminal archive event",
                event.id
            );
        }
        match plan.kind.as_str() {
            "attached" => {
                if attached > 0 {
                    bail!("session plan has duplicate attachment activity");
                }
                attached = 1;
            }
            "status-changed" => {
                if attached == 0 {
                    bail!(
                        "plan activity '{}' precedes the immutable plan attachment",
                        event.id
                    );
                }
                let segment_id = plan.segment_id.as_deref().with_context(|| {
                    format!("plan status activity '{}' has no segment id", event.id)
                })?;
                let from = plan
                    .from_status
                    .as_deref()
                    .map(parse_status)
                    .transpose()?
                    .with_context(|| {
                        format!("plan status activity '{}' has no prior status", event.id)
                    })?;
                let to = plan
                    .to_status
                    .as_deref()
                    .map(parse_status)
                    .transpose()?
                    .with_context(|| {
                        format!("plan status activity '{}' has no target status", event.id)
                    })?;
                let current = *statuses.get(segment_id).with_context(|| {
                    format!(
                        "plan status activity '{}' references unknown segment '{segment_id}'",
                        event.id
                    )
                })?;
                if current != from {
                    bail!(
                        "plan activity '{}' expected segment '{}' to be {}, but the folded status is {}",
                        event.id,
                        segment_id,
                        from,
                        current
                    );
                }
                if matches!(to, PlanStatus::Doing | PlanStatus::Done) {
                    let segment = segments
                        .get(segment_id)
                        .expect("validated status activity references a plan segment");
                    let blocked_by = segment
                        .depends_on
                        .iter()
                        .filter(|dependency| statuses.get(*dependency) != Some(&PlanStatus::Done))
                        .cloned()
                        .collect::<Vec<_>>();
                    if !blocked_by.is_empty() {
                        bail!(
                            "plan activity '{}' moves segment '{}' to {} while dependencies are incomplete: {}",
                            event.id,
                            segment_id,
                            to,
                            blocked_by.join(", ")
                        );
                    }
                }
                statuses.insert(segment_id.to_string(), to);
            }
            "log" | "archived" => {
                if attached == 0 {
                    bail!(
                        "plan activity '{}' precedes the immutable plan attachment",
                        event.id
                    );
                }
                if plan.kind == "archived" {
                    archived = true;
                }
            }
            other => bail!("activity '{}' has unknown plan kind '{other}'", event.id),
        }
    }
    if attached != 1 {
        bail!("session plan requires exactly one attachment activity; found {attached}");
    }
    Ok(statuses)
}

fn parse_status(value: &str) -> Result<PlanStatus> {
    match value {
        "todo" => Ok(PlanStatus::Todo),
        "doing" => Ok(PlanStatus::Doing),
        "blocked" => Ok(PlanStatus::Blocked),
        "done" => Ok(PlanStatus::Done),
        _ => bail!("invalid plan status '{value}'"),
    }
}

fn validate_snapshot_options(options: &SnapshotOptions) -> Result<()> {
    if options.event_limit == 0 || options.event_limit > MAX_SNAPSHOT_EVENTS {
        bail!("plan event limit must be between 1 and {MAX_SNAPSHOT_EVENTS}");
    }
    if options.notification_limit == 0 || options.notification_limit > MAX_SNAPSHOT_NOTIFICATIONS {
        bail!("plan notification limit must be between 1 and {MAX_SNAPSHOT_NOTIFICATIONS}");
    }
    Ok(())
}

fn snapshot_from_parts(
    check: PlanCheck,
    events: Vec<ActivityEvent>,
    show: sessions::SessionShowResult,
    options: &SnapshotOptions,
) -> Result<PlanSnapshot> {
    validate_snapshot_options(options)?;
    let statuses = fold_statuses(&check.definition, &check.plan_hash, &events)?;
    let segments = check
        .definition
        .segments
        .iter()
        .map(|segment| {
            let status = *statuses
                .get(&segment.id)
                .expect("validated plan status map includes every segment");
            let blocked_by = segment
                .depends_on
                .iter()
                .filter(|dependency| statuses.get(*dependency) != Some(&PlanStatus::Done))
                .cloned()
                .collect::<Vec<_>>();
            PlanSegmentSnapshot {
                id: segment.id.clone(),
                title: segment.title.clone(),
                status,
                guide: segment.guide.clone(),
                justification: segment.justification.clone(),
                decisions: segment.decisions.clone(),
                verification: segment.verification.clone(),
                depends_on: segment.depends_on.clone(),
                ready: status != PlanStatus::Done && blocked_by.is_empty(),
                blocked_by,
            }
        })
        .collect::<Vec<_>>();
    let events_total = events.len();
    let events_omitted = events_total.saturating_sub(options.event_limit);
    let events = events.into_iter().skip(events_omitted).collect();
    Ok(PlanSnapshot {
        schema: PLAN_SNAPSHOT_SCHEMA,
        plan_schema: check.definition.schema,
        plan_hash: check.plan_hash,
        title: check.title,
        session: show.session,
        segments,
        events,
        events_total,
        events_omitted,
        notifications: show.notifications_sent,
        notifications_total: show.total_notifications_sent,
        notifications_omitted: show.notifications_omitted,
        notifications_scan_complete: show.scan_complete,
        notification_warnings_total: show.warnings_total,
        notification_warnings: show.warnings,
    })
}

pub fn snapshot(w: &Wiki, session_id: &str, options: SnapshotOptions) -> Result<PlanSnapshot> {
    validate_snapshot_options(&options)?;
    let check = load_plan(w, session_id)?;
    let (session, events) = sessions::load_session_and_activity(w, session_id)?;
    let show = sessions::show_result_for_session(
        w,
        session,
        &SessionShowRequest {
            limit: Some(options.notification_limit),
            cursor: 0,
        },
    )?;
    snapshot_from_parts(check, events, show, &options)
}

fn event_payload(kind: &str, plan_hash: &str) -> PlanActivityData {
    PlanActivityData {
        schema: PLAN_EVENT_SCHEMA.into(),
        kind: kind.into(),
        segment_id: None,
        from_status: None,
        to_status: None,
        log_kind: None,
        summary: None,
        note: None,
        plan_sha256: Some(plan_hash.into()),
        receipt_sha256: None,
        total_segments: None,
        done_segments: None,
        incomplete_segments: None,
        allow_incomplete: None,
    }
}

pub fn attach(w: &Wiki, session_id: &str, raw: &str) -> Result<PlanSnapshot> {
    let guard = w.acquire_mutation_guard()?;
    let session = sessions::load_session(w, session_id)?;
    if session.status != "active" {
        bail!("session '{session_id}' is closed");
    }

    let plan_path = sessions::session_file_path(w, session_id, PLAN_FILE)?;
    if plan_path.exists() {
        // Existing plans use semantic canonicalization without requiring their
        // guide pages to still be live. This makes a same-plan retry capable
        // of repairing an interrupted attach after a guide is later renamed.
        let checked = parse_and_check(w, raw, false)?;
        let existing = load_plan(w, session_id)?;
        if existing.plan_hash != checked.plan_hash {
            bail!("session '{session_id}' already has a different immutable plan");
        }
        let events = sessions::ordered_activity_events(w, session_id)?;
        let attachments = events
            .iter()
            .filter(|event| {
                event
                    .plan
                    .as_ref()
                    .is_some_and(|plan| plan.kind == "attached")
            })
            .collect::<Vec<_>>();
        let attachment_path = match attachments.as_slice() {
            [] => {
                let activity = sessions::append_structured_activity_guarded(
                    w,
                    &guard,
                    session_id,
                    "plan-attached",
                    None,
                    event_payload("attached", &existing.plan_hash),
                )?;
                activity.history_path
            }
            [event]
                if event
                    .plan
                    .as_ref()
                    .and_then(|plan| plan.plan_sha256.as_deref())
                    == Some(existing.plan_hash.as_str()) =>
            {
                format!("sessions/{session_id}/activity/{}.toml", event.id)
            }
            [_] => bail!("session '{session_id}' attachment event has the wrong plan hash"),
            _ => bail!("session '{session_id}' has duplicate plan attachment events"),
        };
        sessions::commit_session_paths_guarded(
            w,
            &guard,
            &format!("wookie: finish attaching plan to session {session_id}"),
            &[
                format!("sessions/{session_id}/{PLAN_FILE}"),
                attachment_path,
            ],
        )?;
        drop(guard);
        return snapshot(w, session_id, SnapshotOptions::default());
    }

    // Pages and plan attachment share the mutation guard, so guide existence
    // cannot change between validation and immutable publication.
    let checked = check(w, raw)?;
    let plan_history_path = sessions::write_immutable_session_file_guarded(
        w,
        &guard,
        session_id,
        PLAN_FILE,
        &checked.canonical_toml,
    )?;
    let activity = match sessions::append_structured_activity_guarded(
        w,
        &guard,
        session_id,
        "plan-attached",
        None,
        event_payload("attached", &checked.plan_hash),
    ) {
        Ok(activity) => activity,
        Err(error) => {
            if let Err(rollback) =
                sessions::remove_session_file_guarded(w, &guard, session_id, PLAN_FILE)
            {
                bail!(
                    "attaching session plan failed: {error:#}; rollback also failed: {rollback:#}"
                );
            }
            return Err(error);
        }
    };
    sessions::commit_session_paths_guarded(
        w,
        &guard,
        &format!("wookie: attach plan to session {session_id}"),
        &[plan_history_path, activity.history_path],
    )?;
    drop(guard);
    snapshot(w, session_id, SnapshotOptions::default())
}

fn current_state(
    w: &Wiki,
    session_id: &str,
) -> Result<(PlanCheck, Vec<ActivityEvent>, BTreeMap<String, PlanStatus>)> {
    let check = load_plan(w, session_id)?;
    let events = sessions::ordered_activity_events(w, session_id)?;
    let statuses = fold_statuses(&check.definition, &check.plan_hash, &events)?;
    Ok((check, events, statuses))
}

pub fn update(
    w: &Wiki,
    session_id: &str,
    segment_id: &str,
    status: PlanStatus,
    note: Option<&str>,
) -> Result<PlanSnapshot> {
    let guard = w.acquire_mutation_guard()?;
    let session = sessions::load_session(w, session_id)?;
    if session.status != "active" {
        bail!("session '{session_id}' is closed");
    }
    if !valid_segment_id(segment_id) {
        bail!("invalid plan segment id");
    }
    let (check, _, statuses) = current_state(w, session_id)?;
    let segment = check
        .definition
        .segments
        .iter()
        .find(|segment| segment.id == segment_id)
        .with_context(|| format!("plan has no segment '{segment_id}'"))?;
    let from = *statuses
        .get(segment_id)
        .expect("validated plan includes requested segment");
    if from == status {
        bail!("plan segment '{segment_id}' is already {status}");
    }
    let note = note
        .map(|value| clean_text("plan update note", value, MAX_UPDATE_NOTE_BYTES))
        .transpose()?;
    if matches!(status, PlanStatus::Doing | PlanStatus::Done) {
        let blocked_by = segment
            .depends_on
            .iter()
            .filter(|dependency| statuses.get(*dependency) != Some(&PlanStatus::Done))
            .cloned()
            .collect::<Vec<_>>();
        if !blocked_by.is_empty() {
            bail!(
                "plan segment '{segment_id}' cannot move to {status}; incomplete dependencies: {}",
                blocked_by.join(", ")
            );
        }
    }

    let mut payload = event_payload("status-changed", &check.plan_hash);
    payload.segment_id = Some(segment_id.into());
    payload.from_status = Some(from.to_string());
    payload.to_status = Some(status.to_string());
    payload.note = note;
    let activity = sessions::append_structured_activity_guarded(
        w,
        &guard,
        session_id,
        "plan-status",
        None,
        payload,
    )?;
    sessions::commit_session_paths_guarded(
        w,
        &guard,
        &format!("wookie: update plan {session_id}/{segment_id}"),
        &[activity.history_path],
    )?;
    drop(guard);
    snapshot(w, session_id, SnapshotOptions::default())
}

pub fn log(
    w: &Wiki,
    session_id: &str,
    segment_id: Option<&str>,
    kind: PlanLogKind,
    summary: &str,
) -> Result<PlanSnapshot> {
    let guard = w.acquire_mutation_guard()?;
    let session = sessions::load_session(w, session_id)?;
    if session.status != "active" {
        bail!("session '{session_id}' is closed");
    }
    let check = load_plan(w, session_id)?;
    let segment_id = segment_id
        .map(|id| {
            if !valid_segment_id(id) {
                bail!("invalid plan segment id");
            }
            if !check
                .definition
                .segments
                .iter()
                .any(|segment| segment.id == id)
            {
                bail!("plan has no segment '{id}'");
            }
            Ok(id.to_string())
        })
        .transpose()?;
    let summary = clean_text("plan log summary", summary, MAX_LOG_SUMMARY_BYTES)?;
    let mut payload = event_payload("log", &check.plan_hash);
    payload.segment_id = segment_id;
    payload.log_kind = Some(kind.to_string());
    payload.summary = Some(summary);
    let activity = sessions::append_structured_activity_guarded(
        w, &guard, session_id, "plan-log", None, payload,
    )?;
    sessions::commit_session_paths_guarded(
        w,
        &guard,
        &format!("wookie: log plan activity for session {session_id}"),
        &[activity.history_path],
    )?;
    drop(guard);
    snapshot(w, session_id, SnapshotOptions::default())
}

fn archive_receipt(
    check: &PlanCheck,
    session: &Session,
    summary: &str,
    statuses: &BTreeMap<String, PlanStatus>,
    events: &[ActivityEvent],
    show: &sessions::SessionShowResult,
    allow_incomplete: bool,
) -> Result<PlanArchiveReceipt> {
    let material = ArchiveReceiptMaterial {
        schema: PLAN_ARCHIVE_SCHEMA,
        session_id: &session.id,
        title: &check.title,
        summary,
        plan_hash: &check.plan_hash,
        statuses,
        events,
        notifications: &show.notifications_sent,
        notifications_total: show.total_notifications_sent,
        allow_incomplete,
    };
    let receipt_sha256 = sha256_hex(&serde_json::to_vec(&material)?);
    let done_segments = statuses
        .values()
        .filter(|status| **status == PlanStatus::Done)
        .count();
    Ok(PlanArchiveReceipt {
        schema: PLAN_ARCHIVE_SCHEMA,
        session_id: session.id.clone(),
        title: check.title.clone(),
        summary: summary.into(),
        plan_hash: check.plan_hash.clone(),
        receipt_sha256,
        total_segments: statuses.len(),
        done_segments,
        incomplete_segments: statuses.len().saturating_sub(done_segments),
        allow_incomplete,
        activity_events: events.len(),
        notifications: show.total_notifications_sent,
        notifications_omitted: show.notifications_omitted,
    })
}

fn escape_markdown_text(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for (index, character) in value.chars().enumerate() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '\\' | '`' | '*' | '_' | '[' | ']' | '!' | '|' | '~' => {
                escaped.push('\\');
                escaped.push(character);
            }
            '#' | '-' | '+' if index == 0 => {
                escaped.push('\\');
                escaped.push(character);
            }
            character => escaped.push(character),
        }
    }
    escaped
}

fn render_archive(
    receipt: &PlanArchiveReceipt,
    definition: &PlanDefinition,
    statuses: &BTreeMap<String, PlanStatus>,
    events: &[ActivityEvent],
    notifications: &[SessionNotificationSummary],
) -> Result<String> {
    let mut output = String::new();
    writeln!(output, "# {}\n", escape_markdown_text(&receipt.title))?;
    writeln!(
        output,
        "{}\n\nReceipt: `{}`\nPlan: `{}`\nIncomplete-plan authorization: `{}`\n",
        escape_markdown_text(&receipt.summary),
        receipt.receipt_sha256,
        receipt.plan_hash,
        receipt.allow_incomplete
    )?;
    writeln!(output, "## Final plan\n")?;
    for segment in &definition.segments {
        let status = statuses
            .get(&segment.id)
            .expect("validated archive contains every segment");
        writeln!(
            output,
            "- **{}** (`{}`): {} — guide `[[{}]]`",
            escape_markdown_text(&segment.title),
            segment.id,
            status,
            segment.guide
        )?;
    }
    writeln!(output, "\n## Session activity\n")?;
    if events.is_empty() {
        writeln!(output, "No activity was recorded before archival.")?;
    }
    for event in events {
        write!(
            output,
            "- `{}` — {}",
            event.at,
            escape_markdown_text(&event.action)
        )?;
        if let Some(plan) = &event.plan {
            if let Some(segment) = &plan.segment_id {
                write!(output, " · `{segment}`")?;
            }
            if let Some(to) = &plan.to_status {
                write!(output, " → `{to}`")?;
            }
            if let Some(kind) = &plan.log_kind {
                write!(output, " · {kind}")?;
            }
            if let Some(summary) = &plan.summary {
                write!(output, " · {}", escape_markdown_text(summary))?;
            }
            if let Some(note) = &plan.note {
                write!(output, " · {}", escape_markdown_text(note))?;
            }
        }
        writeln!(output)?;
    }
    writeln!(output, "\n## Notifications sent\n")?;
    if notifications.is_empty() {
        writeln!(output, "No notifications were sent.")?;
    }
    for notification in notifications {
        writeln!(
            output,
            "- `{}` — [{} / {}] {}",
            notification.created_at,
            notification.kind,
            notification.importance,
            escape_markdown_text(&notification.summary)
        )?;
    }
    if receipt.notifications_omitted > 0 {
        writeln!(
            output,
            "\n{} older notification(s) were omitted by the archive safety limit.",
            receipt.notifications_omitted
        )?;
    }
    if output.len() > MAX_ARCHIVE_MARKDOWN_BYTES {
        bail!("rendered plan archive exceeds the {MAX_ARCHIVE_MARKDOWN_BYTES}-byte safety ceiling");
    }
    Ok(output)
}

fn archived_payload<'a>(
    events: &'a [ActivityEvent],
    plan_hash: &str,
) -> Option<(usize, &'a PlanActivityData)> {
    events.iter().enumerate().rev().find_map(|(index, event)| {
        event
            .plan
            .as_ref()
            .filter(|plan| {
                plan.kind == "archived" && plan.plan_sha256.as_deref() == Some(plan_hash)
            })
            .map(|plan| (index, plan))
    })
}

fn ensure_archive_file(
    w: &Wiki,
    guard: &crate::publish::MutationGuard,
    session_id: &str,
    markdown: &str,
) -> Result<String> {
    w.ensure_mutation_guard(guard)?;
    let path = sessions::session_file_path(w, session_id, ARCHIVE_FILE)?;
    if let Some(existing) = existing_archive_file(&path)? {
        if existing != markdown {
            bail!("session '{session_id}' has a conflicting immutable plan archive");
        }
    } else {
        sessions::write_new(&path, markdown)?;
    }
    Ok(format!("sessions/{session_id}/{ARCHIVE_FILE}"))
}

fn existing_archive_file(path: &std::path::Path) -> Result<Option<String>> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error)
            .with_context(|| format!("inspecting session plan archive {}", path.display())),
        Ok(_) => sessions::read_bounded_regular_utf8(
            path,
            u64::try_from(MAX_ARCHIVE_MARKDOWN_BYTES).unwrap_or(u64::MAX),
            "session plan archive",
        )
        .map(Some),
    }
}

fn preflight_archive_file(w: &Wiki, session_id: &str, markdown: &str) -> Result<()> {
    let path = sessions::session_file_path(w, session_id, ARCHIVE_FILE)?;
    if let Some(existing) = existing_archive_file(&path)? {
        if existing != markdown {
            bail!("session '{session_id}' has a conflicting immutable plan archive");
        }
    }
    Ok(())
}

pub fn archive(
    w: &Wiki,
    session_id: &str,
    allow_incomplete: bool,
    summary: Option<&str>,
) -> Result<PlanArchive> {
    let guard = w.acquire_mutation_guard()?;
    let session = sessions::load_session(w, session_id)?;
    let (check, events, statuses) = current_state(w, session_id)?;
    let show = sessions::show_result(
        w,
        session_id,
        &SessionShowRequest {
            limit: Some(MAX_SNAPSHOT_NOTIFICATIONS),
            cursor: 0,
        },
    )?;
    if !show.scan_complete || show.warnings_total > 0 {
        bail!(
            "notification scan is incomplete or contains {} warning(s); refusing to archive because outgoing notifications could be omitted",
            show.warnings_total
        );
    }

    if session.status == "closed" {
        let (archive_index, archived) =
            archived_payload(&events, &check.plan_hash).with_context(|| {
                format!("session '{session_id}' is closed without a plan archive receipt")
            })?;
        let summary = archived
            .summary
            .as_deref()
            .context("archived plan activity is missing its summary")?;
        let archived_allow_incomplete = archived
            .allow_incomplete
            .context("archived plan activity is missing its incomplete-plan authorization")?;
        let receipt = archive_receipt(
            &check,
            &Session {
                status: "active".into(),
                ..session.clone()
            },
            summary,
            &statuses,
            &events[..archive_index],
            &show,
            archived_allow_incomplete,
        )?;
        if archived.total_segments != Some(receipt.total_segments)
            || archived.done_segments != Some(receipt.done_segments)
            || archived.incomplete_segments != Some(receipt.incomplete_segments)
            || archived.allow_incomplete != Some(receipt.allow_incomplete)
        {
            bail!("session '{session_id}' archive receipt counts or authorization no longer match its activity");
        }
        if archived.receipt_sha256.as_deref() != Some(&receipt.receipt_sha256) {
            bail!("session '{session_id}' archive receipt no longer matches its activity");
        }
        let pre_archive_events = &events[..archive_index];
        let markdown = render_archive(
            &receipt,
            &check.definition,
            &statuses,
            pre_archive_events,
            &show.notifications_sent,
        )?;
        let archive_path = ensure_archive_file(w, &guard, session_id, &markdown)?;
        let archive_activity_path = format!(
            "sessions/{session_id}/activity/{}.toml",
            events[archive_index].id
        );
        sessions::commit_session_paths_guarded(
            w,
            &guard,
            &format!("wookie: finish plan archive for session {session_id}"),
            &[archive_activity_path, archive_path.clone()],
        )?;
        let final_snapshot = snapshot(w, session_id, SnapshotOptions::default())?;
        drop(guard);
        return Ok(PlanArchive {
            receipt,
            snapshot: final_snapshot,
            archive_path,
        });
    }

    let incomplete = statuses
        .values()
        .filter(|status| **status != PlanStatus::Done)
        .count();
    if incomplete > 0 && !allow_incomplete {
        bail!(
            "plan has {incomplete} incomplete segment(s); pass allow-incomplete explicitly to archive"
        );
    }
    let summary = summary
        .map(|value| clean_text("plan archive summary", value, MAX_SEGMENT_TEXT_BYTES))
        .transpose()?
        .unwrap_or_else(|| format!("Plan '{}' archived.", check.title));
    let receipt = archive_receipt(
        &check,
        &session,
        &summary,
        &statuses,
        &events,
        &show,
        allow_incomplete,
    )?;
    let markdown = render_archive(
        &receipt,
        &check.definition,
        &statuses,
        &events,
        &show.notifications_sent,
    )?;
    // Reject an unsafe or conflicting destination before the authoritative
    // close event. A missing file is deliberately not created yet: after the
    // event, publishing the deterministic projection is retry-safe.
    preflight_archive_file(w, session_id, &markdown)?;

    let mut payload = event_payload("archived", &check.plan_hash);
    payload.summary = Some(summary);
    payload.receipt_sha256 = Some(receipt.receipt_sha256.clone());
    payload.total_segments = Some(receipt.total_segments);
    payload.done_segments = Some(receipt.done_segments);
    payload.incomplete_segments = Some(receipt.incomplete_segments);
    payload.allow_incomplete = Some(allow_incomplete);
    let activity = sessions::append_structured_activity_guarded(
        w,
        &guard,
        session_id,
        "plan-archived",
        Some("closed"),
        payload,
    )?;
    let archive_path = ensure_archive_file(w, &guard, session_id, &markdown)?;
    sessions::commit_session_paths_guarded(
        w,
        &guard,
        &format!("wookie: archive plan for session {session_id}"),
        &[activity.history_path, archive_path.clone()],
    )?;
    let final_snapshot = snapshot(w, session_id, SnapshotOptions::default())?;
    drop(guard);
    Ok(PlanArchive {
        receipt,
        snapshot: final_snapshot,
        archive_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::{Frontmatter, Page};
    use crate::sessions::{start_with_options, StartOptions};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    struct Fixture {
        home: PathBuf,
        wiki: Wiki,
        session: Session,
    }

    impl Fixture {
        fn new() -> Self {
            let home = std::env::temp_dir().join(format!(
                "wookie-plan-test-{}-{}",
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
            let session = start_with_options(
                &wiki,
                StartOptions {
                    agent: Some("test".into()),
                    activity_debounce_seconds: 60,
                    ..StartOptions::default()
                },
            )
            .unwrap();
            Self {
                home,
                wiki,
                session,
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.home);
        }
    }

    fn plan() -> &'static str {
        r#"schema = "wookie.plan/v1"
title = "Implement plan support"

[[segments]]
id = "design"
title = "Confirm boundaries"
status = "todo"
guide = "architecture/guide"
justification = "The implementation needs a stable boundary."
decisions = ["Keep the definition immutable."]
verification = "Review the stored definition."

[[segments]]
id = "build"
title = "Build the feature"
status = "todo"
guide = "architecture/guide"
justification = "The validated design must become working code."
decisions = ["Use append-only activity for state."]
verification = "Run the focused tests."
depends_on = ["design"]
"#
    }

    fn write_test_activity(fixture: &Fixture, event: &ActivityEvent) -> PathBuf {
        let session_root =
            sessions::session_file_path(&fixture.wiki, &fixture.session.id, PLAN_FILE)
                .unwrap()
                .parent()
                .unwrap()
                .to_path_buf();
        let activity_dir = session_root.join("activity");
        fs::create_dir_all(&activity_dir).unwrap();
        let path = activity_dir.join(format!("{}.toml", event.id));
        fs::write(&path, toml::to_string_pretty(event).unwrap()).unwrap();
        path
    }

    fn publish_plan_without_activity(fixture: &Fixture, checked: &PlanCheck) {
        let guard = fixture.wiki.acquire_mutation_guard().unwrap();
        sessions::write_immutable_session_file_guarded(
            &fixture.wiki,
            &guard,
            &fixture.session.id,
            PLAN_FILE,
            &checked.canonical_toml,
        )
        .unwrap();
        drop(guard);
    }

    #[test]
    fn check_rejects_unknown_fields_missing_guides_and_cycles() {
        let fixture = Fixture::new();
        let unknown = plan().replace(
            "title = \"Implement plan support\"",
            "title = \"Implement plan support\"\nunknown = true",
        );
        assert!(format!("{:#}", check(&fixture.wiki, &unknown).unwrap_err()).contains("unknown"));

        let missing = plan().replace("architecture/guide", "architecture/missing");
        assert!(check(&fixture.wiki, &missing)
            .unwrap_err()
            .to_string()
            .contains("does not exist"));

        let cycle = plan().replace(
            "verification = \"Review the stored definition.\"",
            "verification = \"Review the stored definition.\"\ndepends_on = [\"build\"]",
        );
        assert!(check(&fixture.wiki, &cycle)
            .unwrap_err()
            .to_string()
            .contains("cycle"));

        let invalid_id = plan().replace("id = \"design\"", "id = \"Design\"");
        assert!(check(&fixture.wiki, &invalid_id)
            .unwrap_err()
            .to_string()
            .contains("must match"));

        let oversized = "x".repeat(MAX_PLAN_BYTES + 1);
        assert!(check(&fixture.wiki, &oversized)
            .unwrap_err()
            .to_string()
            .contains("input ceiling"));
    }

    #[test]
    fn plan_visible_text_rejects_escaped_controls_and_bidi_without_echoing_them() {
        let fixture = Fixture::new();
        for (escaped, decoded) in [
            ("safe\\u001b[31m", '\u{001b}'),
            ("safe\\u061cspoof", '\u{061c}'),
            ("safe\\u202espoof", '\u{202e}'),
        ] {
            let raw = plan().replace(
                "title = \"Implement plan support\"",
                &format!("title = \"{escaped}\""),
            );
            let error = check(&fixture.wiki, &raw).unwrap_err().to_string();
            assert!(
                error.contains("control") || error.contains("bidi"),
                "{error}"
            );
            assert!(!error.contains(decoded));
        }
        for query in [
            "line one\nline two",
            "escape\u{001b}[31m",
            "direction\u{200f}change",
        ] {
            let error = clean_task_query(query).unwrap_err().to_string();
            assert!(error.contains("control") || error.contains("bidi"));
            assert!(!error.contains(query));
        }
    }

    #[test]
    fn validation_reuses_shared_guide_lookup_for_large_plans() {
        let fixture = Fixture::new();
        let mut raw = String::from("schema = \"wookie.plan/v1\"\ntitle = \"Shared guide\"\n");
        for index in 0..MAX_PLAN_SEGMENTS {
            raw.push_str(&format!(
                "\n[[segments]]\nid = \"segment-{index}\"\ntitle = \"Segment {index}\"\nstatus = \"todo\"\nguide = \"architecture/guide\"\njustification = \"Bounded shared-guide validation.\"\ndecisions = [\"Reuse one validated guide lookup.\"]\nverification = \"The plan validates.\"\n"
            ));
        }
        assert_eq!(
            check(&fixture.wiki, &raw).unwrap().segment_count,
            MAX_PLAN_SEGMENTS
        );
    }

    #[test]
    fn validation_bounds_individual_and_aggregate_guide_material() {
        let fixture = Fixture::new();
        let mut oversized = fixture.wiki.load_page("architecture/guide").unwrap();
        oversized.body = "x".repeat(MAX_PLAN_GUIDE_BYTES);
        fixture.wiki.save_page_raw(&mut oversized, false).unwrap();
        assert!(check(&fixture.wiki, plan())
            .unwrap_err()
            .to_string()
            .contains("plan validation ceiling"));

        let fixture = Fixture::new();
        let mut raw = String::from("schema = \"wookie.plan/v1\"\ntitle = \"Guide budget\"\n");
        for index in 0..9 {
            let mut guide = Page {
                id: format!("architecture/guide-{index}"),
                fm: Frontmatter {
                    title: format!("Guide {index}"),
                    description: "A bounded guide used by one segment.".into(),
                    created: "2026-07-25".into(),
                    updated: "2026-07-25".into(),
                    ..Frontmatter::default()
                },
                body: format!("**Guide** {}", "x".repeat(480 * 1024)),
            };
            fixture.wiki.save_page_raw(&mut guide, false).unwrap();
            raw.push_str(&format!(
                "\n[[segments]]\nid = \"segment-{index}\"\ntitle = \"Segment {index}\"\nstatus = \"todo\"\nguide = \"architecture/guide-{index}\"\njustification = \"Bound guide material.\"\ndecisions = [\"Reject aggregate expansion.\"]\nverification = \"Validation remains bounded.\"\n"
            ));
        }
        assert!(check(&fixture.wiki, &raw)
            .unwrap_err()
            .to_string()
            .contains("aggregate validation ceiling"));
    }

    #[test]
    fn attach_is_immutable_and_status_events_fold_deterministically() {
        let fixture = Fixture::new();
        let attached = attach(&fixture.wiki, &fixture.session.id, plan()).unwrap();
        assert_eq!(attached.segments.len(), 2);
        assert!(attached.segments[0].ready);
        assert_eq!(attached.segments[1].blocked_by, vec!["design"]);

        let doing = update(
            &fixture.wiki,
            &fixture.session.id,
            "design",
            PlanStatus::Doing,
            Some("Started boundary review."),
        )
        .unwrap();
        assert_eq!(doing.segments[0].status, PlanStatus::Doing);
        let done = update(
            &fixture.wiki,
            &fixture.session.id,
            "design",
            PlanStatus::Done,
            Some("Boundary review passed."),
        )
        .unwrap();
        assert!(done.segments[1].ready);
        assert_eq!(done.plan_hash, attached.plan_hash);

        let changed = plan().replace("Implement plan support", "Different plan");
        assert!(attach(&fixture.wiki, &fixture.session.id, &changed)
            .unwrap_err()
            .to_string()
            .contains("different immutable plan"));
    }

    #[test]
    fn same_millisecond_plan_events_fold_by_persisted_sequence() {
        let fixture = Fixture::new();
        let checked = check(&fixture.wiki, plan()).unwrap();
        publish_plan_without_activity(&fixture, &checked);
        let at = fixture.session.created_at.clone();

        let attached = ActivityEvent {
            id: "activity-20260725-120000-mmmmmmmm".into(),
            at: at.clone(),
            sequence: Some(1),
            action: "plan-attached".into(),
            status: None,
            plan: Some(event_payload("attached", &checked.plan_hash)),
        };
        let mut doing_payload = event_payload("status-changed", &checked.plan_hash);
        doing_payload.segment_id = Some("design".into());
        doing_payload.from_status = Some("todo".into());
        doing_payload.to_status = Some("doing".into());
        let doing = ActivityEvent {
            id: "activity-20260725-120000-zzzzzzzz".into(),
            at: at.clone(),
            sequence: Some(2),
            action: "plan-status".into(),
            status: None,
            plan: Some(doing_payload),
        };
        let mut done_payload = event_payload("status-changed", &checked.plan_hash);
        done_payload.segment_id = Some("design".into());
        done_payload.from_status = Some("doing".into());
        done_payload.to_status = Some("done".into());
        let done = ActivityEvent {
            id: "activity-20260725-120000-aaaaaaaa".into(),
            at,
            sequence: Some(3),
            action: "plan-status".into(),
            status: None,
            plan: Some(done_payload),
        };
        for event in [&attached, &doing, &done] {
            write_test_activity(&fixture, event);
        }

        let current = snapshot(
            &fixture.wiki,
            &fixture.session.id,
            SnapshotOptions::default(),
        )
        .unwrap();
        assert_eq!(current.segments[0].status, PlanStatus::Done);
        assert_eq!(
            current
                .events
                .iter()
                .filter_map(|event| event.sequence)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn folded_events_reject_pre_attach_and_dependency_bypass_tampering() {
        let fixture = Fixture::new();
        let checked = check(&fixture.wiki, plan()).unwrap();
        let mut log_payload = event_payload("log", &checked.plan_hash);
        log_payload.log_kind = Some("progress".into());
        log_payload.summary = Some("Forged early log.".into());
        let early = ActivityEvent {
            id: "activity-20260725-120000-aaaaaaaa".into(),
            at: fixture.session.created_at.clone(),
            sequence: Some(1),
            action: "plan-log".into(),
            status: None,
            plan: Some(log_payload),
        };
        let attached = ActivityEvent {
            id: "activity-20260725-120001-bbbbbbbb".into(),
            at: fixture.session.created_at.clone(),
            sequence: Some(2),
            action: "plan-attached".into(),
            status: None,
            plan: Some(event_payload("attached", &checked.plan_hash)),
        };
        let error = fold_statuses(
            &checked.definition,
            &checked.plan_hash,
            &[early, attached.clone()],
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("precedes"));

        let mut bypass_payload = event_payload("status-changed", &checked.plan_hash);
        bypass_payload.segment_id = Some("build".into());
        bypass_payload.from_status = Some("todo".into());
        bypass_payload.to_status = Some("doing".into());
        let bypass = ActivityEvent {
            id: "activity-20260725-120002-cccccccc".into(),
            at: fixture.session.created_at.clone(),
            sequence: Some(3),
            action: "plan-status".into(),
            status: None,
            plan: Some(bypass_payload),
        };
        let error = fold_statuses(&checked.definition, &checked.plan_hash, &[attached, bypass])
            .unwrap_err()
            .to_string();
        assert!(error.contains("dependencies are incomplete"));

        let attached = ActivityEvent {
            id: "activity-20260725-120003-dddddddd".into(),
            at: "2026-07-25T12:00:03.000Z".into(),
            sequence: Some(1),
            action: "plan-attached".into(),
            status: None,
            plan: Some(event_payload("attached", &checked.plan_hash)),
        };
        let archived = ActivityEvent {
            id: "activity-20260725-120004-eeeeeeee".into(),
            at: "2026-07-25T12:00:04.000Z".into(),
            sequence: Some(2),
            action: "plan-archived".into(),
            status: Some("closed".into()),
            plan: Some(event_payload("archived", &checked.plan_hash)),
        };
        let mut late_log_payload = event_payload("log", &checked.plan_hash);
        late_log_payload.log_kind = Some("note".into());
        late_log_payload.summary = Some("Forged post-archive log.".into());
        let late_log = ActivityEvent {
            id: "activity-20260725-120005-ffffffff".into(),
            at: "2026-07-25T12:00:05.000Z".into(),
            sequence: Some(3),
            action: "plan-log".into(),
            status: None,
            plan: Some(late_log_payload),
        };
        let error = fold_statuses(
            &checked.definition,
            &checked.plan_hash,
            &[attached, archived, late_log],
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("after the terminal archive"));
    }

    #[test]
    fn dependencies_are_enforced() {
        let fixture = Fixture::new();
        attach(&fixture.wiki, &fixture.session.id, plan()).unwrap();
        let dependency_error = update(
            &fixture.wiki,
            &fixture.session.id,
            "build",
            PlanStatus::Doing,
            Some("Starting."),
        )
        .unwrap_err();
        assert!(dependency_error
            .to_string()
            .contains("incomplete dependencies"));
        let done = update(
            &fixture.wiki,
            &fixture.session.id,
            "design",
            PlanStatus::Done,
            None,
        )
        .unwrap();
        assert_eq!(done.segments[0].status, PlanStatus::Done);
    }

    #[test]
    fn same_hash_attach_repairs_a_plan_published_before_its_event() {
        let fixture = Fixture::new();
        let checked = check(&fixture.wiki, plan()).unwrap();
        let guard = fixture.wiki.acquire_mutation_guard().unwrap();
        sessions::write_immutable_session_file_guarded(
            &fixture.wiki,
            &guard,
            &fixture.session.id,
            PLAN_FILE,
            &checked.canonical_toml,
        )
        .unwrap();
        drop(guard);

        let repaired = attach(&fixture.wiki, &fixture.session.id, plan()).unwrap();
        let attachments = repaired
            .events
            .iter()
            .filter(|event| {
                event
                    .plan
                    .as_ref()
                    .is_some_and(|plan| plan.kind == "attached")
            })
            .count();
        assert_eq!(attachments, 1);
        assert_eq!(repaired.plan_hash, checked.plan_hash);
    }

    #[test]
    fn snapshot_survives_a_guide_removed_after_attachment() {
        let fixture = Fixture::new();
        attach(&fixture.wiki, &fixture.session.id, plan()).unwrap();
        fixture.wiki.delete_page("architecture/guide").unwrap();

        let current = snapshot(
            &fixture.wiki,
            &fixture.session.id,
            SnapshotOptions::default(),
        )
        .unwrap();
        assert_eq!(current.segments[0].guide, "architecture/guide");
        assert!(!fixture.wiki.exists("architecture/guide"));
    }

    #[test]
    fn archive_closes_session_and_is_retry_safe() {
        let fixture = Fixture::new();
        attach(&fixture.wiki, &fixture.session.id, plan()).unwrap();
        update(
            &fixture.wiki,
            &fixture.session.id,
            "design",
            PlanStatus::Done,
            Some("Design verified."),
        )
        .unwrap();
        update(
            &fixture.wiki,
            &fixture.session.id,
            "build",
            PlanStatus::Done,
            Some("Build verified."),
        )
        .unwrap();
        let archived = archive(
            &fixture.wiki,
            &fixture.session.id,
            false,
            Some("Implementation complete."),
        )
        .unwrap();
        assert_eq!(archived.snapshot.session.status, "closed");
        assert!(
            sessions::session_file_path(&fixture.wiki, &fixture.session.id, ARCHIVE_FILE)
                .unwrap()
                .exists()
        );
        let retried = archive(&fixture.wiki, &fixture.session.id, false, None).unwrap();
        assert_eq!(
            retried.receipt.receipt_sha256,
            archived.receipt.receipt_sha256
        );
        assert!(!archived.receipt.allow_incomplete);
    }

    #[test]
    fn archive_refuses_incomplete_notification_scan_before_closing() {
        let fixture = Fixture::new();
        attach(&fixture.wiki, &fixture.session.id, plan()).unwrap();
        let session_root =
            sessions::session_file_path(&fixture.wiki, &fixture.session.id, PLAN_FILE)
                .unwrap()
                .parent()
                .unwrap()
                .to_path_buf();
        fs::write(
            session_root.join("notifications/malformed.md"),
            "not a notification",
        )
        .unwrap();

        let current = snapshot(
            &fixture.wiki,
            &fixture.session.id,
            SnapshotOptions::default(),
        )
        .unwrap();
        assert!(current.notifications_scan_complete);
        assert!(current.notification_warnings_total > 0);
        assert!(!current.notification_warnings.is_empty());

        let error = archive(&fixture.wiki, &fixture.session.id, true, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("notification scan"));
        let (session, events) =
            sessions::load_session_and_activity(&fixture.wiki, &fixture.session.id).unwrap();
        assert_eq!(session.status, "active");
        assert!(!events.iter().any(|event| {
            event
                .plan
                .as_ref()
                .is_some_and(|plan| plan.kind == "archived")
        }));
    }

    #[test]
    fn archive_preflights_conflicting_destination_before_close_event() {
        let fixture = Fixture::new();
        attach(&fixture.wiki, &fixture.session.id, plan()).unwrap();
        let archive_path =
            sessions::session_file_path(&fixture.wiki, &fixture.session.id, ARCHIVE_FILE).unwrap();
        fs::write(&archive_path, "conflicting archive").unwrap();

        let error = archive(&fixture.wiki, &fixture.session.id, true, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("conflicting immutable plan archive"));
        let (session, events) =
            sessions::load_session_and_activity(&fixture.wiki, &fixture.session.id).unwrap();
        assert_eq!(session.status, "active");
        assert!(!events.iter().any(|event| {
            event
                .plan
                .as_ref()
                .is_some_and(|plan| plan.kind == "archived")
        }));
    }

    #[cfg(unix)]
    #[test]
    fn archive_preflights_symlink_destination_before_close_event() {
        use std::os::unix::fs::symlink;

        let fixture = Fixture::new();
        attach(&fixture.wiki, &fixture.session.id, plan()).unwrap();
        let archive_path =
            sessions::session_file_path(&fixture.wiki, &fixture.session.id, ARCHIVE_FILE).unwrap();
        let outside = fixture.home.join("missing-outside-archive.md");
        symlink(&outside, &archive_path).unwrap();

        let error = archive(&fixture.wiki, &fixture.session.id, true, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("symlink"));
        let (session, events) =
            sessions::load_session_and_activity(&fixture.wiki, &fixture.session.id).unwrap();
        assert_eq!(session.status, "active");
        assert!(!events.iter().any(|event| {
            event
                .plan
                .as_ref()
                .is_some_and(|plan| plan.kind == "archived")
        }));
    }

    #[test]
    fn archive_retry_detects_count_and_authorization_tampering() {
        for tamper_authorization in [false, true] {
            let fixture = Fixture::new();
            attach(&fixture.wiki, &fixture.session.id, plan()).unwrap();
            update(
                &fixture.wiki,
                &fixture.session.id,
                "design",
                PlanStatus::Done,
                None,
            )
            .unwrap();
            update(
                &fixture.wiki,
                &fixture.session.id,
                "build",
                PlanStatus::Done,
                None,
            )
            .unwrap();
            let archived = archive(&fixture.wiki, &fixture.session.id, false, None).unwrap();
            let event = archived
                .snapshot
                .events
                .iter()
                .find(|event| {
                    event
                        .plan
                        .as_ref()
                        .is_some_and(|plan| plan.kind == "archived")
                })
                .unwrap()
                .clone();
            let event_path =
                sessions::session_file_path(&fixture.wiki, &fixture.session.id, PLAN_FILE)
                    .unwrap()
                    .parent()
                    .unwrap()
                    .join("activity")
                    .join(format!("{}.toml", event.id));
            let mut tampered: ActivityEvent =
                toml::from_str(&fs::read_to_string(&event_path).unwrap()).unwrap();
            let payload = tampered.plan.as_mut().unwrap();
            if tamper_authorization {
                payload.allow_incomplete = Some(true);
            } else {
                payload.done_segments = Some(1);
                payload.incomplete_segments = Some(1);
            }
            fs::write(&event_path, toml::to_string_pretty(&tampered).unwrap()).unwrap();

            let error = archive(&fixture.wiki, &fixture.session.id, false, None)
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("receipt") || error.contains("authorization"),
                "{error}"
            );
        }
    }

    #[test]
    fn archive_escapes_markdown_and_html_from_all_external_prose() {
        let fixture = Fixture::new();
        let malicious_plan = plan()
            .replace(
                "Implement plan support",
                "![title](https://example.invalid/title) <img src=x>",
            )
            .replace(
                "Confirm boundaries",
                "[segment](https://example.invalid/segment)",
            );
        attach(&fixture.wiki, &fixture.session.id, &malicious_plan).unwrap();
        log(
            &fixture.wiki,
            &fixture.session.id,
            None,
            PlanLogKind::Note,
            "![log](https://example.invalid/log) <!-- comment -->",
        )
        .unwrap();
        sessions::notify_with_request(
            &fixture.wiki,
            sessions::NotifyRequest {
                source_session: fixture.session.id.clone(),
                summary: "![notification](https://example.invalid/notify) <script>x</script>"
                    .into(),
                ..sessions::NotifyRequest::default()
            },
        )
        .unwrap();
        let archived = archive(
            &fixture.wiki,
            &fixture.session.id,
            true,
            Some("Done. ![summary](https://example.invalid/summary) <b>unsafe</b>"),
        )
        .unwrap();
        let archive_path = fixture.wiki.dir.join(&archived.archive_path);
        let markdown = fs::read_to_string(archive_path).unwrap();
        assert!(!markdown.contains("!["));
        assert!(!markdown.contains("<img"));
        assert!(!markdown.contains("<script"));
        assert!(!markdown.contains("<!--"));
        assert!(markdown.contains("\\!\\["));
        assert!(markdown.contains("&lt;img"));
        assert!(markdown.contains("Done."));
    }
}
