mod audit;
mod commands;
mod config;
mod git_paths;
mod history;
mod mcp;
mod page;
mod plugins;
mod protocol;
mod publish;
mod report;
mod retrieval;
mod retrieval_index;
mod sessions;
mod settings;
mod snapshot;
mod wiki;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use std::collections::BTreeMap;
use std::io::{IsTerminal, Read};
use std::path::PathBuf;

const MAX_PAGE_STDIN_BYTES: usize = 16 * 1024 * 1024;

fn parse_read_expand_depth(value: &str) -> std::result::Result<usize, String> {
    let depth = value
        .parse::<usize>()
        .map_err(|_| "expand depth must be a non-negative integer".to_string())?;
    if depth > commands::MAX_READ_EXPAND_DEPTH {
        return Err(format!(
            "expand depth must not exceed {}",
            commands::MAX_READ_EXPAND_DEPTH
        ));
    }
    Ok(depth)
}

#[derive(Parser)]
#[command(name = "wookie", version, about = "LLM-first wiki manager")]
struct Cli {
    /// Wiki slug to operate on (default: resolved from the current directory)
    #[arg(long, global = true)]
    wiki: Option<String>,

    /// Emit machine-readable JSON
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a wiki for a project and register it
    Init {
        /// Wiki slug (default: derived from the project directory name)
        slug: Option<String>,
        /// Project root to register (default: this git repo's main worktree, else cwd)
        #[arg(long)]
        project: Option<PathBuf>,
        /// One-line wiki description
        #[arg(long)]
        description: Option<String>,
    },
    /// List all wikis
    List,
    /// Manage an agent session used for cross-session coordination
    Session {
        #[command(subcommand)]
        cmd: SessionCmd,
    },
    /// Publish a short notification from this agent session
    Notify {
        /// Source session id returned by `wookie session start`
        #[arg(long)]
        session: Option<String>,
        /// One-line description used by other agents to judge relevance
        #[arg(long)]
        summary: String,
        /// Notification type (defaults to sessions.default_kind)
        #[arg(long, value_enum)]
        kind: Option<sessions::NotificationKind>,
        /// Relevance priority (defaults to sessions.default_importance)
        #[arg(long, value_enum)]
        importance: Option<sessions::Importance>,
        /// Comma-separated project paths affected by the work
        #[arg(long)]
        paths: Option<String>,
        /// Comma-separated receiving session ids; omit to broadcast
        #[arg(long = "to")]
        targets: Option<String>,
        /// Stable retry key; repeated publication returns the original notice
        #[arg(long)]
        idempotency_key: Option<String>,
        /// Routing metadata as KEY=VALUE (repeatable)
        #[arg(long = "metadata")]
        metadata: Vec<String>,
        /// Do not attach the current branch, commit, worktree, and dirty paths
        #[arg(long)]
        no_git_context: bool,
    },
    /// List notifications visible to an agent session
    Notifications {
        #[arg(long)]
        session: Option<String>,
        /// Include already read, dismissed, and pre-existing notifications
        #[arg(long)]
        all: bool,
        /// Only notifications from these sessions (comma-separated)
        #[arg(long, value_delimiter = ',')]
        from: Vec<String>,
        /// Only these notification kinds (comma-separated)
        #[arg(long, value_enum, value_delimiter = ',')]
        kind: Vec<sessions::NotificationKind>,
        #[arg(long, value_enum)]
        min_importance: Option<sessions::Importance>,
        /// Match affected project path prefixes (repeatable)
        #[arg(long = "path")]
        paths: Vec<String>,
        /// Match Git branches (comma-separated)
        #[arg(long, value_delimiter = ',')]
        branch: Vec<String>,
        /// Match routing metadata as KEY=VALUE (repeatable)
        #[arg(long = "metadata")]
        metadata: Vec<String>,
        #[arg(long)]
        created_after: Option<String>,
        #[arg(long)]
        created_before: Option<String>,
        #[arg(long)]
        max_age_hours: Option<u64>,
        /// Case-insensitive search across summary and body
        #[arg(long)]
        text: Option<String>,
        /// Override this session's initial lookback window
        #[arg(long)]
        lookback_hours: Option<u64>,
        /// Maximum results (defaults to sessions.poll_limit)
        #[arg(long)]
        limit: Option<usize>,
        /// Skip results in the selected order; use the returned continuation offset
        #[arg(long, default_value = "0")]
        offset: usize,
        /// Return oldest notifications first instead of the safe newest-first default
        #[arg(long, conflicts_with = "newest_first")]
        oldest_first: bool,
        /// Explicitly request the default newest-first order (retained for compatibility)
        #[arg(long, conflicts_with = "oldest_first")]
        newest_first: bool,
    },
    /// Read or dismiss one notification
    Notification {
        #[command(subcommand)]
        cmd: NotificationCmd,
    },
    /// Table of contents: every page with its description
    Toc,
    /// Exhaustive catalog of every page and pinned content
    Context,
    /// Bounded, task-aware map of standing instructions and relevant pages
    Prime {
        /// The task the agent is about to perform
        #[arg(long)]
        query: String,
        /// Maximum estimated tokens for the complete response
        #[arg(long)]
        tokens: Option<usize>,
        /// Separate ceiling for standing instructions
        #[arg(long)]
        instruction_tokens: Option<usize>,
        /// Maximum suggested pages
        #[arg(long)]
        limit: Option<usize>,
        /// Maximum suggested pages from any one section
        #[arg(long)]
        max_per_section: Option<usize>,
        /// Prior hash; unchanged state suppresses the section catalog, not instructions or suggestions
        #[arg(long)]
        since: Option<String>,
        /// Query/options/state hash returned with the previous cursor window
        #[arg(long)]
        context_hash: Option<String>,
        /// Continuation offset returned by a previous prime response
        #[arg(long, default_value = "0")]
        cursor: usize,
    },
    /// Print a page; --expand inlines summaries of linked pages
    Read {
        id: String,
        /// Inline linked-page summaries to this depth (default 1 when flag given)
        #[arg(long, num_args = 0..=1, default_missing_value = "1", value_name = "DEPTH", value_parser = parse_read_expand_depth)]
        expand: Option<usize>,
    },
    /// Create a page (body from stdin if piped; otherwise a stub)
    New {
        id: String,
        #[arg(long)]
        title: Option<String>,
        /// Comma-separated tags
        #[arg(long)]
        tags: Option<String>,
        /// One-line description shown in tocs (default: derived from the body)
        #[arg(long)]
        description: Option<String>,
        /// Comma-separated project paths this page documents (used by ingest)
        #[arg(long)]
        sources: Option<String>,
        /// Legacy instruction pin (requires a real body)
        #[arg(long, conflicts_with = "pin_level")]
        pin: bool,
        /// Pin as instructions, a summary, or a metadata-only discoverable reference
        #[arg(long, value_enum)]
        pin_level: Option<page::PinLevel>,
        /// Render the page body and defaults from a project-local protocol
        #[arg(long)]
        protocol: Option<String>,
    },
    /// Replace a page's body from stdin (clears stub status)
    Write {
        id: String,
        /// Append to the body instead of replacing it
        #[arg(long)]
        append: bool,
        /// Comma-separated project paths this page documents (used by ingest)
        #[arg(long)]
        sources: Option<String>,
        /// Set the legacy instruction pin
        #[arg(long, conflicts_with = "unpin")]
        pin: bool,
        /// Remove the pin
        #[arg(long, conflicts_with = "pin_level")]
        unpin: bool,
        /// Set instruction, summary, or metadata-only discoverable pin behavior
        #[arg(long, value_enum, conflicts_with_all = ["pin", "unpin"])]
        pin_level: Option<page::PinLevel>,
        /// Replace the one-line description
        #[arg(long)]
        description: Option<String>,
    },
    /// Delete a page
    Rm { id: String },
    /// Rename/move a page, rewriting all inbound wikilinks
    Mv { old: String, new: String },
    /// Create stubs for broken [[wikilinks]] and print a bounded fill-in worklist
    Expand {
        id: Option<String>,
        /// Maximum returned IDs per worklist category (default: retrieval.search_limit)
        #[arg(long, conflicts_with = "all")]
        limit: Option<usize>,
        /// Maximum estimated response tokens (default: retrieval.search_tokens)
        #[arg(long, conflicts_with = "all")]
        tokens: Option<usize>,
        /// Return the exhaustive worklist after creating every eligible stub
        #[arg(long, conflicts_with_all = ["limit", "tokens"])]
        all: bool,
    },
    /// Ingest the codebase: seed module stubs and emit a documentation
    /// worklist; on later runs, map code changes to stale pages
    Ingest {
        /// How thorough the documentation pass should be
        #[arg(long, value_enum, default_value = "standard")]
        level: commands::IngestLevel,
        /// Record the current project commit as the wiki's sync point
        #[arg(long, visible_alias = "mark-reconciled")]
        mark: bool,
        /// Recover an ambiguous reconciliation metadata commit
        #[arg(long, value_enum)]
        recover: Option<commands::IngestRecoveryAction>,
        /// Receipt emitted by the exact ingest worklist being reconciled
        #[arg(long, value_name = "SHA256")]
        expect_worklist: Option<String>,
        /// Force a fresh ingest even if a sync point exists
        #[arg(long)]
        full: bool,
        /// Diff against this commit instead of the recorded sync point
        #[arg(long)]
        since: Option<String>,
        /// Project checkout to reconcile instead of the registered root
        #[arg(long, alias = "worktree")]
        project_root: Option<PathBuf>,
        /// Maximum items shown per worklist category
        #[arg(long)]
        limit: Option<usize>,
        /// Approximate hard response budget (default: publish.output_tokens)
        #[arg(long)]
        tokens: Option<usize>,
        /// Show the exhaustive worklist (may be large)
        #[arg(long, conflicts_with_all = ["limit", "tokens"])]
        all: bool,
    },
    /// Search pages with deterministic ranking and bounded excerpts
    Search {
        query: String,
        /// Only pages carrying this tag
        #[arg(long)]
        tag: Option<String>,
        /// Maximum results (configured default: retrieval.search_limit)
        #[arg(long)]
        limit: Option<usize>,
        /// Maximum estimated tokens for the complete response
        #[arg(long)]
        tokens: Option<usize>,
        /// Matching body lines per result
        #[arg(long)]
        excerpt_lines: Option<usize>,
        /// Continue at this ranked-result offset
        #[arg(long, default_value = "0")]
        cursor: usize,
        /// Retrieval-state hash returned with the previous cursor window
        #[arg(long)]
        context_hash: Option<String>,
        /// Treat the query as a regular expression
        #[arg(long)]
        regex: bool,
        /// Return every matching page with at most five matching body lines per page
        #[arg(long)]
        all: bool,
    },
    /// Outlinks and backlinks of a page
    Links { id: String },
    /// Emit a critique briefing: check the current changes against every
    /// rules section (the invoking agent executes it)
    Critique {
        /// Only this rules section
        #[arg(long)]
        section: Option<String>,
        /// Critique changes since this ref instead of uncommitted changes
        #[arg(long)]
        since: Option<String>,
        /// Critique staged changes
        #[arg(long, conflicts_with = "since")]
        staged: bool,
        /// Project checkout to inspect instead of the resolved wiki root
        #[arg(long, alias = "worktree")]
        project_root: Option<PathBuf>,
        /// Review this exact commit/tree instead of the checkout's dirty state
        #[arg(long, conflicts_with = "staged")]
        revision: Option<String>,
        /// Critique explicit paths instead of a git diff
        #[arg(long, num_args = 1..)]
        paths: Vec<String>,
        /// Maximum estimated tokens for the compact briefing
        #[arg(long)]
        tokens: Option<usize>,
        /// Explicitly include complete checks and rule bodies
        #[arg(long, conflicts_with = "tokens")]
        all: bool,
    },
    /// Temporarily unlock a locked section (requires explicit user permission)
    Unlock {
        section: String,
        /// Minutes until it relocks automatically
        #[arg(long, default_value = "15")]
        minutes: u64,
    },
    /// Relock a section immediately
    Lock { section: String },
    /// Health check: broken links, orphans, stubs, missing summaries
    Doctor {
        /// Mechanically repair frontmatter issues
        #[arg(long)]
        fix: bool,
        /// Exit non-zero if any error-severity diagnostics remain (for CI)
        #[arg(long)]
        strict: bool,
        /// Project checkout used for source provenance and staleness
        #[arg(long, alias = "worktree")]
        project_root: Option<PathBuf>,
        /// Audit source provenance at this exact Git revision
        #[arg(long)]
        revision: Option<String>,
    },
    /// Compact operator dashboard for wiki health
    Status {
        #[arg(long, alias = "worktree")]
        project_root: Option<PathBuf>,
        #[arg(long)]
        revision: Option<String>,
        /// Exit non-zero when error diagnostics exist
        #[arg(long)]
        strict: bool,
    },
    /// Manage project-local Markdown page protocols
    Protocol {
        #[command(subcommand)]
        cmd: ProtocolCmd,
    },
    /// Validate and atomically apply a multi-page change manifest
    Publish {
        /// TOML or JSON change manifest; omit to read stdin
        manifest: Option<PathBuf>,
        /// Validate and show the exact plan without changing files (default)
        #[arg(long, conflicts_with = "apply")]
        check: bool,
        /// Apply the validated plan as one history unit
        #[arg(long)]
        apply: bool,
        /// Confirms explicit permission for changes inside locked rules sections
        #[arg(long)]
        user_approved: bool,
        /// Apply only if the current plan matches a token returned by --check
        #[arg(long, requires = "apply", conflicts_with = "recover")]
        expect_plan: Option<String>,
        /// Maximum estimated response tokens for a check (default: configured publish.output_tokens)
        #[arg(long, conflicts_with_all = ["apply", "recover", "full_diff"])]
        tokens: Option<usize>,
        /// Return exact before/after page images instead of the bounded compact preview
        #[arg(long, conflicts_with_all = ["apply", "recover", "tokens"])]
        full_diff: bool,
        /// Recover an interrupted publish journal
        #[arg(long, value_enum, conflicts_with_all = ["check", "apply"])]
        recover: Option<PublishRecoveryArg>,
        /// Allow recovery to remove a demonstrably stale publish lock
        #[arg(long, requires = "recover")]
        force_stale_lock: bool,
    },
    /// Rule change proposals backed by checked publish manifests
    Rules {
        #[command(subcommand)]
        cmd: RulesCmd,
    },
    /// Show or edit the wiki's registered project roots
    Roots {
        /// Register an additional project root
        #[arg(long)]
        add: Option<PathBuf>,
        /// Remove a project root
        #[arg(long)]
        remove: Option<PathBuf>,
    },
    /// Permanently delete a wiki (requires --force)
    RemoveWiki {
        slug: String,
        #[arg(long)]
        force: bool,
    },
    /// Rename a wiki's slug
    RenameWiki { old: String, new: String },
    /// Open the wiki as an Obsidian vault
    Obsidian {
        /// Print the obsidian:// URI instead of launching Obsidian
        #[arg(long)]
        print: bool,
    },
    /// Install agent integrations
    Plugin {
        #[command(subcommand)]
        cmd: PluginCmd,
    },
    /// Inspect or edit global defaults and per-wiki overrides
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// Run the MCP server over stdio
    Serve,
}

