//! Deterministic, bounded retrieval helpers.
//!
//! This module intentionally contains no index, embedding, cache, or model
//! dependency. Callers provide page metadata and text; the same inputs always
//! produce the same ranking and stable tie order.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

const EXACT_ID_SCORE: u32 = 1_000;
const EXACT_TITLE_SCORE: u32 = 900;
const ID_TERM_SCORE: u32 = 120;
const TITLE_TERM_SCORE: u32 = 100;
const TAG_SCORE: u32 = 80;
const SOURCE_SCORE: u32 = 60;
const DESCRIPTION_SCORE: u32 = 35;
const BODY_SCORE: u32 = 15;
const LINK_SCORE: u32 = 25;
const STALE_SCORE: u32 = 5;
const MAX_TERMS_PER_REASON: usize = 8;
const MAX_REASON_DETAILS: usize = 4;
const MAX_DESCRIPTION_CHARS: usize = 320;
const MAX_EXCERPT_CHARS: usize = 240;
pub const MAX_QUERY_BYTES: usize = 4 * 1024;
pub const MAX_QUERY_TERMS: usize = 64;

/// Whether the wiki's code-backed knowledge is known to match its project.
///
/// `Current` is deliberately the strongest state: callers may use it only
/// after a successful comparison against the recorded ingest revision found
/// no changed paths. Missing configuration and comparison failures are
/// `Unknown`, never optimistic `Current` results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FreshnessState {
    Current,
    Stale,
    Unknown,
}

impl std::fmt::Display for FreshnessState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Current => "current",
            Self::Stale => "stale",
            Self::Unknown => "unknown",
        })
    }
}

/// Structured result of comparing project changes with page provenance.
/// Counts are optional because a failed or unconfigured comparison cannot
/// truthfully claim that it observed zero changes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FreshnessOutcome {
    pub state: FreshnessState,
    pub changed_count: Option<usize>,
    pub stale_page_ids: Vec<String>,
    pub uncovered_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl FreshnessOutcome {
    /// Produce an explicit unknown result without inventing zero counts.
    pub fn unknown(error: impl Into<String>) -> Self {
        let error = error.into();
        Self {
            state: FreshnessState::Unknown,
            changed_count: None,
            stale_page_ids: Vec::new(),
            uncovered_count: None,
            error: Some(compact(&error, MAX_DESCRIPTION_CHARS)),
        }
    }

    /// Map a successfully computed set of changed project paths to page
    /// provenance. Any changed path makes the wiki stale, even when no page
    /// claims it; those paths are counted as uncovered work.
    pub fn from_changed_paths(pages: &[RetrievalPage], changed_paths: &[String]) -> Self {
        let changed: BTreeSet<String> = changed_paths
            .iter()
            .map(|path| normalize_project_path(path))
            .filter(|path| !path.is_empty())
            .collect();
        let mut stale_page_ids = BTreeSet::new();
        let mut covered_paths = BTreeSet::new();

        for page in pages {
            for path in &changed {
                if page
                    .sources
                    .iter()
                    .any(|source| source_covers_path(source, path))
                {
                    stale_page_ids.insert(page.id.clone());
                    covered_paths.insert(path.clone());
                }
            }
        }

        Self {
            state: if changed.is_empty() {
                FreshnessState::Current
            } else {
                FreshnessState::Stale
            },
            changed_count: Some(changed.len()),
            stale_page_ids: stale_page_ids.into_iter().collect(),
            uncovered_count: Some(changed.len().saturating_sub(covered_paths.len())),
            error: None,
        }
    }

    pub fn is_stale(&self, page_id: &str) -> bool {
        self.stale_page_ids
            .binary_search_by(|candidate| candidate.as_str().cmp(page_id))
            .is_ok()
    }
}

fn normalize_project_path(path: &str) -> String {
    path.replace('\\', "/")
        .trim_start_matches("./")
        .trim_end_matches('/')
        .to_string()
}

