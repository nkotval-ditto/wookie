mod commands;
mod config;
mod mcp;
mod page;
mod plugins;
mod wiki;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::io::{IsTerminal, Read};
use std::path::PathBuf;

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
    /// Table of contents: every page with its description
    Toc,
    /// Compact digest of the wiki for priming an agent
    Context,
    /// Print a page; --expand inlines summaries of linked pages
    Read {
        id: String,
        /// Inline linked-page summaries to this depth (default 1 when flag given)
        #[arg(long, num_args = 0..=1, default_missing_value = "1", value_name = "DEPTH")]
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
        /// Comma-separated project paths this page documents (used by ingest)
        #[arg(long)]
        sources: Option<String>,
        /// Pin: always-on instructions, inlined in full by `wookie context`
        #[arg(long)]
        pin: bool,
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
        /// Pin the page (always-on instructions, inlined by `wookie context`)
        #[arg(long, conflicts_with = "unpin")]
        pin: bool,
        /// Remove the pin
        #[arg(long)]
        unpin: bool,
    },
    /// Delete a page
    Rm { id: String },
    /// Rename/move a page, rewriting all inbound wikilinks
    Mv { old: String, new: String },
    /// Create stubs for broken [[wikilinks]] and print the fill-in worklist
    Expand { id: Option<String> },
    /// Ingest the codebase: seed module stubs and emit a documentation
    /// worklist; on later runs, map code changes to stale pages
    Ingest {
        /// How thorough the documentation pass should be
        #[arg(long, value_enum, default_value = "standard")]
        level: commands::IngestLevel,
        /// Record the current project commit as the wiki's sync point
        #[arg(long)]
        mark: bool,
        /// Force a fresh ingest even if a sync point exists
        #[arg(long)]
        full: bool,
        /// Diff against this commit instead of the recorded sync point
        #[arg(long)]
        since: Option<String>,
    },
    /// Search ids, titles, tags and bodies (case-insensitive regex)
    Search {
        query: String,
        /// Only pages carrying this tag
        #[arg(long)]
        tag: Option<String>,
    },
    /// Outlinks and backlinks of a page
    Links { id: String },
    /// Health check: broken links, orphans, stubs, missing summaries
    Doctor {
        /// Mechanically repair frontmatter issues
        #[arg(long)]
        fix: bool,
    },
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
    /// Run the MCP server over stdio
    Serve,
}

#[derive(Subcommand)]
enum PluginCmd {
    /// Install the integration for an agent (claude or codex)
    Install {
        #[arg(value_enum)]
        target: plugins::Target,
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

fn stdin_body() -> Option<String> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return None;
    }
    let mut buf = String::new();
    stdin.lock().read_to_string(&mut buf).ok()?;
    if buf.trim().is_empty() {
        None
    } else {
        Some(buf)
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
    let home = config::wookie_home();
    let cwd = std::env::current_dir()?;
    let json = cli.json;
    let resolve = || wiki::resolve(&home, cli.wiki.as_deref(), &cwd);

    let out = match cli.cmd {
        Cmd::Init { slug, project, description } => {
            commands::init(&home, &cwd, slug, project, description, json)?
        }
        Cmd::List => commands::list(&home, json)?,
        Cmd::Toc => commands::toc(&resolve()?, json)?,
        Cmd::Context => commands::context(&resolve()?, json)?,
        Cmd::Read { id, expand } => commands::read(&resolve()?, &id, expand.unwrap_or(0), json)?,
        Cmd::New { id, title, tags, sources, pin } => {
            commands::new_page(
                &resolve()?,
                &id,
                title,
                split_csv(tags).unwrap_or_default(),
                split_csv(sources).unwrap_or_default(),
                pin,
                stdin_body(),
                json,
            )?
        }
        Cmd::Write { id, append, sources, pin, unpin } => {
            let body = stdin_body().unwrap_or_default();
            let pin = if pin { Some(true) } else if unpin { Some(false) } else { None };
            commands::write(&resolve()?, &id, &body, append, split_csv(sources), pin, json)?
        }
        Cmd::Rm { id } => commands::rm(&resolve()?, &id, json)?,
        Cmd::Mv { old, new } => commands::mv(&resolve()?, &old, &new, json)?,
        Cmd::Expand { id } => commands::expand(&resolve()?, id.as_deref(), json)?,
        Cmd::Ingest { level, mark, full, since } => {
            let mut w = resolve()?;
            commands::ingest(&mut w, &cwd, level, mark, full, since.as_deref(), json)?
        }
        Cmd::Search { query, tag } => commands::search(&resolve()?, &query, tag.as_deref(), json)?,
        Cmd::Links { id } => commands::links(&resolve()?, &id, json)?,
        Cmd::Doctor { fix } => commands::doctor(&resolve()?, fix, json)?,
        Cmd::Obsidian { print } => commands::obsidian(&resolve()?, print, json)?,
        Cmd::Plugin { cmd: PluginCmd::Install { target } } => plugins::install(target)?,
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