#[derive(Subcommand)]
enum ProtocolCmd {
    /// List available namespaced protocols
    List,
    /// Show one protocol's metadata and Markdown template
    Show { name: String },
    /// Create or replace a protocol from stdin after validating it
    Write { name: String },
    /// Remove a protocol
    Remove { name: String },
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum PublishRecoveryArg {
    Rollback,
    Accept,
}

#[derive(Subcommand)]
enum RulesCmd {
    /// Store and preflight a publish manifest as a rule-change proposal
    Propose {
        manifest: Option<PathBuf>,
        /// Maximum estimated response tokens (default: configured publish.output_tokens)
        #[arg(long, conflicts_with = "full_diff")]
        tokens: Option<usize>,
        /// Return exact before/after page images instead of the bounded compact preview
        #[arg(long, conflicts_with = "tokens")]
        full_diff: bool,
    },
    /// Re-run preflight for a stored proposal
    Review {
        id: String,
        /// Maximum estimated response tokens (default: configured publish.output_tokens)
        #[arg(long, conflicts_with = "full_diff")]
        tokens: Option<usize>,
        /// Return exact before/after page images instead of the bounded compact preview
        #[arg(long, conflicts_with = "tokens")]
        full_diff: bool,
    },
    /// Apply a reviewed proposal, then relock every affected rules section
    Apply {
        id: String,
        /// Required after explicit user approval in this conversation
        #[arg(long)]
        user_approved: bool,
    },
}

#[derive(Subcommand)]
enum SessionCmd {
    /// Start a named coordination session
    Start {
        /// Agent host/type, such as codex or claude
        #[arg(long)]
        agent: Option<String>,
        /// Optional human-readable purpose
        #[arg(long)]
        label: Option<String>,
        /// Include notifications this many hours before session creation
        #[arg(long)]
        lookback_hours: Option<u64>,
        /// Disable automatic debounced activity heartbeats for this session
        #[arg(long)]
        no_heartbeat: bool,
        /// Print only the generated id (convenient for WOOKIE_SESSION)
        #[arg(long)]
        id_only: bool,
    },
    /// List known sessions
    List {
        #[arg(long, value_delimiter = ',')]
        status: Vec<String>,
        #[arg(long, value_delimiter = ',')]
        agent: Vec<String>,
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        created_after: Option<String>,
        #[arg(long)]
        active_after: Option<String>,
        /// Only sessions inactive past sessions.stale_after_minutes
        #[arg(long, conflicts_with = "active")]
        stale: bool,
        /// Only sessions active within sessions.stale_after_minutes
        #[arg(long)]
        active: bool,
        #[arg(long)]
        limit: Option<usize>,
        /// Continue from the offset returned by a previous session list
        #[arg(long, default_value = "0")]
        cursor: usize,
        #[arg(long)]
        oldest_first: bool,
    },
    /// Show one session and a bounded summary of its newest notifications
    Show {
        id: String,
        /// Maximum notification summaries to return (default 20; maximum 1000)
        #[arg(long)]
        limit: Option<usize>,
        /// Continue from the offset returned by a previous session show
        #[arg(long, default_value = "0")]
        cursor: usize,
    },
    /// Record activity for a long-running session
    Heartbeat {
        /// Session id; defaults to WOOKIE_SESSION
        id: Option<String>,
        /// Bypass the configured activity debounce
        #[arg(long)]
        force: bool,
    },
    /// Mark one session closed
    Close {
        /// Session id; defaults to WOOKIE_SESSION
        id: Option<String>,
    },
    /// Remove old sessions (dry-run unless --apply is passed)
    Prune {
        /// Minimum inactivity age; defaults to configured retention or 30 days
        #[arg(long)]
        older_than_days: Option<u64>,
        /// RFC3339 activity cutoff
        #[arg(long)]
        inactive_before: Option<String>,
        /// Always preserve this many newest sessions
        #[arg(long, default_value = "0")]
        keep_latest: usize,
        /// Permit pruning inactive active sessions as well as closed sessions
        #[arg(long)]
        include_active: bool,
        /// Perform deletion; omission previews the exact session ids
        #[arg(long)]
        apply: bool,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Show stored configuration, or resolved values with --effective
    Show {
        #[arg(long)]
        global: bool,
        #[arg(long)]
        effective: bool,
    },
    /// Read one dotted configuration key
    Get {
        key: String,
        #[arg(long)]
        global: bool,
        #[arg(long)]
        effective: bool,
    },
    /// Set one dotted TOML value
    Set {
        key: String,
        value: String,
        #[arg(long)]
        global: bool,
        /// Store the value literally instead of parsing TOML
        #[arg(long)]
        string: bool,
        /// Required for sections.* after explicit user approval
        #[arg(long)]
        user_approved: bool,
    },
    /// Remove one override so it inherits its default
    Unset {
        key: String,
        #[arg(long)]
        global: bool,
        /// Required for sections.* after explicit user approval
        #[arg(long)]
        user_approved: bool,
    },
    /// List supported dotted keys
    Keys {
        #[arg(long)]
        global: bool,
    },
}

#[derive(Subcommand)]
enum NotificationCmd {
    /// Read the full notification and mark it read for this session
    Read {
        id: String,
        #[arg(long)]
        session: Option<String>,
    },
    /// Mark an irrelevant notification dismissed without reading its body
    Dismiss {
        id: String,
        #[arg(long)]
        session: Option<String>,
    },
}

#[derive(Subcommand)]
enum PluginCmd {
    /// Install the integration for an agent (claude or codex)
    Install {
        #[arg(value_enum)]
        target: plugins::Target,
    },
    /// Check whether installed agent guidance matches this wookie version
    Status {
        #[arg(value_enum)]
        target: Option<plugins::Target>,
        /// Exit non-zero when an integration is stale or missing
        #[arg(long)]
        strict: bool,
    },
}

fn split_csv(v: Option<String>) -> Option<Vec<String>> {
    v.map(|t| {
        t.split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    })
}

fn stdin_body(max_bytes: usize) -> Result<Option<String>> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return Ok(None);
    }
    let mut buf = String::new();
    let read_limit = u64::try_from(max_bytes)
        .unwrap_or(u64::MAX - 1)
        .saturating_add(1);
    stdin.lock().take(read_limit).read_to_string(&mut buf)?;
    if buf.len() > max_bytes {
        return Err(anyhow!("stdin body exceeds the {max_bytes}-byte limit"));
    }
    if buf.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(buf))
    }
}