fn source_covers_path(source: &str, changed_path: &str) -> bool {
    let source = normalize_project_path(source);
    if source.is_empty() {
        return false;
    }
    source == "."
        || changed_path == source
        || changed_path
            .strip_prefix(&source)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

/// The minimal page projection needed by deterministic retrieval.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetrievalPage {
    pub id: String,
    pub title: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<String>,
    #[serde(default)]
    pub stale: bool,
}

impl RetrievalPage {
    /// Build the retrieval projection from Wookie's canonical page model.
    pub fn from_page(page: &crate::page::Page, stale: bool) -> Self {
        Self {
            id: page.id.clone(),
            title: page.fm.title.clone(),
            description: page.fm.description.clone(),
            tags: page.fm.tags.clone(),
            sources: page.fm.sources.clone(),
            body: page.body.clone(),
            links: page.links(),
            stale,
        }
    }
}

/// Stable reason categories. Declaration order is also presentation order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasonKind {
    ExactId,
    ExactTitle,
    Id,
    Title,
    Tag,
    Source,
    Text,
    Link,
    Stale,
}

/// An explainable reason a result was selected.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RankReason {
    pub kind: ReasonKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl RankReason {
    fn new(kind: ReasonKind, detail: impl Into<Option<String>>) -> Self {
        Self {
            kind,
            detail: detail.into(),
        }
    }
}

/// Compact ranked output. Full page bodies remain available through `read`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RankedPage {
    pub id: String,
    pub title: String,
    pub description: String,
    pub score: u32,
    pub reasons: Vec<RankReason>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
    #[serde(default)]
    pub stale: bool,
}

impl RankedPage {
    /// Estimated cost of the result's compact text representation. This is
    /// used only for budgeting; callers may present the fields differently.
    pub fn estimated_tokens(&self) -> usize {
        let mut representation = format!(
            "{}\n{}\n{}\nscore: {}\n",
            self.id, self.title, self.description, self.score
        );
        for reason in &self.reasons {
            representation.push_str(&format!("{:?}", reason.kind));
            if let Some(detail) = &reason.detail {
                representation.push_str(": ");
                representation.push_str(detail);
            }
            representation.push('\n');
        }
        if let Some(excerpt) = &self.excerpt {
            representation.push_str(excerpt);
            representation.push('\n');
        }
        // Account for JSON/Markdown labels and separators added by a command.
        estimate_tokens(&representation).saturating_add(8)
    }
}

#[derive(Debug, Clone)]
struct ScoredPage<'a> {
    page: &'a RetrievalPage,
    direct_score: u32,
    score: u32,
    reasons: BTreeSet<RankReason>,
}

/// Rank pages for a natural-language query. Zero-score pages are omitted.
/// An empty query deliberately returns only stale pages; catalog display is a
/// separate concern handled by `context`/`prime`.
pub fn rank_pages(query: &str, pages: &[RetrievalPage]) -> Vec<RankedPage> {
    let normalized_query = normalize(query.trim());
    let terms = query_terms(query);
    let mut scored: Vec<ScoredPage<'_>> = pages
        .iter()
        .map(|page| score_direct(page, &normalized_query, &terms))
        .collect();

    // Link relevance is one hop from a direct match. Both outgoing and
    // incoming graph edges count, but link-only results do not recursively
    // expand the graph.
    let direct_by_id: BTreeMap<String, u32> = scored
        .iter()
        .filter(|item| item.direct_score > 0)
        .map(|item| (item.page.id.clone(), item.direct_score))
        .collect();
    let mut incoming_from_direct: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for item in scored.iter().filter(|item| item.direct_score > 0) {
        for target in &item.page.links {
            if target != &item.page.id {
                incoming_from_direct
                    .entry(target.clone())
                    .or_default()
                    .insert(item.page.id.clone());
            }
        }
    }

    for item in &mut scored {
        let mut neighbors = BTreeSet::new();
        for target in &item.page.links {
            if direct_by_id.get(target.as_str()).copied().unwrap_or(0) > 0
                && target != &item.page.id
            {
                neighbors.insert(target.clone());
            }
        }
        if let Some(sources) = incoming_from_direct.get(&item.page.id) {
            neighbors.extend(sources.iter().cloned());
        }
        if !neighbors.is_empty() {
            let detail = join_limited(neighbors.into_iter(), MAX_REASON_DETAILS);
            item.score = item.score.saturating_add(LINK_SCORE);
            item.reasons
                .insert(RankReason::new(ReasonKind::Link, Some(detail)));
        }
        // Staleness is a small boost, never a substitute for relevance unless
        // the caller intentionally supplied an empty query.
        if item.page.stale && (item.score > 0 || normalized_query.is_empty()) {
            item.score = item.score.saturating_add(STALE_SCORE);
            item.reasons
                .insert(RankReason::new(ReasonKind::Stale, None));
        }
    }

    let mut ranked: Vec<RankedPage> = scored
        .into_iter()
        .filter(|item| item.score > 0)
        .map(|item| {
            let excerpt = matching_excerpt(&item.page.body, &terms);
            RankedPage {
                id: item.page.id.clone(),
                title: item.page.title.clone(),
                description: compact(&item.page.description, MAX_DESCRIPTION_CHARS),
                score: item.score,
                reasons: item.reasons.into_iter().collect(),
                excerpt,
                stale: item.page.stale,
            }
        })
        .collect();
    ranked.sort_by(|left, right| {
        right
            .score
            .cmp(&left.score)
            .then_with(|| left.id.cmp(&right.id))
            .then_with(|| left.title.cmp(&right.title))
    });
    ranked
}

fn score_direct<'a>(page: &'a RetrievalPage, query: &str, terms: &[String]) -> ScoredPage<'a> {
    let id = normalize(&page.id);
    let title = normalize(&page.title);
    let mut score = 0_u32;
    let mut reasons = BTreeSet::new();

    if !query.is_empty() && id == query {
        score = score.saturating_add(EXACT_ID_SCORE);
        reasons.insert(RankReason::new(ReasonKind::ExactId, None));
    }
    if !query.is_empty() && title == query {
        score = score.saturating_add(EXACT_TITLE_SCORE);
        reasons.insert(RankReason::new(ReasonKind::ExactTitle, None));
    }

    // A stopword-only query can still resolve an exact id or title, but it
    // must not force full-body scans or make common prose look relevant.
    if terms.is_empty() {
        return ScoredPage {
            page,
            direct_score: score,
            score,
            reasons,
        };
    }

    let id_terms = matching_terms(&id, terms);
    if !id_terms.is_empty() && id != query {
        score = score.saturating_add(
            ID_TERM_SCORE.saturating_mul(id_terms.len().min(MAX_TERMS_PER_REASON) as u32),
        );
        reasons.insert(RankReason::new(
            ReasonKind::Id,
            Some(join_limited(id_terms.into_iter(), MAX_TERMS_PER_REASON)),
        ));
    }
    let title_terms = matching_terms(&title, terms);
    if !title_terms.is_empty() && title != query {
        score = score.saturating_add(
            TITLE_TERM_SCORE.saturating_mul(title_terms.len().min(MAX_TERMS_PER_REASON) as u32),
        );
        reasons.insert(RankReason::new(
            ReasonKind::Title,
            Some(join_limited(title_terms.into_iter(), MAX_TERMS_PER_REASON)),
        ));
    }

    let matching_tags: BTreeSet<String> = page
        .tags
        .iter()
        .filter(|tag| terms.iter().any(|term| normalize(tag) == *term))
        .cloned()
        .collect();
    if !matching_tags.is_empty() {
        score = score.saturating_add(
            TAG_SCORE.saturating_mul(matching_tags.len().min(MAX_REASON_DETAILS) as u32),
        );
        reasons.insert(RankReason::new(
            ReasonKind::Tag,
            Some(join_limited(matching_tags.into_iter(), MAX_REASON_DETAILS)),
        ));
    }

    let full_query_is_informative = !query.is_empty();
    let matching_sources: BTreeSet<String> = page
        .sources
        .iter()
        .filter(|source| {
            let normalized = normalize(source);
            (full_query_is_informative && normalized.contains(query))
                || terms.iter().any(|term| normalized.contains(term))
        })
        .cloned()
        .collect();
    if !matching_sources.is_empty() {
        score = score.saturating_add(
            SOURCE_SCORE.saturating_mul(matching_sources.len().min(MAX_REASON_DETAILS) as u32),
        );
        reasons.insert(RankReason::new(
            ReasonKind::Source,
            Some(join_limited(
                matching_sources.into_iter(),
                MAX_REASON_DETAILS,
            )),
        ));
    }

    let description = normalize(&page.description);
    let body = normalize(&page.body);
    let mut text_fields = Vec::new();
    if !matching_terms(&description, terms).is_empty()
        || (full_query_is_informative && description.contains(query))
    {
        score = score.saturating_add(DESCRIPTION_SCORE);
        text_fields.push("description".to_string());
    }
    if !matching_terms(&body, terms).is_empty()
        || (full_query_is_informative && body.contains(query))
    {
        score = score.saturating_add(BODY_SCORE);
        text_fields.push("body".to_string());
    }
    if !text_fields.is_empty() {
        reasons.insert(RankReason::new(
            ReasonKind::Text,
            Some(text_fields.join(", ")),
        ));
    }

    ScoredPage {
        page,
        direct_score: score,
        score,
        reasons,
    }
}