fn read_input_file_or_stdin(path: Option<&std::path::Path>, max_bytes: usize) -> Result<String> {
    if let Some(path) = path {
        let path_metadata = std::fs::symlink_metadata(path)
            .with_context(|| format!("inspecting input {}", path.display()))?;
        if path_metadata.file_type().is_symlink() || !path_metadata.is_file() {
            return Err(anyhow!(
                "input {} must be a regular file, not a symlink, directory, or stream",
                path.display()
            ));
        }
        let mut file = std::fs::File::open(path)
            .with_context(|| format!("opening input {}", path.display()))?;
        if !file.metadata()?.is_file() {
            return Err(anyhow!(
                "input {} must remain a regular file",
                path.display()
            ));
        }
        let max_plus_one = u64::try_from(max_bytes)
            .unwrap_or(u64::MAX)
            .saturating_add(1);
        let mut bytes = Vec::with_capacity(max_bytes.min(64 * 1024));
        (&mut file)
            .take(max_plus_one)
            .read_to_end(&mut bytes)
            .with_context(|| format!("reading input {}", path.display()))?;
        if bytes.len() > max_bytes {
            return Err(anyhow!(
                "input {} exceeds the {max_bytes}-byte limit",
                path.display()
            ));
        }
        return String::from_utf8(bytes)
            .with_context(|| format!("input {} is not UTF-8", path.display()));
    }
    stdin_body(max_bytes)?.ok_or_else(|| anyhow!("missing input: pass a file or pipe it on stdin"))
}