fn normalize(value: &str) -> String {
    value.to_lowercase()
}

/// Query terms preserve useful path punctuation while stripping surrounding
/// prose punctuation. Repeated terms are collapsed in stable order.
pub fn query_terms(query: &str) -> Vec<String> {
    collect_query_terms(query, MAX_QUERY_TERMS)
        .into_iter()
        .collect()
}

/// Reject requests that would make deterministic ranking scale with an
/// effectively unbounded user-controlled query.
pub fn validate_query(query: &str) -> Result<()> {
    if query.len() > MAX_QUERY_BYTES {
        bail!("retrieval query exceeds the {MAX_QUERY_BYTES}-byte limit");
    }
    if collect_query_terms(query, MAX_QUERY_TERMS + 1).len() > MAX_QUERY_TERMS {
        bail!("retrieval query exceeds the {MAX_QUERY_TERMS}-term limit");
    }
    Ok(())
}

fn collect_query_terms(query: &str, limit: usize) -> BTreeSet<String> {
    let mut terms = BTreeSet::new();
    for raw in query.split(|character: char| {
        !(character.is_alphanumeric() || matches!(character, '-' | '_' | '/' | '.'))
    }) {
        let term = raw.trim_matches('.').to_lowercase();
        if !term.is_empty()
            && (term.chars().count() > 1 || term.contains(['/', '.']))
            && !is_stopword(&term)
        {
            terms.insert(term);
            if terms.len() >= limit {
                break;
            }
        }
    }
    terms
}

fn is_stopword(term: &str) -> bool {
    matches!(
        term,
        "a" | "an"
            | "and"
            | "are"
            | "as"
            | "at"
            | "be"
            | "by"
            | "for"
            | "from"
            | "how"
            | "i"
            | "in"
            | "is"
            | "it"
            | "of"
            | "on"
            | "or"
            | "that"
            | "the"
            | "this"
            | "to"
            | "was"
            | "what"
            | "when"
            | "where"
            | "which"
            | "why"
            | "with"
            | "you"
    )
}

fn matching_terms(haystack: &str, terms: &[String]) -> BTreeSet<String> {
    terms
        .iter()
        .filter(|term| haystack.contains(term.as_str()))
        .cloned()
        .collect()
}

fn join_limited(values: impl Iterator<Item = String>, limit: usize) -> String {
    values.take(limit).collect::<Vec<_>>().join(", ")
}

fn matching_excerpt(body: &str, terms: &[String]) -> Option<String> {
    let selected = terms
        .iter()
        .find_map(|term| {
            body.lines()
                .find(|line| normalize(line).contains(term.as_str()) && !line.trim().is_empty())
        })
        .or_else(|| body.lines().find(|line| !line.trim().is_empty()))?;
    let compacted = compact(selected.trim(), MAX_EXCERPT_CHARS);
    (!compacted.is_empty()).then_some(compacted)
}

fn compact(value: &str, max_chars: usize) -> String {
    let one_line = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() <= max_chars {
        return one_line;
    }
    let mut output: String = one_line.chars().take(max_chars.saturating_sub(1)).collect();
    output.push('…');
    output
}

pub(crate) fn compact_excerpt(value: &str) -> String {
    compact(value, MAX_EXCERPT_CHARS)
}

/// Approximate LLM tokens conservatively for ordinary Markdown: one token per
/// three UTF-8 bytes, rounded up. It intentionally requires no tokenizer or
/// model dependency. This is an estimate, not a model-specific hard limit.
pub fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.len().saturating_add(2) / 3
    }
}

/// Estimate the complete standing-instruction entry emitted by `prime`.
///
/// Keep health checks and task-start enforcement on this shared framing so a
/// wiki cannot pass `doctor` and then fail `prime` at the same configured
/// instruction budget.
pub fn estimate_standing_tokens(id: &str, text: &str) -> usize {
    estimate_tokens(&format!("{id}\n{text}\n"))
}

/// Append-only text builder that never exceeds its estimated token budget.
#[derive(Debug, Clone)]
#[cfg(test)]
pub struct BudgetWriter {
    output: String,
    budget_tokens: usize,
    estimated_tokens: usize,
}

#[cfg(test)]
impl BudgetWriter {
    pub fn new(budget_tokens: usize) -> Self {
        Self {
            output: String::new(),
            budget_tokens,
            estimated_tokens: 0,
        }
    }

    /// Append a complete chunk or leave the output unchanged.
    pub fn try_push(&mut self, chunk: &str) -> bool {
        let cost = estimate_tokens(chunk);
        if cost > self.remaining_tokens() {
            return false;
        }
        self.output.push_str(chunk);
        self.estimated_tokens = self.estimated_tokens.saturating_add(cost);
        true
    }

    pub fn into_string(self) -> String {
        self.output
    }

    pub fn estimated_tokens(&self) -> usize {
        self.estimated_tokens
    }

    pub fn remaining_tokens(&self) -> usize {
        self.budget_tokens.saturating_sub(self.estimated_tokens)
    }
}

/// Controls shared by bounded search/prime result selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionOptions {
    pub token_budget: usize,
    pub limit: usize,
    /// Zero-based result offset used as a simple continuation cursor.
    #[serde(default)]
    pub offset: usize,
}

impl Default for SelectionOptions {
    fn default() -> Self {
        Self {
            token_budget: 1_500,
            limit: 10,
            offset: 0,
        }
    }
}

impl SelectionOptions {
    pub fn validate(&self) -> Result<()> {
        if self.token_budget == 0 {
            bail!("retrieval token budget must be greater than zero");
        }
        if self.limit == 0 {
            bail!("retrieval result limit must be greater than zero");
        }
        Ok(())
    }
}

/// Compact, stable telemetry suitable for human output or JSON reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetrievalTelemetry {
    pub pages_considered: usize,
    pub pages_matched: usize,
    pub pages_returned: usize,
    pub pages_omitted: usize,
    pub estimated_tokens: usize,
    pub budget_tokens: usize,
    pub limit: usize,
    pub query_terms: usize,
}

/// A bounded prefix of ranked results. `next_offset` is present whenever more
/// ranked results remain reachable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetrievalSelection {
    pub results: Vec<RankedPage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<usize>,
    /// Set when the highest-ranked remaining result cannot fit even into an
    /// empty budget. No continuation is returned in that case, preventing a
    /// caller from retrying the same cursor forever; `read <id>` remains the
    /// explicit path to that full result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_blocked_by: Option<String>,
    pub telemetry: RetrievalTelemetry,
}