fn session_id(explicit: Option<String>) -> Result<String> {
    explicit
        .or_else(|| std::env::var("WOOKIE_SESSION").ok())
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| {
            anyhow!(
                "missing session id — pass --session <id> or set WOOKIE_SESSION after `wookie session start`"
            )
        })
}

fn metadata_pairs(values: Vec<String>) -> Result<BTreeMap<String, String>> {
    let mut metadata = BTreeMap::new();
    for value in values {
        let (key, value) = value
            .split_once('=')
            .ok_or_else(|| anyhow!("metadata must use KEY=VALUE syntax: '{value}'"))?;
        if metadata
            .insert(key.to_string(), value.to_string())
            .is_some()
        {
            return Err(anyhow!("duplicate metadata key '{key}'"));
        }
    }
    Ok(metadata)
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

fn configured_kind(value: &str) -> Result<sessions::NotificationKind> {
    Ok(match value {
        "code-change" => sessions::NotificationKind::CodeChange,
        "decision" => sessions::NotificationKind::Decision,
        "blocker" => sessions::NotificationKind::Blocker,
        "handoff" => sessions::NotificationKind::Handoff,
        "warning" => sessions::NotificationKind::Warning,
        "note" => sessions::NotificationKind::Note,
        _ => return Err(anyhow!("invalid configured notification kind '{value}'")),
    })
}

fn configured_importance(value: &str) -> Result<sessions::Importance> {
    Ok(match value {
        "low" => sessions::Importance::Low,
        "normal" => sessions::Importance::Normal,
        "high" => sessions::Importance::High,
        _ => {
            return Err(anyhow!(
                "invalid configured notification importance '{value}'"
            ))
        }
    })
}

fn ensure_coordination_enabled(w: &wiki::Wiki) -> Result<()> {
    if !w.sessions.enabled {
        return Err(anyhow!(
            "session coordination is disabled for wiki '{}' (sessions.enabled=false)",
            w.slug
        ));
    }
    Ok(())
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

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let home = config::wookie_home()?;
    let cwd = std::env::current_dir()?;
    let json = cli.json;
    let resolve = || wiki::resolve(&home, cli.wiki.as_deref(), &cwd);

    let out = match cli.cmd {
        Cmd::Init {
            slug,
            project,
            description,
        } => commands::init(&home, &cwd, slug, project, description, json)?,
        Cmd::List => commands::list(&home, json)?,
        Cmd::Session { cmd } => {
            let w = resolve()?;
            ensure_coordination_enabled(&w)?;
            match cmd {
                SessionCmd::Start {
                    agent,
                    label,
                    lookback_hours,
                    no_heartbeat,
                    id_only,
                } => {
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
                    let lookback = lookback_hours.unwrap_or(w.sessions.initial_lookback_hours);
                    if lookback > config::MAX_SESSION_LOOKBACK_HOURS {
                        return Err(anyhow!(
                            "session lookback is too large; it must not exceed {} hours",
                            config::MAX_SESSION_LOOKBACK_HOURS
                        ));
                    }
                    let session = sessions::start_with_options(
                        &w,
                        sessions::StartOptions {
                            agent,
                            label,
                            notification_lookback_seconds: hours_to_seconds(lookback)?,
                            activity_debounce_seconds: w.sessions.activity_debounce_seconds,
                            heartbeat_on_activity: w.sessions.heartbeat_on_activity
                                && !no_heartbeat,
                            max_agent_bytes: w.sessions.max_agent_bytes,
                            max_label_bytes: w.sessions.max_label_bytes,
                        },
                    )?;
                    if json {
                        serde_json::to_string(&session)?
                    } else if id_only {
                        session.id
                    } else {
                        format!(
                            "Started session '{}'. Set WOOKIE_SESSION to reuse this id.",
                            session.id
                        )
                    }
                }
                SessionCmd::List {
                    status,
                    agent,
                    label,
                    created_after,
                    active_after,
                    stale,
                    active,
                    limit,
                    cursor,
                    oldest_first,
                } => {
                    let stale_minutes = i64::try_from(w.sessions.stale_after_minutes)
                        .map_err(|_| anyhow!("sessions.stale_after_minutes is too large"))?;
                    let stale_duration = chrono::Duration::try_minutes(stale_minutes)
                        .ok_or_else(|| anyhow!("sessions.stale_after_minutes is too large"))?;
                    let cutoff = chrono::Utc::now()
                        .checked_sub_signed(stale_duration)
                        .ok_or_else(|| anyhow!("stale-session cutoff is out of range"))?
                        .to_rfc3339();
                    let result = sessions::list_with_options(
                        &w,
                        &sessions::SessionListRequest {
                            statuses: status,
                            agents: agent,
                            label_contains: label,
                            created_after,
                            active_after: if active {
                                Some(cutoff.clone())
                            } else {
                                active_after
                            },
                            active_before: stale.then_some(cutoff),
                            limit,
                            cursor,
                            newest_first: !oldest_first,
                        },
                    )?;
                    sessions::format_session_list(&result, json)?
                }
                SessionCmd::Show { id, limit, cursor } => sessions::show_with_options(
                    &w,
                    &id,
                    &sessions::SessionShowRequest { limit, cursor },
                    json,
                )?,
                SessionCmd::Heartbeat { id, force } => {
                    let id = session_id(id)?;
                    let session = sessions::heartbeat(&w, &id, force)?;
                    if json {
                        serde_json::to_string(&session)?
                    } else {
                        format!("Recorded heartbeat for session '{}'.", session.id)
                    }
                }
                SessionCmd::Close { id } => sessions::close(&w, &session_id(id)?, json)?,
                SessionCmd::Prune {
                    older_than_days,
                    inactive_before,
                    keep_latest,
                    include_active,
                    apply,
                } => {
                    let older_than_seconds = if let Some(days) = older_than_days {
                        Some(days_to_seconds(days)?)
                    } else if inactive_before.is_some() {
                        None
                    } else {
                        Some(days_to_seconds(w.sessions.retention_days.unwrap_or(30))?)
                    };
                    let result = sessions::prune_sessions(
                        &w,
                        &sessions::PruneRequest {
                            closed_only: !include_active,
                            older_than_seconds,
                            inactive_before,
                            keep_latest,
                            dry_run: !apply,
                        },
                    )?;
                    sessions::format_prune(&result, json)?
                }
            }
        }
        Cmd::Notify {
            session,
            summary,
            kind,
            importance,
            paths,
            targets,
            idempotency_key,
            metadata,
            no_git_context,
        } => {
            let w = resolve()?;
            ensure_coordination_enabled(&w)?;
            let session = session_id(session)?;
            let notification = sessions::notify_with_request(
                &w,
                sessions::NotifyRequest {
                    source_session: session.clone(),
                    summary,
                    kind: kind.unwrap_or(configured_kind(&w.sessions.default_kind)?),
                    importance: importance
                        .unwrap_or(configured_importance(&w.sessions.default_importance)?),
                    paths: split_csv(paths).unwrap_or_default(),
                    body: stdin_body(w.sessions.max_body_bytes)?,
                    targets: split_csv(targets).unwrap_or_default(),
                    idempotency_key,
                    git: (w.sessions.include_git_context && !no_git_context)
                        .then(|| sessions::capture_git_context(&w, &cwd))
                        .flatten(),
                    metadata: metadata_pairs(metadata)?,
                    limits: notification_limits(&w.sessions),
                },
            )?;
            if json {
                serde_json::json!({"notification": notification.meta}).to_string()
            } else {
                format!(
                    "Published notification '{}' from session '{}'.",
                    notification.meta.id, session
                )
            }
        }
        Cmd::Notifications {
            session,
            all,
            from,
            kind,
            min_importance,
            paths,
            branch,
            metadata,
            created_after,
            created_before,
            max_age_hours,
            text,
            lookback_hours,
            limit,
            offset,
            oldest_first,
            newest_first,
        } => {
            let w = resolve()?;
            ensure_coordination_enabled(&w)?;
            if lookback_hours.is_some_and(|hours| hours > config::MAX_SESSION_LOOKBACK_HOURS) {
                return Err(anyhow!(
                    "notification lookback is too large; it must not exceed {} hours",
                    config::MAX_SESSION_LOOKBACK_HOURS
                ));
            }
            let limit = limit.unwrap_or(w.sessions.poll_limit);
            if !(1..=w.sessions.poll_limit).contains(&limit) {
                return Err(anyhow!(
                    "requested limit {limit} must be between 1 and configured sessions.poll_limit {}",
                    w.sessions.poll_limit
                ));
            }
            let result = sessions::inbox_with_request(
                &w,
                &sessions::InboxRequest {
                    session_id: session_id(session)?,
                    include_acknowledged: all,
                    lookback_seconds: lookback_hours.map(hours_to_seconds).transpose()?,
                    filter: sessions::NotificationFilter {
                        source_sessions: from,
                        kinds: kind,
                        min_importance,
                        path_prefixes: paths,
                        branches: branch,
                        metadata: metadata_pairs(metadata)?,
                        created_after,
                        created_before,
                        max_age_seconds: max_age_hours.map(hours_to_seconds).transpose()?,
                        text,
                    },
                    limit: Some(limit),
                    offset,
                    newest_first: newest_first || !oldest_first,
                },
            )?;
            sessions::format_inbox(&result, json)?
        }
        Cmd::Notification { cmd } => match cmd {
            NotificationCmd::Read { id, session } => {
                let w = resolve()?;
                ensure_coordination_enabled(&w)?;
                sessions::read_notification(&w, &session_id(session)?, &id, json)?
            }
            NotificationCmd::Dismiss { id, session } => {
                let w = resolve()?;
                ensure_coordination_enabled(&w)?;
                sessions::dismiss_notification(&w, &session_id(session)?, &id, json)?
            }
        },
        Cmd::Toc => commands::toc(&resolve()?, json)?,
        Cmd::Context => commands::context(&resolve()?, json)?,
        Cmd::Prime {
            query,
            tokens,
            instruction_tokens,
            limit,
            max_per_section,
            since,
            context_hash,
            cursor,
        } => commands::prime(
            &resolve()?,
            &commands::PrimeOptions {
                query,
                tokens,
                instruction_tokens,
                limit,
                max_per_section,
                since,
                context_hash,
                cursor,
                cwd: Some(cwd.clone()),
            },
            json,
        )?,
        Cmd::Read { id, expand } => commands::read(&resolve()?, &id, expand.unwrap_or(0), json)?,
        Cmd::New {
            id,
            title,
            tags,
            description,
            sources,
            pin,
            pin_level,
            protocol,
        } => commands::new_page(
            &resolve()?,
            &id,
            title,
            description,
            split_csv(tags).unwrap_or_default(),
            split_csv(sources).unwrap_or_default(),
            pin_level.or_else(|| pin.then_some(page::PinLevel::Instruction)),
            protocol.as_deref(),
            stdin_body(MAX_PAGE_STDIN_BYTES)?,
            json,
        )?,
        Cmd::Write {
            id,
            append,
            sources,
            pin,
            unpin,
            pin_level,
            description,
        } => {
            let body = stdin_body(MAX_PAGE_STDIN_BYTES)?.unwrap_or_default();
            let pin = if let Some(level) = pin_level {
                commands::PinChange::Set(level)
            } else if pin {
                commands::PinChange::Set(page::PinLevel::Instruction)
            } else if unpin {
                commands::PinChange::Clear
            } else {
                commands::PinChange::Keep
            };
            commands::write(
                &resolve()?,
                &id,
                &body,
                append,
                split_csv(sources),
                pin,
                description,
                json,
            )?
        }
        Cmd::Rm { id } => commands::rm(&resolve()?, &id, json)?,
        Cmd::Mv { old, new } => commands::mv(&resolve()?, &old, &new, json)?,
        Cmd::Expand {
            id,
            limit,
            tokens,
            all,
        } => commands::expand(
            &resolve()?,
            &commands::ExpandOptions {
                id: id.as_deref(),
                limit,
                tokens,
                all,
            },
            json,
        )?,
        Cmd::Ingest {
            level,
            mark,
            recover,
            expect_worklist,
            full,
            since,
            project_root,
            limit,
            tokens,
            all,
        } => {
            let mut w = resolve()?;
            commands::ingest(
                &mut w,
                project_root.as_deref().unwrap_or(&cwd),
                &commands::IngestOptions {
                    project_root: project_root.as_deref(),
                    level,
                    mark,
                    recover,
                    expect_worklist: expect_worklist.as_deref(),
                    full,
                    since: since.as_deref(),
                    limit,
                    tokens,
                    all,
                    json,
                },
            )?
        }
        Cmd::Search {
            query,
            tag,
            limit,
            tokens,
            excerpt_lines,
            cursor,
            context_hash,
            regex,
            all,
        } => commands::search_with_options(
            &resolve()?,
            &commands::SearchOptions {
                query,
                tag,
                limit,
                tokens,
                excerpt_lines,
                cursor,
                context_hash,
                regex,
                all,
                cwd: Some(cwd.clone()),
            },
            json,
        )?,
        Cmd::Links { id } => commands::links(&resolve()?, &id, json)?,
        Cmd::Critique {
            section,
            since,
            staged,
            project_root,
            revision,
            paths,
            tokens,
            all,
        } => commands::critique(
            &resolve()?,
            &cwd,
            &commands::CritiqueOptions {
                project_root: project_root.as_deref(),
                revision: revision.as_deref(),
                section: section.as_deref(),
                since: since.as_deref(),
                staged,
                paths: &paths,
                tokens,
                all,
                json,
            },
        )?,
        Cmd::Unlock { section, minutes } => commands::unlock(&resolve()?, &section, minutes, json)?,
        Cmd::Lock { section } => commands::lock(&resolve()?, &section, json)?,
        Cmd::Doctor {
            fix,
            strict,
            project_root,
            revision,
        } => {
            let (report, errors) = commands::doctor_with_options(
                &resolve()?,
                fix,
                &audit::AuditOptions {
                    project_root,
                    project_revision: revision,
                },
                json,
            )?;
            if strict && errors > 0 {
                println!("{report}");
                std::process::exit(1);
            }
            report
        }
        Cmd::Status {
            project_root,
            revision,
            strict,
        } => {
            let (report, errors) = commands::status(
                &resolve()?,
                &audit::AuditOptions {
                    project_root,
                    project_revision: revision,
                },
                json,
            )?;
            if strict && errors > 0 {
                println!("{report}");
                std::process::exit(1);
            }
            report
        }
        Cmd::Protocol { cmd } => match cmd {
            ProtocolCmd::List => commands::protocol_list(&resolve()?, json)?,
            ProtocolCmd::Show { name } => commands::protocol_show(&resolve()?, &name, json)?,
            ProtocolCmd::Write { name } => commands::protocol_write(
                &resolve()?,
                &name,
                &stdin_body(MAX_PAGE_STDIN_BYTES)?.unwrap_or_default(),
                json,
            )?,
            ProtocolCmd::Remove { name } => commands::protocol_remove(&resolve()?, &name, json)?,
        },
        Cmd::Publish {
            manifest,
            check: _,
            apply,
            user_approved,
            expect_plan,
            tokens,
            full_diff,
            recover,
            force_stale_lock,
        } => {
            let w = resolve()?;
            if let Some(action) = recover {
                commands::publish_recover(
                    &w,
                    match action {
                        PublishRecoveryArg::Rollback => publish::RecoveryAction::Rollback,
                        PublishRecoveryArg::Accept => publish::RecoveryAction::Accept,
                    },
                    force_stale_lock,
                    json,
                )?
            } else {
                let raw = read_input_file_or_stdin(manifest.as_deref(), MAX_PAGE_STDIN_BYTES)?;
                commands::publish_changes(
                    &w,
                    &raw,
                    apply,
                    user_approved,
                    expect_plan.as_deref(),
                    &commands::PublishOutputOptions { tokens, full_diff },
                    json,
                )?
            }
        }
        Cmd::Rules { cmd } => match cmd {
            RulesCmd::Propose {
                manifest,
                tokens,
                full_diff,
            } => commands::rules_propose(
                &resolve()?,
                &read_input_file_or_stdin(manifest.as_deref(), MAX_PAGE_STDIN_BYTES)?,
                &commands::PublishOutputOptions { tokens, full_diff },
                json,
            )?,
            RulesCmd::Review {
                id,
                tokens,
                full_diff,
            } => commands::rules_review(
                &resolve()?,
                &id,
                &commands::PublishOutputOptions { tokens, full_diff },
                json,
            )?,
            RulesCmd::Apply { id, user_approved } => {
                commands::rules_apply(&resolve()?, &id, user_approved, json)?
            }
        },
        Cmd::Roots { add, remove } => {
            let mut w = resolve()?;
            commands::roots(&mut w, add, remove, json)?
        }
        Cmd::RemoveWiki { slug, force } => commands::remove_wiki(&home, &slug, force, json)?,
        Cmd::RenameWiki { old, new } => commands::rename_wiki(&home, &old, &new, json)?,
        Cmd::Obsidian { print } => commands::obsidian(&resolve()?, print, json)?,
        Cmd::Plugin { cmd } => match cmd {
            PluginCmd::Install { target } => plugins::install(target, json)?,
            PluginCmd::Status { target, strict } => plugins::status(target, strict, json)?,
        },
        Cmd::Config { cmd } => match cmd {
            ConfigCmd::Show { global, effective } => {
                if global {
                    if effective {
                        return Err(anyhow!(
                            "--effective applies to a resolved wiki, not global defaults"
                        ));
                    }
                    settings::show_global(&home, json)?
                } else {
                    settings::show_wiki(&resolve()?, effective, json)?
                }
            }
            ConfigCmd::Get {
                key,
                global,
                effective,
            } => {
                if global {
                    if effective {
                        return Err(anyhow!(
                            "--effective applies to a resolved wiki, not global defaults"
                        ));
                    }
                    settings::get_global(&home, &key, json)?
                } else {
                    settings::get_wiki(&resolve()?, &key, effective, json)?
                }
            }
            ConfigCmd::Set {
                key,
                value,
                global,
                string,
                user_approved,
            } => {
                if global {
                    if user_approved {
                        return Err(anyhow!(
                            "--user-approved is only meaningful for per-wiki sections.* settings"
                        ));
                    }
                    settings::set_global(&home, &key, &value, string, json)?
                } else {
                    let mut w = resolve()?;
                    settings::set_wiki(&mut w, &key, &value, string, user_approved, json)?
                }
            }
            ConfigCmd::Unset {
                key,
                global,
                user_approved,
            } => {
                if global {
                    if user_approved {
                        return Err(anyhow!(
                            "--user-approved is only meaningful for per-wiki sections.* settings"
                        ));
                    }
                    settings::unset_global(&home, &key, json)?
                } else {
                    let mut w = resolve()?;
                    settings::unset_wiki(&mut w, &key, user_approved, json)?
                }
            }
            ConfigCmd::Keys { global } => settings::keys_output(global, json),
        },
        Cmd::Serve => {
            mcp::serve()?;
            String::new()
        }
    };

    if !out.is_empty() {
        println!("{out}");
    }
    Ok(())
}

#[cfg(test)]
mod input_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn fixture_path(name: &str) -> PathBuf {
        let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "wookie-input-{name}-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn read_expand_parser_rejects_unbounded_depths() {
        assert_eq!(parse_read_expand_depth("0").unwrap(), 0);
        assert_eq!(
            parse_read_expand_depth(&commands::MAX_READ_EXPAND_DEPTH.to_string()).unwrap(),
            commands::MAX_READ_EXPAND_DEPTH
        );
        assert!(
            parse_read_expand_depth(&(commands::MAX_READ_EXPAND_DEPTH + 1).to_string()).is_err()
        );
        assert!(parse_read_expand_depth(&u64::MAX.to_string()).is_err());
    }

    #[test]
    fn bounded_manifest_reader_rejects_non_regular_and_oversized_inputs() {
        let dir = fixture_path("bounded");
        let input = dir.join("manifest.toml");
        std::fs::write(&input, b"12345").unwrap();
        assert!(read_input_file_or_stdin(Some(&input), 4).is_err());
        assert!(read_input_file_or_stdin(Some(&dir), 16).is_err());
        std::fs::write(&input, b"ok").unwrap();
        assert_eq!(read_input_file_or_stdin(Some(&input), 4).unwrap(), "ok");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn bounded_manifest_reader_rejects_symlinks_and_fifos() {
        use std::os::unix::fs::symlink;

        let dir = fixture_path("special");
        let target = dir.join("target.toml");
        let link = dir.join("link.toml");
        std::fs::write(&target, b"safe").unwrap();
        symlink(&target, &link).unwrap();
        assert!(read_input_file_or_stdin(Some(&link), 16).is_err());

        let fifo = dir.join("manifest.fifo");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(status.success());
        assert!(read_input_file_or_stdin(Some(&fifo), 16).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