/// Select a ranked prefix within both result and estimated-token limits.
pub fn select_ranked(
    ranked: &[RankedPage],
    pages_considered: usize,
    query: &str,
    options: SelectionOptions,
) -> Result<RetrievalSelection> {
    options.validate()?;
    let start = options.offset.min(ranked.len());
    let mut results = Vec::new();
    let mut estimated_tokens = 0_usize;
    let mut cursor = start;
    let mut budget_blocked_by = None;
    while cursor < ranked.len() && results.len() < options.limit {
        let result = &ranked[cursor];
        let cost = result.estimated_tokens();
        if cost > options.token_budget.saturating_sub(estimated_tokens) {
            if results.is_empty() {
                budget_blocked_by = Some(result.id.clone());
            }
            break;
        }
        estimated_tokens = estimated_tokens.saturating_add(cost);
        results.push(result.clone());
        cursor += 1;
    }
    let next_offset = if budget_blocked_by.is_some() {
        None
    } else {
        (cursor < ranked.len()).then_some(cursor)
    };
    let pages_omitted = ranked
        .len()
        .saturating_sub(start.saturating_add(results.len()));
    Ok(RetrievalSelection {
        results,
        next_offset,
        budget_blocked_by,
        telemetry: RetrievalTelemetry {
            pages_considered,
            pages_matched: ranked.len(),
            pages_returned: cursor.saturating_sub(start),
            pages_omitted,
            estimated_tokens,
            budget_tokens: options.token_budget,
            limit: options.limit,
            query_terms: query_terms(query).len(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(id: &str, title: &str) -> RetrievalPage {
        RetrievalPage {
            id: id.into(),
            title: title.into(),
            description: format!("Summary for {title}"),
            ..RetrievalPage::default()
        }
    }

    #[test]
    fn successful_zero_change_comparison_is_current() {
        let freshness = FreshnessOutcome::from_changed_paths(&[page("code/lib", "Lib")], &[]);
        assert_eq!(freshness.state, FreshnessState::Current);
        assert_eq!(freshness.changed_count, Some(0));
        assert_eq!(freshness.uncovered_count, Some(0));
        assert!(freshness.stale_page_ids.is_empty());
        assert_eq!(freshness.error, None);
    }

    #[test]
    fn uncovered_changes_are_stale_even_without_a_mapped_page() {
        let mut documented = page("code/lib", "Lib");
        documented.sources = vec!["src/lib.rs".into()];
        let freshness = FreshnessOutcome::from_changed_paths(
            &[documented],
            &["src/new.rs".into(), "src/new.rs".into()],
        );
        assert_eq!(freshness.state, FreshnessState::Stale);
        assert_eq!(freshness.changed_count, Some(1));
        assert_eq!(freshness.uncovered_count, Some(1));
        assert!(freshness.stale_page_ids.is_empty());
    }

    #[test]
    fn changed_sources_map_to_stable_stale_page_ids() {
        let mut worker = page("code/worker", "Worker");
        worker.sources = vec!["src/worker/".into()];
        let mut architecture = page("architecture/worker", "Worker architecture");
        architecture.sources = vec!["./src/worker".into()];
        let freshness = FreshnessOutcome::from_changed_paths(
            &[worker, architecture],
            &["src/worker/run.rs".into(), "README.md".into()],
        );
        assert_eq!(freshness.state, FreshnessState::Stale);
        assert_eq!(freshness.changed_count, Some(2));
        assert_eq!(freshness.uncovered_count, Some(1));
        assert_eq!(
            freshness.stale_page_ids,
            vec!["architecture/worker", "code/worker"]
        );
        assert!(freshness.is_stale("code/worker"));
        assert!(!freshness.is_stale("code/other"));
    }

    #[test]
    fn unknown_freshness_does_not_claim_zero_changes() {
        let freshness = FreshnessOutcome::unknown("invalid ingest revision");
        assert_eq!(freshness.state, FreshnessState::Unknown);
        assert_eq!(freshness.changed_count, None);
        assert_eq!(freshness.uncovered_count, None);
        assert_eq!(freshness.error.as_deref(), Some("invalid ingest revision"));
    }

    #[test]
    fn query_size_and_term_count_have_hard_ceilings() {
        let oversized = "x".repeat(MAX_QUERY_BYTES + 1);
        assert!(validate_query(&oversized).is_err());

        let too_many_terms = (0..=MAX_QUERY_TERMS)
            .map(|index| format!("term{index}"))
            .collect::<Vec<_>>()
            .join(" ");
        assert!(validate_query(&too_many_terms).is_err());
        assert_eq!(query_terms(&too_many_terms).len(), MAX_QUERY_TERMS);
    }

    #[test]
    fn exact_and_field_matches_are_ranked_deterministically() {
        let mut exact = page("architecture/cache", "Cache");
        exact.tags.push("performance".into());
        let mut source = page("code/cache", "Cache module");
        source.sources.push("src/cache.rs".into());
        source.body = "Fast storage implementation".into();
        let pages = vec![source, exact];

        let first = rank_pages("architecture/cache", &pages);
        let second = rank_pages("architecture/cache", &pages);
        assert_eq!(first, second);
        assert_eq!(first[0].id, "architecture/cache");
        assert!(first[0]
            .reasons
            .iter()
            .any(|reason| reason.kind == ReasonKind::ExactId));
    }

    #[test]
    fn tie_breaks_by_page_id() {
        let pages = vec![page("z-page", "Cache"), page("a-page", "Cache")];
        let ranked = rank_pages("cache", &pages);
        assert_eq!(
            ranked
                .iter()
                .map(|item| item.id.as_str())
                .collect::<Vec<_>>(),
            vec!["a-page", "z-page"]
        );
    }

    #[test]
    fn tag_source_text_and_stale_reasons_are_explained() {
        let candidate = RetrievalPage {
            id: "code/worker".into(),
            title: "Worker".into(),
            description: "Background cache worker".into(),
            tags: vec!["performance".into()],
            sources: vec!["src/cache/worker.rs".into()],
            body: "The cache is refreshed here.".into(),
            stale: true,
            ..RetrievalPage::default()
        };
        let ranked = rank_pages("performance src/cache cache", &[candidate]);
        let kinds: Vec<ReasonKind> = ranked[0].reasons.iter().map(|reason| reason.kind).collect();
        assert!(kinds.contains(&ReasonKind::Tag));
        assert!(kinds.contains(&ReasonKind::Source));
        assert!(kinds.contains(&ReasonKind::Text));
        assert!(kinds.contains(&ReasonKind::Stale));
    }

    #[test]
    fn link_neighbors_are_included_but_not_recursively_expanded() {
        let direct = RetrievalPage {
            id: "architecture/cache".into(),
            title: "Cache".into(),
            links: vec!["decisions/cache-policy".into()],
            ..RetrievalPage::default()
        };
        let neighbor = RetrievalPage {
            id: "decisions/cache-policy".into(),
            title: "Eviction policy".into(),
            links: vec!["guides/operations".into()],
            ..RetrievalPage::default()
        };
        let distant = page("guides/operations", "Operations");
        let ranked = rank_pages("architecture/cache", &[direct, neighbor, distant]);
        assert!(ranked.iter().any(|item| {
            item.id == "decisions/cache-policy"
                && item
                    .reasons
                    .iter()
                    .any(|reason| reason.kind == ReasonKind::Link)
        }));
        assert!(!ranked.iter().any(|item| item.id == "guides/operations"));
    }

    #[test]
    fn stale_is_only_a_boost_for_nonempty_queries() {
        let stale = RetrievalPage {
            id: "unrelated".into(),
            title: "Unrelated".into(),
            stale: true,
            ..RetrievalPage::default()
        };
        assert!(rank_pages("cache", std::slice::from_ref(&stale)).is_empty());
        let ranked = rank_pages("", &[stale]);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].reasons[0].kind, ReasonKind::Stale);
    }

    #[test]
    fn budget_writer_never_partially_appends() {
        let mut writer = BudgetWriter::new(3);
        assert!(writer.try_push("abcdef")); // two estimated tokens
        assert!(!writer.try_push("another chunk"));
        assert_eq!(writer.estimated_tokens(), 2);
        assert_eq!(writer.into_string(), "abcdef");
    }

    #[test]
    fn selector_honors_limit_budget_and_continuation() {
        let pages = vec![
            page("cache-a", "Cache A"),
            page("cache-b", "Cache B"),
            page("cache-c", "Cache C"),
        ];
        let ranked = rank_pages("cache", &pages);
        let enough_for_one = ranked[0].estimated_tokens();
        let selected = select_ranked(
            &ranked,
            pages.len(),
            "cache",
            SelectionOptions {
                token_budget: enough_for_one,
                limit: 2,
                offset: 0,
            },
        )
        .unwrap();
        assert_eq!(selected.results.len(), 1);
        assert_eq!(selected.next_offset, Some(1));
        assert!(selected.telemetry.estimated_tokens <= enough_for_one);
        assert_eq!(selected.telemetry.pages_considered, 3);
        assert_eq!(selected.telemetry.pages_returned, 1);
    }

    #[test]
    fn selection_offset_reports_only_remaining_omissions() {
        let pages = vec![
            page("cache-a", "Cache A"),
            page("cache-b", "Cache B"),
            page("cache-c", "Cache C"),
        ];
        let ranked = rank_pages("cache", &pages);
        let selected = select_ranked(
            &ranked,
            pages.len(),
            "cache",
            SelectionOptions {
                token_budget: 10_000,
                limit: 1,
                offset: 1,
            },
        )
        .unwrap();
        assert_eq!(selected.results.len(), 1);
        assert_eq!(selected.telemetry.pages_omitted, 1);
        assert_eq!(selected.next_offset, Some(2));
    }

    #[test]
    fn oversized_first_result_does_not_return_a_stalled_cursor() {
        let ranked = rank_pages("cache", &[page("cache", "Cache")]);
        let selected = select_ranked(
            &ranked,
            1,
            "cache",
            SelectionOptions {
                token_budget: 1,
                limit: 1,
                offset: 0,
            },
        )
        .unwrap();
        assert!(selected.results.is_empty());
        assert_eq!(selected.next_offset, None);
        assert_eq!(selected.budget_blocked_by.as_deref(), Some("cache"));
    }

    #[test]
    fn zero_selection_options_are_rejected() {
        let ranked = rank_pages("cache", &[page("cache", "Cache")]);
        let error = select_ranked(
            &ranked,
            1,
            "cache",
            SelectionOptions {
                token_budget: 0,
                limit: 1,
                offset: 0,
            },
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("greater than zero"), "{error}");
    }

    #[test]
    fn stopwords_do_not_flatten_ranking_but_exact_queries_remain() {
        let mut generic = page("guides/general", "How the tool is used");
        generic.body = "A general page with common prose.".into();
        generic.sources = vec!["src/data.rs".into()];
        let target = page("architecture/cache", "Cache");
        let ranked = rank_pages("how is the cache", &[generic.clone(), target]);
        assert_eq!(ranked[0].id, "architecture/cache");
        assert_eq!(query_terms("how is the cache"), vec!["cache"]);

        let exact = rank_pages("a", &[page("a", "Single")]);
        assert_eq!(exact[0].id, "a");
        assert!(exact[0]
            .reasons
            .iter()
            .any(|reason| reason.kind == ReasonKind::ExactId));

        assert!(rank_pages("a", &[generic]).is_empty());
        assert_eq!(
            query_terms("context,pinned; cache."),
            vec!["cache", "context", "pinned"]
        );
    }

    #[test]
    fn excerpts_and_descriptions_are_compact() {
        let candidate = RetrievalPage {
            id: "long".into(),
            title: "Long".into(),
            description: "word ".repeat(200),
            body: format!("intro\n{} cache match", "x".repeat(400)),
            ..RetrievalPage::default()
        };
        let ranked = rank_pages("cache", &[candidate]);
        assert!(ranked[0].description.chars().count() <= MAX_DESCRIPTION_CHARS);
        assert!(ranked[0].excerpt.as_ref().unwrap().chars().count() <= MAX_EXCERPT_CHARS);
    }
}
