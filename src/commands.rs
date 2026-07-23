//! Every wookie verb, shared by the CLI and the MCP server. Each function
//! returns its output as a string; callers decide where it goes.

use crate::config::{GlobalConfig, MAX_EXCERPT_LINES, MAX_RETRIEVAL_TOKENS, MAX_SEARCH_LIMIT};
use crate::page::{first_sentence, humanize, rewrite_links, today, Page, PinLevel};
use crate::sessions;
use crate::wiki::{self, Wiki};
use crate::{audit, protocol, publish, report, retrieval, retrieval_index, snapshot};
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn slugify(name: &str) -> String {
    let mut s: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    while s.contains("--") {
        s = s.replace("--", "-");
    }
    s.trim_matches('-').to_string()
}

fn indent(text: &str) -> String {
    text.lines()
        .map(|l| format!("  {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn init(
    home: &Path,
    cwd: &Path,
    slug: Option<String>,
    project: Option<PathBuf>,
    description: Option<String>,
    json: bool,
) -> Result<String> {
    // Register the main worktree, never a linked worktree that may be temporary.
    let project_root = match project {
        Some(p) => p,
        None => wiki::git_main_worktree(cwd).unwrap_or_else(|| cwd.to_path_buf()),
    };
    let project_root = project_root
        .canonicalize()
        .unwrap_or(project_root)
        .to_string_lossy()
        .to_string();

    let slug = match slug {
        Some(s) => slugify(&s),
        None => slugify(
            Path::new(&project_root)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "wiki".to_string())
                .as_str(),
        ),
    };
    if slug.is_empty() {
        bail!("could not derive a wiki slug; pass one explicitly: wookie init <slug>");
    }

    let description = description.unwrap_or_default();
    let wiki_config = wiki::WikiConfig {
        name: slug.clone(),
        description: description.clone(),
        project_roots: vec![project_root.clone()],
        auto_commit: None,
        sessions: Default::default(),
        history: Default::default(),
        retrieval: Default::default(),
        audit: Default::default(),
        publish: Default::default(),
        last_ingest_commit: None,
        sections: wiki::default_sections(),
    };
    // Validate user-provided name/description/root metadata before creating
    // any persistent wiki directories, so a rejected init leaves no partial
    // registration behind.
    wiki_config.validate()?;
    let dir = GlobalConfig::with_home_lock(home, |home_guard| {
        if wiki::all_wikis(home).contains(&slug) {
            bail!("wiki '{slug}' already exists (wookie list)");
        }
        for other in wiki::all_wikis(home) {
            if let Ok(w) = wiki::open(home, &other) {
                if w.config.project_roots.iter().any(|r| r == &project_root) {
                    bail!("{project_root} is already registered to wiki '{other}'");
                }
            }
        }

        let canonical_home = home.canonicalize().unwrap_or_else(|_| home.to_path_buf());
        let dir = wiki::create_contained_dir_all(&canonical_home, Path::new(&slug))?;
        wiki::create_contained_dir_all(&dir, Path::new("pages"))?;
        wiki::create_contained_dir_all(&dir, Path::new("protocols/findings"))?;
        let config_path = wiki::contained_path(&dir, Path::new("wookie.toml"))?;
        wiki::atomic_write(&config_path, toml::to_string_pretty(&wiki_config)?)?;
        let finding_protocol =
            wiki::contained_path(&dir, Path::new("protocols/findings/finding.md"))?;
        wiki::atomic_write(
            &finding_protocol,
            "+++\ndescription = \"Record an actionable review finding\"\nsection = \"findings\"\ntags = [\"finding\", \"status/open\"]\n+++\n**{{title}}** records finding `{{id}}` and the evidence needed to resolve it.\n\n## Severity\n\nSet one tag: `severity/critical`, `severity/high`, `severity/medium`, `severity/low`, or `severity/info`.\n\n## Affected files\n\n- `path/to/file`\n\n## Owner\n\nUnassigned. Add an `owner/<name>` tag when assigned.\n\n## Remediation\n\nDescribe the required change.\n\n## Verification evidence\n\nRecord the command, revision, or artifact proving the remediation. Replace `status/open` with `status/verified` when complete.\n",
        )?;
        GlobalConfig::ensure_exists_guarded(home, home_guard)?;
        Ok(dir)
    })?;

    let w = wiki::open(home, &slug)?;
    let guard = w.acquire_mutation_guard()?;
    w.ensure_gitignore_guarded(&guard)?;
    w.init_git();
    let mut index = Page {
        id: "index".into(),
        fm: crate::page::Frontmatter {
            title: humanize(&slug),
            description: if description.is_empty() {
                format!("Home page of the {slug} wiki")
            } else {
                description.clone()
            },
            tags: vec![],
            created: today(),
            updated: today(),
            status: None,
            sources: vec![],
            pin: false,
            pin_level: None,
            aliases: vec![humanize(&slug)],
            extra: vec![],
        },
        body: format!(
            "**The front door of this wiki.** It maps the project at `{project_root}`; \
             every page below is reachable by hovering or clicking a wikilink.\n\n\
             Add pages with `wookie new <id>` and connect them with wikilinks like `[[another-page]]`. \
             Start with `wookie prime --query \"your task\"` for a bounded overview and run \
             `wookie doctor` to check health. Use `wookie context` only for the exhaustive catalog.\n\n\
             > [!tip] In Obsidian, hover any `[[link]]` to preview a page's summary paragraph."
        ),
    };
    w.assert_writable(&index.id)?;
    w.save_page_raw_guarded(&guard, &mut index, false)?;
    w.commit_paths(
        "wookie: init",
        &[
            ".gitignore".into(),
            "wookie.toml".into(),
            "protocols/findings/finding.md".into(),
            "pages/index.md".into(),
        ],
    )?;

    if json {
        return Ok(serde_json::json!({
            "slug": slug, "dir": dir, "project_root": project_root,
        })
        .to_string());
    }
    Ok(format!(
        "Created wiki '{slug}' at {}\nRegistered project root: {project_root}\nSeeded page: index",
        dir.display()
    ))
}

pub fn list(home: &Path, json: bool) -> Result<String> {
    let slugs = wiki::all_wikis(home);
    if slugs.is_empty() {
        return Ok("No wikis yet. Run `wookie init` from a project directory.".into());
    }
    let mut rows = vec![];
    for slug in &slugs {
        let (pages, stubs, description, roots) = match wiki::open(home, slug) {
            Ok(w) => {
                let pages = w.all_pages();
                let stubs = pages.iter().filter(|p| p.is_stub()).count();
                (
                    pages.len(),
                    stubs,
                    w.config.description.clone(),
                    w.config.project_roots.clone(),
                )
            }
            Err(_) => (0, 0, "(unreadable)".into(), vec![]),
        };
        rows.push(serde_json::json!({
            "slug": slug, "pages": pages, "stubs": stubs,
            "description": description, "project_roots": roots,
        }));
    }
    if json {
        return Ok(serde_json::json!({ "wikis": rows }).to_string());
    }
    let mut out = String::new();
    for r in &rows {
        let _ = writeln!(
            out,
            "{}  ({} pages, {} stubs)  {}",
            r["slug"].as_str().unwrap_or(""),
            r["pages"],
            r["stubs"],
            r["project_roots"]
                .as_array()
                .map(|a| a
                    .iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(", "))
                .unwrap_or_default(),
        );
    }
    Ok(out.trim_end().to_string())
}

fn toc_rows(w: &Wiki) -> Vec<(String, String, bool)> {
    w.all_pages()
        .into_iter()
        .map(|p| (p.id.clone(), p.fm.description.clone(), p.is_stub()))
        .collect()
}

fn section_of(id: &str) -> Option<&str> {
    id.split_once('/').map(|(s, _)| s)
}

/// Pages grouped into (index row, section rows in section order, unfiled rows).
/// `index` is the wiki's front door and leads the listing.
type Row = (String, String, bool);
type Rows = Vec<Row>;
fn grouped_rows(w: &Wiki) -> (Option<Row>, Vec<(String, wiki::SectionConfig, Rows)>, Rows) {
    let sections = w.sections();
    let rows = toc_rows(w);
    let mut by_section: BTreeMap<String, Rows> = BTreeMap::new();
    let mut unfiled: Rows = vec![];
    let mut index: Option<Row> = None;
    for row in rows {
        let sec = section_of(&row.0).map(str::to_string);
        match sec {
            Some(s) if sections.contains_key(&s) => by_section.entry(s).or_default().push(row),
            _ if row.0 == "index" => index = Some(row),
            _ => unfiled.push(row),
        }
    }
    let grouped = sections
        .into_iter()
        .map(|(name, cfg)| {
            let pages = by_section.remove(&name).unwrap_or_default();
            (name, cfg, pages)
        })
        .collect();
    (index, grouped, unfiled)
}

fn render_grouped(w: &Wiki, out: &mut String) {
    let (index, grouped, unfiled) = grouped_rows(w);
    if let Some((id, desc, _)) = &index {
        let _ = writeln!(out, "\n- {id} — {desc}");
    }
    for (name, cfg, pages) in &grouped {
        let mut flags = vec![];
        if cfg.kind == wiki::SectionKind::Rules {
            flags.push("rules".to_string());
        }
        if cfg.is_locked() {
            flags.push(if w.is_unlocked(name) {
                "temporarily unlocked".to_string()
            } else {
                "locked".to_string()
            });
        }
        let flags = if flags.is_empty() {
            String::new()
        } else {
            format!(" [{}]", flags.join(", "))
        };
        let _ = writeln!(out, "\n{name}/{flags} — {}", cfg.description);
        if pages.is_empty() {
            let _ = writeln!(out, "  (no pages yet)");
        }
        for (id, desc, stub) in pages {
            let marker = if *stub { "  [stub]" } else { "" };
            let _ = writeln!(out, "- {id} — {desc}{marker}");
        }
    }
    if !unfiled.is_empty() {
        let _ = writeln!(out, "\nunfiled (consider moving under a section):");
        for (id, desc, stub) in &unfiled {
            let marker = if *stub { "  [stub]" } else { "" };
            let _ = writeln!(out, "- {id} — {desc}{marker}");
        }
    }
}

fn grouped_json(w: &Wiki) -> serde_json::Value {
    let (index, grouped, unfiled) = grouped_rows(w);
    serde_json::json!({
        "index": index.map(|(id, d, _)| serde_json::json!({"id": id, "description": d})),
        "sections": grouped.iter().map(|(name, cfg, pages)| serde_json::json!({
            "name": name,
            "description": cfg.description,
            "kind": if cfg.kind == wiki::SectionKind::Rules { "rules" } else { "info" },
            "locked": cfg.is_locked(),
            "required": cfg.required,
            "pages": pages.iter().map(|(id, d, stub)| serde_json::json!({"id": id, "description": d, "stub": stub})).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "unfiled": unfiled.iter().map(|(id, d, stub)| serde_json::json!({"id": id, "description": d, "stub": stub})).collect::<Vec<_>>(),
    })
}

pub fn toc(w: &Wiki, json: bool) -> Result<String> {
    if json {
        let mut v = grouped_json(w);
        v["wiki"] = serde_json::json!(w.slug);
        return Ok(v.to_string());
    }
    if toc_rows(w).is_empty() {
        return Ok(format!("Wiki '{}' has no pages yet.", w.slug));
    }
    let mut out = String::new();
    render_grouped(w, &mut out);
    Ok(out.trim().to_string())
}

pub fn context(w: &Wiki, json: bool) -> Result<String> {
    let pages = w.all_pages();
    let stubs = pages.iter().filter(|p| p.is_stub()).count();
    let pinned: Vec<&Page> = pages.iter().filter(|p| p.fm.pin).collect();
    if json {
        let mut v = grouped_json(w);
        v["wiki"] = serde_json::json!(w.slug);
        v["description"] = serde_json::json!(w.config.description);
        v["project_roots"] = serde_json::json!(w.config.project_roots);
        v["pinned"] = pinned
            .iter()
            .map(|p| {
                let level = p.pin_level().unwrap_or(PinLevel::Instruction);
                match level {
                    PinLevel::Instruction => {
                        serde_json::json!({"id": p.id, "level": level, "body": p.body.clone(), "stub": p.is_stub()})
                    }
                    PinLevel::Summary => {
                        serde_json::json!({"id": p.id, "level": level, "body": p.summary(), "stub": p.is_stub()})
                    }
                    PinLevel::Discoverable => serde_json::json!({
                        "id": p.id,
                        "level": level,
                        "title": p.fm.title.clone(),
                        "description": p.fm.description.clone(),
                        "stub": p.is_stub(),
                        "read_command": format!("wookie read {}", p.id),
                    }),
                }
            })
            .collect();
        return Ok(v.to_string());
    }
    let mut out = String::new();
    let _ = writeln!(out, "Wiki: {} — {}", w.slug, w.config.description);
    let _ = writeln!(out, "Project roots: {}", w.config.project_roots.join(", "));
    let _ = writeln!(
        out,
        "{} pages, {} stubs needing content",
        pages.len(),
        stubs
    );
    let standing = pinned
        .iter()
        .copied()
        .filter(|page| page.pin_level() != Some(PinLevel::Discoverable))
        .collect::<Vec<_>>();
    let discoverable = pinned
        .iter()
        .copied()
        .filter(|page| page.pin_level() == Some(PinLevel::Discoverable))
        .collect::<Vec<_>>();
    if !standing.is_empty() {
        let _ = writeln!(out, "\n== Pinned instructions (always follow these) ==");
        for p in standing {
            let level = p.pin_level().unwrap_or(PinLevel::Instruction);
            let content = match level {
                PinLevel::Instruction => p.body.clone(),
                PinLevel::Summary => p.summary(),
                PinLevel::Discoverable => unreachable!("filtered above"),
            };
            let _ = writeln!(
                out,
                "\n### {} ({}, {:?}){}\n{}",
                p.fm.title,
                p.id,
                level,
                if p.is_stub() { " [stub]" } else { "" },
                content.trim_end()
            );
        }
    }
    if !discoverable.is_empty() {
        let _ = writeln!(out, "\n== Pinned references (read on demand) ==");
        for page in discoverable {
            let _ = writeln!(
                out,
                "- {} — {}{} | `wookie read {}`",
                page.id,
                page.fm.description,
                if page.is_stub() { " [stub]" } else { "" },
                page.id
            );
        }
    }
    if !pinned.is_empty() {
        let _ = writeln!(out, "\n== Reference pages ==");
    }
    render_grouped(w, &mut out);
    let _ = writeln!(
        out,
        "\nRead a page with linked context: wookie read <id> --expand\nSearch: wookie search <query> | Grow: wookie expand"
    );
    Ok(out.trim_end().to_string())
}

#[derive(Debug, Clone)]
pub struct PrimeOptions {
    pub query: String,
    pub tokens: Option<usize>,
    pub instruction_tokens: Option<usize>,
    pub limit: Option<usize>,
    pub max_per_section: Option<usize>,
    pub since: Option<String>,
    pub cursor: usize,
    /// Exact query/options/state identity returned by the previous window.
    pub context_hash: Option<String>,
    /// Invocation directory used to select the active registered worktree.
    pub cwd: Option<PathBuf>,
}

fn hash_field(hash: &mut Sha256, bytes: &[u8]) {
    hash.update(u64::try_from(bytes.len()).unwrap_or(u64::MAX).to_be_bytes());
    hash.update(bytes);
}

fn prime_state_hash(
    w: &Wiki,
    catalog_content_hash: &str,
    pin_levels: &BTreeMap<String, PinLevel>,
    freshness: &retrieval::FreshnessOutcome,
) -> Result<String> {
    let mut hash = Sha256::new();
    hash_field(&mut hash, b"wookie.prime-state/v1");
    hash_field(&mut hash, catalog_content_hash.as_bytes());
    hash_field(&mut hash, w.slug.as_bytes());
    hash_field(&mut hash, &serde_json::to_vec(&w.config)?);
    hash_field(&mut hash, &serde_json::to_vec(&w.retrieval)?);
    hash_field(&mut hash, &serde_json::to_vec(pin_levels)?);
    hash_field(&mut hash, &serde_json::to_vec(freshness)?);
    for (name, section) in w.sections() {
        hash_field(&mut hash, name.as_bytes());
        hash_field(&mut hash, section.description.as_bytes());
        hash_field(&mut hash, format!("{:?}", section.kind).as_bytes());
        hash_field(&mut hash, &[u8::from(section.is_locked())]);
        for required in section.required {
            hash_field(&mut hash, required.as_bytes());
        }
    }
    Ok(format!("sha256:{:x}", hash.finalize()))
}

#[derive(Clone, Copy)]
struct PrimeWindow {
    budget: usize,
    instruction_budget: usize,
    limit: usize,
    max_per_section: usize,
}

fn prime_context_hash(state_hash: &str, options: &PrimeOptions, window: PrimeWindow) -> String {
    let mut hash = Sha256::new();
    hash_field(&mut hash, b"wookie.prime-context/v2");
    hash_field(&mut hash, state_hash.as_bytes());
    hash_field(&mut hash, options.query.as_bytes());
    for value in [
        window.budget,
        window.instruction_budget,
        window.limit,
        window.max_per_section,
    ] {
        hash_field(
            &mut hash,
            &u64::try_from(value).unwrap_or(u64::MAX).to_be_bytes(),
        );
    }
    format!("sha256:{:x}", hash.finalize())
}

fn prime_continuation_argv(
    options: &PrimeOptions,
    state_hash: &str,
    context_hash: &str,
    cursor: usize,
    window: PrimeWindow,
) -> Vec<String> {
    vec![
        "wookie".to_string(),
        "prime".to_string(),
        "--query".to_string(),
        options.query.clone(),
        "--tokens".to_string(),
        window.budget.to_string(),
        "--instruction-tokens".to_string(),
        window.instruction_budget.to_string(),
        "--limit".to_string(),
        window.limit.to_string(),
        "--max-per-section".to_string(),
        window.max_per_section.to_string(),
        "--since".to_string(),
        state_hash.to_string(),
        "--context-hash".to_string(),
        context_hash.to_string(),
        "--cursor".to_string(),
        cursor.to_string(),
    ]
}

fn git_worktree_root(cwd: &Path) -> Option<PathBuf> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .ok()?;
    output.status.success().then(|| {
        PathBuf::from(String::from_utf8_lossy(&output.stdout).trim())
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from(String::from_utf8_lossy(&output.stdout).trim()))
    })
}

fn freshness_project_root(w: &Wiki, cwd: Option<&Path>) -> Result<PathBuf> {
    if w.config.project_roots.is_empty() {
        bail!("no project root is configured");
    }
    let registered = w
        .config
        .project_roots
        .iter()
        .map(|root| {
            let path = PathBuf::from(root);
            path.canonicalize().unwrap_or(path)
        })
        .collect::<Vec<_>>();
    if let Some(cwd) = cwd {
        let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
        if let Some(root) = registered
            .iter()
            .filter(|root| cwd.starts_with(root))
            .max_by_key(|root| root.components().count())
        {
            return Ok(root.clone());
        }
        // A linked worktree resolves through its registered main checkout,
        // but freshness must inspect the active worktree's dirty state.
        if let (Some(main), Some(active)) = (wiki::git_main_worktree(&cwd), git_worktree_root(&cwd))
        {
            let main = main.canonicalize().unwrap_or(main);
            if registered.iter().any(|root| root == &main) {
                return Ok(active);
            }
        }
    }
    if let [root] = registered.as_slice() {
        return Ok(root.clone());
    }
    bail!("multiple project roots are configured and the active worktree is ambiguous")
}

fn project_freshness(w: &Wiki, pages: &[Page], cwd: Option<&Path>) -> retrieval::FreshnessOutcome {
    let Some(base) = w.config.last_ingest_commit.as_deref() else {
        return retrieval::FreshnessOutcome::unknown("no last ingest revision is recorded");
    };
    let root = match freshness_project_root(w, cwd) {
        Ok(root) => root,
        Err(error) => return retrieval::FreshnessOutcome::unknown(error.to_string()),
    };
    let changed = match changed_since(&root, base) {
        Ok(changed) => changed,
        Err(error) => {
            return retrieval::FreshnessOutcome::unknown(format!(
                "cannot compare with the last ingest revision: {error:#}"
            ));
        }
    };
    let projections = pages
        .iter()
        .map(|page| retrieval::RetrievalPage {
            id: page.id.clone(),
            sources: audit::effective_page_sources(page),
            ..retrieval::RetrievalPage::default()
        })
        .collect::<Vec<_>>();
    retrieval::FreshnessOutcome::from_changed_paths(&projections, &changed)
}

fn freshness_summary(freshness: &retrieval::FreshnessOutcome) -> String {
    match freshness.state {
        retrieval::FreshnessState::Current => {
            "current (successful comparison found no project changes)".to_string()
        }
        retrieval::FreshnessState::Stale => format!(
            "stale ({} changed path(s), {} mapped page(s), {} uncovered)",
            freshness.changed_count.unwrap_or_default(),
            freshness.stale_page_ids.len(),
            freshness.uncovered_count.unwrap_or_default()
        ),
        retrieval::FreshnessState::Unknown => format!(
            "unknown ({})",
            freshness
                .error
                .as_deref()
                .unwrap_or("project freshness could not be determined")
        ),
    }
}

fn reason_text(result: &retrieval::RankedPage) -> String {
    result
        .reasons
        .iter()
        .take(3)
        .map(|reason| {
            let kind = serde_json::to_value(reason.kind)
                .ok()
                .and_then(|value| value.as_str().map(str::to_string))
                .unwrap_or_else(|| format!("{:?}", reason.kind).to_lowercase());
            reason
                .detail
                .as_deref()
                .map(|detail| format!("{kind}: {detail}"))
                .unwrap_or(kind)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

const MAX_PRIME_DISCOVERABLE_PAGES: usize = 20;

pub fn prime(w: &Wiki, options: &PrimeOptions, json: bool) -> Result<String> {
    let started = std::time::Instant::now();
    retrieval::validate_query(&options.query)?;
    let budget = options.tokens.unwrap_or(w.retrieval.prime_tokens);
    let instruction_budget = options
        .instruction_tokens
        .unwrap_or_else(|| w.retrieval.instruction_tokens.min(budget));
    let limit = options.limit.unwrap_or(w.retrieval.search_limit);
    let max_per_section = options
        .max_per_section
        .unwrap_or(w.retrieval.max_per_section);
    if budget == 0 || instruction_budget == 0 || limit == 0 || max_per_section == 0 {
        bail!("prime budgets, limit, and max-per-section must be greater than zero");
    }
    if limit > MAX_SEARCH_LIMIT {
        bail!("prime limit must not exceed {MAX_SEARCH_LIMIT}");
    }
    if budget > MAX_RETRIEVAL_TOKENS || instruction_budget > MAX_RETRIEVAL_TOKENS {
        bail!("prime token budgets must not exceed {MAX_RETRIEVAL_TOKENS}");
    }
    if max_per_section > MAX_SEARCH_LIMIT {
        bail!("prime max-per-section must not exceed {MAX_SEARCH_LIMIT}");
    }
    if instruction_budget > budget {
        bail!("instruction token budget cannot exceed the total prime budget");
    }
    let window = PrimeWindow {
        budget,
        instruction_budget,
        limit,
        max_per_section,
    };

    let mut catalog = retrieval_index::load(w)?;
    let pin_levels = catalog.pin_levels.clone();
    let mut pages = std::mem::take(&mut catalog.pages);
    // Standing text is security-sensitive behavior, not merely a retrieval
    // hint. Re-read Instruction/Summary bodies from canonical storage and
    // bind them to the same raw leaf named by this catalog generation.
    for page in pages.iter_mut().filter(|page| {
        matches!(
            pin_levels.get(&page.id),
            Some(PinLevel::Instruction | PinLevel::Summary)
        )
    }) {
        let path = w.page_path(&page.id)?;
        let raw = snapshot::read_raw_page(&path)?;
        let raw_sha256 = snapshot::raw_page_sha256(&raw);
        if catalog.raw_sha256.get(&page.id) != Some(&raw_sha256) {
            bail!(
                "pinned page '{}' changed while priming; retry the command",
                page.id
            );
        }
        let text = std::str::from_utf8(&raw)
            .with_context(|| format!("pinned page '{}' is not valid UTF-8", page.id))?;
        *page = Page::parse(&page.id, text);
        if page.pin_level() != pin_levels.get(&page.id).copied() {
            bail!(
                "pinned page '{}' changed pin level while priming; retry the command",
                page.id
            );
        }
    }
    retrieval_index::verify_generation(w, &catalog)?;
    if let Some((page, issue)) = pages
        .iter()
        .find_map(|page| page.standing_text_issue().map(|issue| (page, issue)))
    {
        bail!(
            "pinned standing page '{}' {issue}; fill or unpin it before priming",
            page.id
        );
    }
    let freshness = project_freshness(w, &pages, options.cwd.as_deref());
    let state_hash = prime_state_hash(w, &catalog.content_hash, &pin_levels, &freshness)?;
    let context_hash = prime_context_hash(&state_hash, options, window);
    let legacy_cursor_binding =
        options.context_hash.is_none() && options.since.as_deref() == Some(context_hash.as_str());
    if options.cursor > 0
        && options.context_hash.as_deref() != Some(context_hash.as_str())
        && !legacy_cursor_binding
    {
        bail!(
            "prime cursor {} is not bound to the current query/options and state; restart at cursor 0 or pass --context-hash {context_hash}",
            options.cursor
        );
    }
    let unchanged = options.since.as_deref() == Some(state_hash.as_str())
        || options.since.as_deref() == Some(context_hash.as_str());
    let projections: Vec<_> = pages
        .iter()
        .map(|page| retrieval::RetrievalPage::from_page(page, freshness.is_stale(&page.id)))
        .collect();
    let mut ranked = retrieval::rank_pages(&options.query, &projections);
    if ranked.is_empty() {
        for id in ["index", "architecture/overview"] {
            if let Some(page) = pages.iter().find(|page| page.id == id) {
                ranked.push(retrieval::RankedPage {
                    id: page.id.clone(),
                    title: page.fm.title.clone(),
                    description: page.fm.description.clone(),
                    score: 0,
                    reasons: Vec::new(),
                    excerpt: None,
                    stale: freshness.is_stale(&page.id),
                });
            }
        }
    }

    let instructions: Vec<_> = pages
        .iter()
        .filter(|page| {
            matches!(
                page.pin_level(),
                Some(PinLevel::Instruction | PinLevel::Summary)
            )
        })
        .map(|page| {
            (
                page.id.clone(),
                page.pin_level().unwrap_or(PinLevel::Instruction),
                page.pinned_text(),
            )
        })
        .collect();
    let discoverable_total = pages
        .iter()
        .filter(|page| page.pin_level() == Some(PinLevel::Discoverable))
        .count();
    let discoverable_pages = pages
        .iter()
        .filter(|page| page.pin_level() == Some(PinLevel::Discoverable))
        .take(MAX_PRIME_DISCOVERABLE_PAGES)
        .map(|page| {
            serde_json::json!({
                "id": page.id,
                "title": retrieval::compact_excerpt(&page.fm.title),
                "description": retrieval::compact_excerpt(&page.fm.description),
                "stub": page.is_stub(),
                "read_command": format!("wookie read {}", page.id),
            })
        })
        .collect::<Vec<_>>();
    let instruction_tokens = instructions
        .iter()
        .map(|(id, _, text)| retrieval::estimate_standing_tokens(id, text))
        .sum::<usize>();
    if instruction_tokens > instruction_budget {
        bail!(
            "pinned standing text needs about {instruction_tokens} tokens, exceeding the configured {instruction_budget}-token instruction budget; shorten or unpin pages (doctor reports this too)"
        );
    }

    let mut selected: Vec<(usize, retrieval::RankedPage)> = Vec::new();
    let mut per_section: HashMap<String, usize> = HashMap::new();
    let mut next_cursor = None;
    let mut scan = options.cursor.min(ranked.len());
    while scan < ranked.len() && selected.len() < limit {
        let ranked_index = scan;
        let result = &ranked[scan];
        let section = result.id.split('/').next().unwrap_or("unfiled").to_string();
        scan += 1;
        let count = per_section.entry(section).or_default();
        if *count >= max_per_section {
            // A numeric cursor can represent only one contiguous ranked
            // window. Stop at the first capped item so a continuation never
            // skips pages or repeats later results.
            next_cursor = Some(ranked_index);
            break;
        }
        *count += 1;
        selected.push((ranked_index, result.clone()));
    }
    if next_cursor.is_none() && scan < ranked.len() {
        next_cursor = Some(scan);
    }

    let sections: Vec<_> = w
        .sections()
        .into_iter()
        .map(|(name, config)| {
            serde_json::json!({
                "name": name,
                "description": config.description,
                "kind": config.kind,
                "locked": config.is_locked(),
            })
        })
        .collect();

    if json {
        let mut json_selected = selected.clone();
        let mut json_sections = if unchanged {
            Vec::new()
        } else {
            sections.clone()
        };
        let mut json_discoverable_pages = discoverable_pages.clone();
        // Freshness detail is telemetry, so keep its diagnostic page list
        // compact while preserving the total and omission count. The full
        // reconciliation worklist remains available through `wookie ingest`.
        let mut json_stale_page_ids = freshness
            .stale_page_ids
            .iter()
            .take(20)
            .cloned()
            .collect::<Vec<_>>();
        let mut continuation = next_cursor;
        let mut budget_blocked_by: Option<String> = None;
        loop {
            let pages_omitted = ranked.len().saturating_sub(
                options
                    .cursor
                    .min(ranked.len())
                    .saturating_add(json_selected.len()),
            );
            let sections_omitted = sections.len().saturating_sub(json_sections.len());
            let discoverable_omitted =
                discoverable_total.saturating_sub(json_discoverable_pages.len());
            let mut value = serde_json::json!({
                "schema": "wookie.prime/v1",
                "wiki": {"slug": w.slug, "description": w.config.description, "freshness": freshness.state},
                "query": options.query,
                "state_hash": state_hash,
                "context_hash": context_hash,
                "unchanged_since": unchanged,
                "instructions": instructions.iter().map(|(id, level, text)| serde_json::json!({"id": id, "level": level, "text": text})).collect::<Vec<_>>(),
                "discoverable_pages": json_discoverable_pages,
                "discoverable_next_command": (discoverable_omitted > 0).then_some("wookie context"),
                "sections": json_sections,
                "suggested_pages": json_selected.iter().map(|(_, result)| serde_json::json!({
                    "id": result.id,
                    "title": result.title,
                    "description": result.description,
                    "score": result.score,
                    "reasons": result.reasons,
                    "stale": result.stale,
                })).collect::<Vec<_>>(),
                "continuation": continuation,
                "continuation_argv": continuation.map(|cursor| prime_continuation_argv(options, &state_hash, &context_hash, cursor, window)),
                "budget_blocked_by": budget_blocked_by,
                "next_command": budget_blocked_by.as_deref().map(|id| format!("wookie read {id}")),
                "omissions": {"sections": sections_omitted, "pages": pages_omitted, "discoverable_pages": discoverable_omitted},
                "telemetry": {
                    "pages_considered": pages.len(), "pages_matched": ranked.len(),
                    "pages_returned": json_selected.len(), "pages_omitted": pages_omitted,
                    "estimated_tokens": 0, "instruction_tokens": instruction_tokens,
                    "pin_pages_reread": pin_levels.values().filter(|level| **level != PinLevel::Discoverable).count(),
                    "discoverable_pages_returned": json_discoverable_pages.len(),
                    "discoverable_pages_omitted": discoverable_omitted,
                    "budget_tokens": budget, "retrieval_ms": started.elapsed().as_millis(),
                    "cache": &catalog.cache,
                    "freshness": {
                        "state": freshness.state,
                        "changed_count": freshness.changed_count,
                        "stale_page_count": freshness.stale_page_ids.len(),
                        "stale_page_ids": json_stale_page_ids,
                        "stale_page_ids_omitted": freshness.stale_page_ids.len().saturating_sub(json_stale_page_ids.len()),
                        "uncovered_count": freshness.uncovered_count,
                        "error": freshness.error.as_deref()
                    }
                }
            });
            let initial = serde_json::to_string(&value)?;
            value["telemetry"]["estimated_tokens"] =
                serde_json::json!(retrieval::estimate_tokens(&initial));
            let rendered = serde_json::to_string(&value)?;
            if retrieval::estimate_tokens(&rendered) <= budget {
                return Ok(rendered);
            }
            // Keep at least one task-relevant page while compacting the map;
            // after section summaries are exhausted, that final page can go
            // too. Every dropped page remains reachable via the cursor.
            if json_selected.len() > 1 {
                let (index, _) = json_selected.pop().expect("length checked");
                continuation = Some(index);
            } else if json_sections.pop().is_some()
                || json_stale_page_ids.pop().is_some()
                || json_discoverable_pages.pop().is_some()
            {
                continue;
            } else if let Some((_, result)) = json_selected.pop() {
                budget_blocked_by = Some(result.id);
                continuation = None;
            } else {
                bail!(
                    "prime metadata and standing instructions exceed the {budget}-token response budget"
                );
            }
        }
    }

    let mut prefix = format!(
        "Wiki: {} — {}\nFreshness: {} | State hash: {state_hash}{}\nContext hash: {context_hash}\nTask: {}\n",
        w.slug,
        w.config.description,
        freshness_summary(&freshness),
        if unchanged {
            " (catalog unchanged)"
        } else {
            ""
        },
        options.query
    );
    if !instructions.is_empty() {
        prefix.push_str("\n== Standing instructions ==\n");
        for (id, level, text) in &instructions {
            let _ = write!(prefix, "\n### {id} ({level:?})\n{text}\n");
        }
    }
    let section_lines = sections
        .iter()
        .map(|section| {
            format!(
                "- {}/ — {}\n",
                section["name"].as_str().unwrap_or_default(),
                section["description"].as_str().unwrap_or_default()
            )
        })
        .collect::<Vec<_>>();
    let suggestion_chunks = selected
        .iter()
        .map(|(ranked_index, result)| {
            let stale_marker = if result.stale { " [stale]" } else { "" };
            let reasons = reason_text(result);
            (
                *ranked_index,
                format!(
                    "- {} — {}: {}{}\n  Why: {} | score {}\n",
                    result.id,
                    report::terminal_safe(&result.title),
                    report::terminal_safe(&result.description),
                    stale_marker,
                    if reasons.is_empty() {
                        "catalog fallback"
                    } else {
                        &reasons
                    },
                    result.score
                ),
            )
        })
        .collect::<Vec<_>>();
    let mut section_count = if unchanged { 0 } else { section_lines.len() };
    let mut suggestion_count = suggestion_chunks.len();
    let mut discoverable_count = discoverable_pages.len();
    let mut continuation = next_cursor;
    let mut budget_blocked_by = None;
    loop {
        let mut out = prefix.clone();
        if discoverable_count > 0 {
            out.push_str("\n== Discoverable pinned pages ==\n");
            for page in discoverable_pages.iter().take(discoverable_count) {
                let _ = writeln!(
                    out,
                    "- {} — {}: {} | `wookie read {}`",
                    page["id"].as_str().unwrap_or_default(),
                    report::terminal_safe(page["title"].as_str().unwrap_or_default()),
                    report::terminal_safe(page["description"].as_str().unwrap_or_default()),
                    page["id"].as_str().unwrap_or_default(),
                );
            }
        }
        let discoverable_omitted = discoverable_total.saturating_sub(discoverable_count);
        if discoverable_omitted > 0 {
            let _ = writeln!(
                out,
                "Discoverable pins omitted: {discoverable_omitted}; run `wookie context` for the complete pinned-reference map."
            );
        }
        if !unchanged {
            out.push_str("\n== Sections ==\n");
            for line in section_lines.iter().take(section_count) {
                out.push_str(line);
            }
            let omitted = section_lines.len().saturating_sub(section_count);
            if omitted > 0 {
                let _ = writeln!(
                    out,
                    "Sections omitted: {omitted}; run `wookie context` for the full catalog."
                );
            }
        }
        out.push_str("\n== Suggested pages ==\n");
        for (_, chunk) in suggestion_chunks.iter().take(suggestion_count) {
            out.push_str(chunk);
        }
        if let Some(cursor) = continuation {
            let _ = writeln!(
                out,
                "Continuation: reuse state hash {state_hash}, the same query/options, context hash {context_hash}, and cursor {cursor} ({} ranked page(s) remain).",
                ranked.len().saturating_sub(cursor)
            );
        }
        if let Some(id) = &budget_blocked_by {
            let _ = writeln!(
                out,
                "Suggested page metadata exceeded the remaining budget; read it directly with `wookie read {id}`."
            );
        }
        let pages_omitted = ranked.len().saturating_sub(
            options
                .cursor
                .min(ranked.len())
                .saturating_add(suggestion_count),
        );
        let telemetry_prefix = format!(
            "Telemetry: considered {}, matched {}, returned {}, omitted {}, instructions {}, retrieval {} ms, cache {} ({} reused, {} refreshed); estimated ",
            pages.len(),
            ranked.len(),
            suggestion_count,
            pages_omitted,
            instruction_tokens,
            started.elapsed().as_millis(),
            catalog.cache.state,
            catalog.cache.pages_reused,
            catalog.cache.pages_refreshed,
        );
        let provisional = format!("{out}{telemetry_prefix}0 / {budget} tokens.\n");
        let estimate = retrieval::estimate_tokens(&provisional);
        let mut rendered = format!("{out}{telemetry_prefix}{estimate} / {budget} tokens.\n");
        let final_estimate = retrieval::estimate_tokens(&rendered);
        if final_estimate != estimate {
            rendered = format!("{out}{telemetry_prefix}{final_estimate} / {budget} tokens.\n");
        }
        if retrieval::estimate_tokens(&rendered) <= budget {
            return Ok(rendered.trim_end().to_string());
        }
        if suggestion_count > 1 {
            suggestion_count -= 1;
            continuation = Some(suggestion_chunks[suggestion_count].0);
        } else if section_count > 0 {
            section_count -= 1;
        } else if discoverable_count > 0 {
            discoverable_count -= 1;
        } else if suggestion_count == 1 {
            suggestion_count = 0;
            continuation = None;
            budget_blocked_by = selected.first().map(|(_, page)| page.id.clone());
        } else {
            bail!(
                "prime metadata and standing instructions exceed the {budget}-token response budget"
            );
        }
    }
}

pub const MAX_READ_EXPAND_DEPTH: usize = 5;
pub const MAX_READ_EXPANDED_PAGES: usize = 100;
const MAX_READ_BROKEN_LINKS: usize = 100;

pub fn read(w: &Wiki, id: &str, expand: usize, json: bool) -> Result<String> {
    if expand > MAX_READ_EXPAND_DEPTH {
        bail!("read expansion depth must not exceed {MAX_READ_EXPAND_DEPTH}");
    }
    let page = w.load_page(id)?;

    let mut linked: Vec<Page> = vec![];
    let mut broken: Vec<String> = vec![];
    let mut linked_omitted = 0_usize;
    let mut linked_omitted_ids = Vec::new();
    let mut broken_omitted = 0_usize;
    if expand > 0 {
        use std::collections::VecDeque;

        let mut visited: HashSet<String> = HashSet::from([id.to_string()]);
        let mut frontier = page
            .links()
            .into_iter()
            .map(|target| (target, 1_usize))
            .collect::<VecDeque<_>>();
        while let Some((target, depth)) = frontier.pop_front() {
            if depth > expand || !visited.insert(target.clone()) {
                continue;
            }
            if !w.exists(&target) {
                if broken.len() < MAX_READ_BROKEN_LINKS {
                    broken.push(retrieval::compact_excerpt(&target));
                } else {
                    broken_omitted += 1;
                }
                continue;
            }
            if linked.len() >= MAX_READ_EXPANDED_PAGES {
                linked_omitted += 1;
                if linked_omitted_ids.len() < 10 {
                    linked_omitted_ids.push(retrieval::compact_excerpt(&target));
                }
                continue;
            }
            let linked_page = w.load_page(&target)?;
            if depth < expand {
                frontier.extend(
                    linked_page
                        .links()
                        .into_iter()
                        .map(|next| (next, depth + 1)),
                );
            }
            linked.push(linked_page);
        }
    }

    if json {
        let linked_omitted_ids_omitted = linked_omitted.saturating_sub(linked_omitted_ids.len());
        return Ok(serde_json::json!({
            "id": page.id, "frontmatter": page.fm, "body": page.body,
            "linked": linked.iter().map(|p| serde_json::json!({
                "id": p.id, "title": retrieval::compact_excerpt(&p.fm.title), "description": retrieval::compact_excerpt(&p.fm.description),
                "summary": retrieval::compact_excerpt(&p.summary()), "stub": p.is_stub(),
            })).collect::<Vec<_>>(),
            "broken_links": broken,
            "broken_links_omitted": broken_omitted,
            "linked_omitted": linked_omitted,
            "linked_omitted_ids": linked_omitted_ids,
            "linked_omitted_ids_omitted": linked_omitted_ids_omitted,
            "continuation": (linked_omitted > 0).then_some("Read omitted page ids directly with `wookie read <id>` or narrow the expansion depth."),
        })
        .to_string());
    }

    let mut out = page.render();
    if expand > 0 && !linked.is_empty() {
        let _ = write!(out, "\n--- Linked context (depth {expand}) ---\n");
        for p in &linked {
            let stub = if p.is_stub() { " [stub]" } else { "" };
            let _ = write!(
                out,
                "\n[[{}]] {} — {}{}\n{}\n",
                p.id,
                p.fm.title,
                retrieval::compact_excerpt(&p.fm.description),
                stub,
                indent(&retrieval::compact_excerpt(&p.summary()))
            );
        }
    }
    if !broken.is_empty() {
        let _ = write!(
            out,
            "\nBroken links: {} (run `wookie expand {id}` to create stubs)\n",
            broken.join(", ")
        );
    }
    if broken_omitted > 0 {
        let _ = writeln!(out, "Broken links omitted: {broken_omitted}.");
    }
    if linked_omitted > 0 {
        let _ = writeln!(
            out,
            "Linked summaries omitted: {linked_omitted} (first omitted: {}). Read them directly with `wookie read <id>` or use a narrower expansion depth.",
            linked_omitted_ids.join(", ")
        );
    }
    Ok(out.trim_end().to_string())
}

#[allow(clippy::too_many_arguments)]
pub fn new_page(
    w: &Wiki,
    id: &str,
    title: Option<String>,
    description: Option<String>,
    tags: Vec<String>,
    sources: Vec<String>,
    pin: Option<PinLevel>,
    protocol_name: Option<&str>,
    body: Option<String>,
    json: bool,
) -> Result<String> {
    let mut effective_id = id.to_string();
    let mut effective_title = title;
    let mut effective_tags = tags;
    let mut effective_body = body;
    if protocol_name.is_some() && effective_body.is_some() {
        bail!("--protocol supplies the page body; do not also pipe a body");
    }
    // Protocol templates and effective section policy share the same writer
    // lock as page/config mutations. Hold one guard from the first such read
    // through the final page write and history update.
    let guard = w.acquire_mutation_guard()?;
    if let Some(name) = protocol_name {
        let rendered = protocol::render(
            &w.dir,
            name,
            protocol::RenderInput {
                id,
                title: effective_title.as_deref(),
                date: None,
            },
        )?;
        validate_protocol_section(w, name, rendered.section.as_deref())?;
        effective_id = rendered.id;
        effective_title = Some(rendered.title);
        effective_tags.extend(rendered.tags);
        effective_tags.sort();
        effective_tags.dedup();
        effective_body = Some(rendered.body);
    }
    let id = effective_id.as_str();
    wiki::validate_id(id)?;
    w.assert_writable(id)?;
    if w.exists(id) {
        bail!("page '{id}' already exists — use `wookie write {id}` to replace its body");
    }
    let has_body = effective_body
        .as_deref()
        .map(|b| !b.trim().is_empty())
        .unwrap_or(false);
    if !has_body && matches!(pin, Some(PinLevel::Instruction | PinLevel::Summary)) {
        bail!("instruction and summary pins require a non-empty page body; discoverable pins may be stubs");
    }
    let title_final = effective_title.unwrap_or_else(|| humanize(id));
    let mut page = Page {
        id: id.to_string(),
        fm: crate::page::Frontmatter {
            title: title_final.clone(),
            description: if has_body {
                String::new()
            } else {
                format!("TODO: describe {id}")
            },
            tags: effective_tags,
            created: today(),
            updated: today(),
            status: if has_body { None } else { Some("stub".into()) },
            sources,
            pin: pin.is_some(),
            pin_level: pin,
            aliases: vec![title_final.clone()],
            extra: vec![],
        },
        body: effective_body
            .filter(|b| !b.trim().is_empty())
            .unwrap_or_else(|| {
                format!(
                    "**TODO: define {title_final}.** Replace this with one bold-lead paragraph \
                 that stands alone as the hover summary; link related pages with [[wikilinks]]."
                )
            }),
    };
    if has_body {
        page.fm.description = first_sentence(&page.summary());
    }
    if let Some(description) = description {
        page.fm.description = description;
    }
    if let Some(issue) = page.standing_text_issue() {
        bail!("cannot create pinned standing page '{id}': it {issue}");
    }
    w.save_page_raw_guarded(&guard, &mut page, false)?;
    w.commit_paths(&format!("wookie: new {id}"), &[format!("pages/{id}.md")])?;

    let filing_note = match section_of(id) {
        Some(s) if w.sections().contains_key(s) => String::new(),
        _ if id == "index" => String::new(),
        _ => format!(
            "\nNote: '{id}' is unfiled. Known sections: {}. Consider `wookie mv` into one (locked sections need user approval + `wookie unlock` first).",
            w.sections().keys().cloned().collect::<Vec<_>>().join(", ")
        ),
    };
    if json {
        return Ok(serde_json::json!({"id": id, "stub": page.is_stub(), "unfiled": !filing_note.is_empty(), "protocol": protocol_name}).to_string());
    }
    if page.is_stub() {
        Ok(format!(
            "Created stub '{id}'. Fill it by piping a body: wookie write {id} <<'EOF' ... EOF{filing_note}"
        ))
    } else {
        Ok(format!("Created page '{id}'.{filing_note}"))
    }
}

fn validate_protocol_section(w: &Wiki, protocol_name: &str, section: Option<&str>) -> Result<()> {
    let Some(section) = section else {
        return Ok(());
    };
    let sections = w.sections();
    if !sections.contains_key(section) {
        bail!(
            "protocol '{protocol_name}' declares section '{section}', which is not configured for wiki '{}'; configured sections: {}",
            w.slug,
            sections.keys().cloned().collect::<Vec<_>>().join(", ")
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub enum PinChange {
    Keep,
    Clear,
    Set(PinLevel),
}

#[allow(clippy::too_many_arguments)]
pub fn write(
    w: &Wiki,
    id: &str,
    body: &str,
    append: bool,
    sources: Option<Vec<String>>,
    pin: PinChange,
    description: Option<String>,
    json: bool,
) -> Result<String> {
    if body.trim().is_empty()
        && sources.is_none()
        && matches!(pin, PinChange::Keep)
        && description.is_none()
    {
        bail!("empty body — pipe page content via stdin (e.g. wookie write {id} <<'EOF' ... EOF)");
    }
    wiki::validate_id(id)?;
    let guard = w.acquire_mutation_guard()?;
    w.assert_writable(id)?;
    let mut page = match w.load_page(id) {
        Ok(p) => p,
        Err(_) => bail!("page '{id}' does not exist — create it with `wookie new {id}`"),
    };
    if append && body.trim().is_empty() {
        bail!("--append requires a non-empty body");
    } else if append {
        page.body = format!("{}\n\n{}", page.body.trim_end(), body.trim());
    } else if !body.trim().is_empty() {
        page.body = body.trim().to_string();
    }
    // Only a real body write clears stub status. Metadata-only updates (for
    // example changing a pin level) must not make an unfilled stub look done.
    if !body.trim().is_empty() {
        page.fm.status = None;
    }
    if let Some(sources) = sources {
        page.fm.sources = sources;
    }
    match pin {
        PinChange::Keep => {}
        PinChange::Clear => {
            page.fm.pin = false;
            page.fm.pin_level = None;
        }
        PinChange::Set(level) => {
            page.fm.pin = true;
            page.fm.pin_level = Some(level);
        }
    }
    if page.fm.description.is_empty() || page.fm.description.starts_with("TODO") {
        page.fm.description = first_sentence(&page.summary());
    }
    if let Some(description) = description {
        page.fm.description = description;
    }
    if let Some(issue) = page.standing_text_issue() {
        bail!("cannot write pinned standing page '{id}': it {issue}");
    }
    w.save_page_raw_guarded(&guard, &mut page, true)?;
    w.commit_paths(&format!("wookie: write {id}"), &[format!("pages/{id}.md")])?;

    let broken: Vec<String> = page.links().into_iter().filter(|l| !w.exists(l)).collect();
    if json {
        return Ok(serde_json::json!({"id": id, "broken_links": broken}).to_string());
    }
    let mut out = format!("Wrote '{id}'.");
    if !broken.is_empty() {
        let _ = write!(
            out,
            "\nBroken links: {} — run `wookie expand {id}` to create stubs for them.",
            broken.join(", ")
        );
    }
    Ok(out)
}

pub fn protocol_list(w: &Wiki, json: bool) -> Result<String> {
    let protocols = protocol::list(&w.dir)?;
    if json {
        return Ok(serde_json::json!({
            "schema": "wookie.protocol-list/v1",
            "wiki": w.slug,
            "protocols": protocols,
        })
        .to_string());
    }
    if protocols.is_empty() {
        return Ok("No protocols. Create one with `wookie protocol write <name>`.".into());
    }
    let mut out = String::new();
    for item in protocols {
        let section = item
            .section
            .as_deref()
            .map(|section| format!(" -> {section}/"))
            .unwrap_or_default();
        let _ = writeln!(out, "{}{} — {}", item.name, section, item.description);
    }
    Ok(out.trim_end().to_string())
}

pub fn protocol_show(w: &Wiki, name: &str, json: bool) -> Result<String> {
    let item = protocol::show(&w.dir, name)?;
    if json {
        return Ok(serde_json::to_string(&item)?);
    }
    let mut out = format!("Protocol: {}\n", item.name);
    if !item.header.description.is_empty() {
        let _ = writeln!(out, "Description: {}", item.header.description);
    }
    if let Some(section) = &item.header.section {
        let _ = writeln!(out, "Section: {section}/");
    }
    if !item.header.tags.is_empty() {
        let _ = writeln!(out, "Tags: {}", item.header.tags.join(", "));
    }
    let _ = write!(out, "\n{}", item.template);
    Ok(out.trim_end().to_string())
}

pub fn protocol_write(w: &Wiki, name: &str, raw: &str, json: bool) -> Result<String> {
    if raw.trim().is_empty() {
        bail!("empty protocol — pipe a Markdown template on stdin");
    }
    let parsed = protocol::parse(name, raw)?;
    // Parsing user input is lock-free; every wiki/config-dependent read and
    // the eventual publication happen under one shared mutation guard.
    let _guard = w.acquire_mutation_guard()?;
    validate_protocol_section(w, name, parsed.header.section.as_deref())?;
    let relative = Path::new("protocols").join(format!("{name}.md"));
    let path = w.contained_path(&relative)?;
    let existed = path.exists();
    if let Some(parent) = relative.parent() {
        wiki::create_contained_dir_all(&w.dir, parent)?;
    }
    wiki::atomic_write(&path, raw)?;
    w.commit_paths(
        &format!("wookie: protocol write {name}"),
        &[relative.to_string_lossy().replace('\\', "/")],
    )?;
    if json {
        return Ok(serde_json::json!({
            "name": name,
            "created": !existed,
            "header": parsed.header,
        })
        .to_string());
    }
    Ok(format!(
        "{} protocol '{name}'.",
        if existed { "Updated" } else { "Created" }
    ))
}

pub fn protocol_remove(w: &Wiki, name: &str, json: bool) -> Result<String> {
    protocol::validate_name(name)?;
    let relative = Path::new("protocols").join(format!("{name}.md"));
    let path = w.contained_path(&relative)?;
    let _guard = w.acquire_mutation_guard()?;
    std::fs::remove_file(&path).with_context(|| format!("no protocol '{name}'"))?;
    w.commit_paths(
        &format!("wookie: protocol remove {name}"),
        &[relative.to_string_lossy().replace('\\', "/")],
    )?;
    if json {
        Ok(serde_json::json!({"removed": name}).to_string())
    } else {
        Ok(format!("Removed protocol '{name}'."))
    }
}

pub fn rm(w: &Wiki, id: &str, json: bool) -> Result<String> {
    let guard = w.acquire_mutation_guard()?;
    w.assert_writable(id)?;
    let backlinks = w.backlinks(id);
    w.delete_page_raw_guarded(&guard, id)?;
    w.commit_paths(&format!("wookie: rm {id}"), &[format!("pages/{id}.md")])?;
    if json {
        return Ok(serde_json::json!({"removed": id, "dangling_backlinks": backlinks}).to_string());
    }
    let mut out = format!("Removed '{id}'.");
    if !backlinks.is_empty() {
        let _ = write!(
            out,
            "\nThese pages still link to it: {} — fix them or run `wookie doctor`.",
            backlinks.join(", ")
        );
    }
    Ok(out)
}

pub fn mv(w: &Wiki, old: &str, new: &str, json: bool) -> Result<String> {
    wiki::validate_id(new)?;
    let guard = w.acquire_mutation_guard()?;
    w.assert_writable(old)?;
    w.assert_writable(new)?;
    if w.exists(new) {
        bail!("page '{new}' already exists");
    }
    let mut page = w.load_page(old)?;
    let mut pending = vec![];
    // Load and prepare every mutation before touching disk. Unlike
    // `all_pages`, this deliberately propagates unreadable-page errors: moving
    // without inspecting a potential backlink could leave it dangling.
    for id in w.page_ids() {
        if id == old {
            continue;
        }
        let other = w
            .load_page(&id)
            .with_context(|| format!("preparing move of '{old}' to '{new}'"))?;
        let (body, changed) = rewrite_links(&other.body, old, new);
        if changed {
            // Rewriting an inbound link is still a content mutation. Rules
            // pages remain absolute: the user must explicitly unlock their
            // section before a move may touch them.
            w.assert_writable(&id)?;
            let mut updated = other.clone();
            updated.body = body;
            pending.push((other, updated));
        }
    }
    // A self-link moves with its page too.
    page.body = rewrite_links(&page.body, old, new).0;
    page.id = new.to_string();
    // Keep the source alive while inbound links are rewritten. At every
    // intermediate point both old and new links therefore resolve.
    w.save_page_raw_guarded(&guard, &mut page, false)?;
    let mut applied = vec![];
    for (original, mut updated) in pending {
        if let Err(error) = w.save_page_raw_guarded(&guard, &mut updated, false) {
            return Err(rollback_page_move(w, &guard, new, &applied, error));
        }
        applied.push(original);
    }
    if let Err(error) = w.delete_page_raw_guarded(&guard, old) {
        return Err(rollback_page_move(w, &guard, new, &applied, error));
    }
    let rewritten: Vec<String> = applied.into_iter().map(|page| page.id).collect();
    let mut history_paths = vec![format!("pages/{old}.md"), format!("pages/{new}.md")];
    history_paths.extend(rewritten.iter().map(|id| format!("pages/{id}.md")));
    w.commit_paths(&format!("wookie: mv {old} -> {new}"), &history_paths)?;
    if json {
        return Ok(serde_json::json!({"from": old, "to": new, "rewrote": rewritten}).to_string());
    }
    Ok(format!(
        "Moved '{old}' -> '{new}'. Rewrote links in {} page(s){}",
        rewritten.len(),
        if rewritten.is_empty() {
            ".".to_string()
        } else {
            format!(": {}", rewritten.join(", "))
        }
    ))
}

/// Restore backlink pages after a failed move. The destination is removed
/// only if every backlink was restored; otherwise both IDs remain present so
/// neither the old nor partially rewritten links can be broken.
fn rollback_page_move(
    w: &Wiki,
    guard: &crate::publish::MutationGuard,
    new: &str,
    applied: &[Page],
    cause: anyhow::Error,
) -> anyhow::Error {
    let mut rollback_errors = vec![];
    for original in applied.iter().rev() {
        let mut original = original.clone();
        if let Err(error) = w.save_page_raw_guarded(guard, &mut original, false) {
            rollback_errors.push(format!("restoring '{}': {error:#}", original.id));
        }
    }
    if rollback_errors.is_empty() {
        if let Err(error) = w.delete_page_raw_guarded(guard, new) {
            rollback_errors.push(format!("removing temporary destination '{new}': {error:#}"));
        }
    }

    if rollback_errors.is_empty() {
        cause.context("page move failed; all completed changes were rolled back")
    } else {
        anyhow::anyhow!(
            "page move failed: {cause:#}. Rollback was incomplete: {}. Both page IDs were kept where necessary so rewritten links still resolve",
            rollback_errors.join("; ")
        )
    }
}

/// A mutating command needs enough room to state whether all writes completed,
/// even when the caller asks for a very small projection.
pub const MIN_EXPAND_TOKENS: usize = 256;

#[derive(Debug, Clone, Copy)]
pub struct ExpandOptions<'a> {
    pub id: Option<&'a str>,
    pub limit: Option<usize>,
    pub tokens: Option<usize>,
    pub all: bool,
}

#[derive(Debug, Clone, Copy)]
struct ExpandTotals {
    created: usize,
    stubs: usize,
    skipped_locked: usize,
}

fn expand_continuation(id: Option<&str>) -> serde_json::Value {
    let command = id
        .map(|id| format!("wookie expand {id} --all"))
        .unwrap_or_else(|| "wookie expand --all".to_string());
    serde_json::json!({
        "command": command,
        "read": "wookie read <id> --expand",
        "guidance": "The mutation is complete. Use the exhaustive command to list every current stub, then read individual pages on demand."
    })
}

fn expand_output_tokens(output: &str) -> usize {
    // CLI output adds one trailing newline; reserving it also makes the MCP
    // payload estimate conservative by one byte.
    retrieval::estimate_tokens(&format!("{output}\n"))
}

#[allow(clippy::too_many_arguments)]
fn expand_report_json(
    snapshot: report::Snapshot,
    created: &[String],
    stubs: &[String],
    skipped_locked: &[String],
    totals: ExpandTotals,
    budget: Option<usize>,
    limit: Option<usize>,
    all: bool,
    id: Option<&str>,
) -> Result<String> {
    let omitted_created = totals.created.saturating_sub(created.len());
    let omitted_stubs = totals.stubs.saturating_sub(stubs.len());
    let omitted_locked = totals.skipped_locked.saturating_sub(skipped_locked.len());
    let omitted = omitted_created
        .saturating_add(omitted_stubs)
        .saturating_add(omitted_locked);
    let omissions = serde_json::json!({
        "created": omitted_created,
        "stubs": omitted_stubs,
        "skipped_locked": omitted_locked,
    });
    let diagnostics = if totals.skipped_locked == 0 {
        vec![]
    } else {
        vec![report::Diagnostic::new(
            report::code::RULE_LOCKED,
            report::Severity::Warning,
            format!(
                "{} broken link target(s) were skipped because their rules sections are locked",
                totals.skipped_locked
            ),
        )
        .suggestion("Ask the user before unlocking a rules section")
        .data("total", totals.skipped_locked)]
    };
    let mut report = report::Report::with_diagnostics("expand", snapshot, diagnostics);
    report.insert_data("created", serde_json::json!(created));
    report.insert_data("stubs", serde_json::json!(stubs));
    report.insert_data("skipped_locked", serde_json::json!(skipped_locked));
    report.insert_data(
        "totals",
        serde_json::json!({
            "created": totals.created,
            "stubs": totals.stubs,
            "skipped_locked": totals.skipped_locked,
        }),
    );
    report.insert_data("omissions", omissions.clone());

    let mut value = serde_json::to_value(report)?;
    // Preserve the original command-level fields while adding bounded-output
    // metadata. Report consumers can equivalently use `data`.
    value["created"] = serde_json::json!(created);
    value["stubs"] = serde_json::json!(stubs);
    value["skipped_locked"] = serde_json::json!(skipped_locked);
    value["totals"] = serde_json::json!({
        "created": totals.created,
        "stubs": totals.stubs,
        "skipped_locked": totals.skipped_locked,
    });
    value["omissions"] = omissions;
    value["continuation"] = if omitted > 0 {
        expand_continuation(id)
    } else {
        serde_json::Value::Null
    };
    value["telemetry"] = serde_json::json!({
        "estimated_tokens": 0,
        "budget_tokens": budget,
        "limit_per_category": limit,
        "all": all,
    });

    // The estimate field contributes to its own serialized size. A few fixed
    // point iterations make the advertised number match the final payload.
    for _ in 0..3 {
        let rendered = serde_json::to_string(&value)?;
        value["telemetry"]["estimated_tokens"] = serde_json::json!(expand_output_tokens(&rendered));
    }
    Ok(serde_json::to_string(&value)?)
}

#[allow(clippy::too_many_arguments)]
fn expand_human_output(
    created: &[String],
    stubs: &[String],
    skipped_locked: &[String],
    missing: &BTreeMap<String, Vec<String>>,
    totals: ExpandTotals,
    id: Option<&str>,
    budget: Option<usize>,
    limit: Option<usize>,
    all: bool,
) -> String {
    let mut out = String::new();
    if totals.skipped_locked > 0 {
        let _ = writeln!(
            out,
            "Skipped {} broken link(s) into locked sections (ask the user before unlocking).",
            totals.skipped_locked
        );
        for target in skipped_locked {
            let _ = writeln!(out, "- {target}");
        }
    }
    if totals.created == 0 {
        if totals.skipped_locked == 0 {
            let _ = writeln!(out, "No broken links found — nothing to stub.");
        } else {
            let _ = writeln!(out, "Created no stubs; every missing target was locked.");
        }
    } else {
        let _ = writeln!(out, "Created {} stub page(s):", totals.created);
        for target in created {
            let sources = &missing[target];
            if all {
                let _ = writeln!(out, "- {target}  (linked from {})", sources.join(", "));
            } else {
                let _ = writeln!(out, "- {target}  (linked from {} page(s))", sources.len());
            }
        }
    }
    if totals.stubs == 0 {
        let _ = writeln!(out, "No stubs waiting for content.");
    } else {
        let _ = writeln!(out, "\nStubs needing content ({}):", totals.stubs);
        for stub in stubs {
            let _ = writeln!(out, "- {stub}");
        }
    }

    let omitted_created = totals.created.saturating_sub(created.len());
    let omitted_stubs = totals.stubs.saturating_sub(stubs.len());
    let omitted_locked = totals.skipped_locked.saturating_sub(skipped_locked.len());
    if omitted_created
        .saturating_add(omitted_stubs)
        .saturating_add(omitted_locked)
        > 0
    {
        let continuation = id
            .map(|id| format!("wookie expand {id} --all"))
            .unwrap_or_else(|| "wookie expand --all".to_string());
        let _ = writeln!(
            out,
            "\nOmitted: {omitted_created} created ID(s), {omitted_stubs} current stub ID(s), {omitted_locked} locked target ID(s)."
        );
        let _ = writeln!(
            out,
            "Mutation complete. Continue with `{continuation}` for the exhaustive current worklist."
        );
    }
    if totals.stubs > 0 {
        let _ = writeln!(
            out,
            "Read one on demand with `wookie read <id> --expand`; writing real content clears stub status."
        );
    }
    if let Some(budget) = budget {
        let mut estimate = 0;
        for _ in 0..3 {
            let candidate = format!(
                "{out}Telemetry: returned at most {} ID(s) per category; estimated {estimate} / {budget} tokens.\n",
                limit.unwrap_or(0)
            );
            estimate = retrieval::estimate_tokens(&candidate);
        }
        let _ = writeln!(
            out,
            "Telemetry: returned at most {} ID(s) per category; estimated {estimate} / {budget} tokens.",
            limit.unwrap_or(0)
        );
    }
    out.trim_end().to_string()
}

pub fn expand(w: &Wiki, options: &ExpandOptions<'_>, json: bool) -> Result<String> {
    if options.all && (options.limit.is_some() || options.tokens.is_some()) {
        bail!("--all is mutually exclusive with --limit and --tokens");
    }
    let limit = options.limit.unwrap_or(w.retrieval.search_limit);
    if limit == 0 {
        bail!("expand limit must be greater than zero");
    }
    if limit > MAX_SEARCH_LIMIT {
        bail!("expand limit must not exceed {MAX_SEARCH_LIMIT}");
    }
    let budget = options.tokens.unwrap_or(w.retrieval.search_tokens);
    if !options.all && budget < MIN_EXPAND_TOKENS {
        bail!("expand token budget must be at least {MIN_EXPAND_TOKENS}");
    }
    if !options.all && budget > MAX_RETRIEVAL_TOKENS {
        bail!("expand token budget must not exceed {MAX_RETRIEVAL_TOKENS}");
    }

    let guard = w.acquire_mutation_guard()?;
    let catalog = w.all_pages();
    let pages: Vec<Page> = match options.id {
        Some(id) => vec![w.load_page(id)?],
        None => catalog.clone(),
    };

    // target -> pages that link to it
    let mut missing: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for p in &pages {
        for target in p.links() {
            if wiki::validate_id(&target).is_ok() && !w.exists(&target) {
                missing.entry(target).or_default().push(p.id.clone());
            }
        }
    }

    let mut eligible = vec![];
    let mut skipped_locked = vec![];
    for target in missing.keys() {
        if w.assert_writable(target).is_err() {
            skipped_locked.push(target.clone());
            continue;
        }
        eligible.push(target.clone());
    }

    let predicted_totals = ExpandTotals {
        created: eligible.len(),
        stubs: catalog.iter().filter(|page| page.is_stub()).count() + eligible.len(),
        skipped_locked: skipped_locked.len(),
    };
    if !options.all {
        // Prove before writing that even an empty-ID projection can report the
        // mutation outcome within the requested budget.
        let predicted_snapshot = report::Snapshot::new(&w.slug)
            .wiki_content_hash("0".repeat(64))
            .wiki_revision("0".repeat(128));
        let baseline = if json {
            expand_report_json(
                predicted_snapshot,
                &[],
                &[],
                &[],
                predicted_totals,
                Some(budget),
                Some(limit),
                false,
                options.id,
            )?
        } else {
            expand_human_output(
                &[],
                &[],
                &[],
                &missing,
                predicted_totals,
                options.id,
                Some(budget),
                Some(limit),
                false,
            )
        };
        let required = expand_output_tokens(&baseline);
        if required > budget {
            bail!(
                "expand summary needs about {required} tokens; raise --tokens before retrying (no stubs were created)"
            );
        }
    }

    let mut created = vec![];
    for target in &eligible {
        let sources = &missing[target];
        let mut stub = Page {
            id: target.clone(),
            fm: crate::page::Frontmatter {
                title: humanize(target),
                description: format!("TODO: describe {target}"),
                tags: vec![],
                created: today(),
                updated: today(),
                status: Some("stub".into()),
                sources: vec![],
                pin: false,
                pin_level: None,
                aliases: vec![humanize(target)],
                extra: vec![],
            },
            body: format!(
                "**TODO: define {}.** Replace this with one bold-lead paragraph that \
                 stands alone as the hover summary; link related pages with `[[wikilinks]]`.\n\n\
                 > [!note] Linked from: {}.",
                humanize(target),
                sources
                    .iter()
                    .map(|s| format!("[[{s}]]"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        w.save_page_raw_guarded(&guard, &mut stub, false)?;
        created.push(target.clone());
    }
    if !created.is_empty() {
        let paths = created
            .iter()
            .map(|id| format!("pages/{id}.md"))
            .collect::<Vec<_>>();
        w.commit_paths(&format!("wookie: expand ({} stubs)", created.len()), &paths)?;
    }

    let stubs: Vec<String> = w
        .all_pages()
        .iter()
        .filter(|p| p.is_stub())
        .map(|p| p.id.clone())
        .collect();

    let totals = ExpandTotals {
        created: created.len(),
        stubs: stubs.len(),
        skipped_locked: skipped_locked.len(),
    };
    let mut selected_created = if options.all {
        created.clone()
    } else {
        created.iter().take(limit).cloned().collect()
    };
    let mut selected_stubs = if options.all {
        stubs.clone()
    } else {
        stubs.iter().take(limit).cloned().collect()
    };
    let mut selected_locked = if options.all {
        skipped_locked.clone()
    } else {
        skipped_locked.iter().take(limit).cloned().collect()
    };

    if json {
        let mut snapshot =
            report::Snapshot::new(&w.slug).wiki_content_hash(snapshot::wiki_content_hash(w)?);
        if let Some(revision) = wiki_revision(w) {
            snapshot = snapshot.wiki_revision(revision);
        }
        loop {
            let rendered = expand_report_json(
                snapshot.clone(),
                &selected_created,
                &selected_stubs,
                &selected_locked,
                totals,
                (!options.all).then_some(budget),
                (!options.all).then_some(limit),
                options.all,
                options.id,
            )?;
            if options.all || expand_output_tokens(&rendered) <= budget {
                return Ok(rendered);
            }
            if !selected_stubs.is_empty() {
                selected_stubs.pop();
            } else if !selected_created.is_empty() {
                selected_created.pop();
            } else if !selected_locked.is_empty() {
                selected_locked.pop();
            } else {
                // The same empty projection was checked before mutation, so
                // this can only indicate an internal accounting regression.
                bail!("expand response exceeded its prevalidated token budget");
            }
        }
    }

    loop {
        let rendered = expand_human_output(
            &selected_created,
            &selected_stubs,
            &selected_locked,
            &missing,
            totals,
            options.id,
            (!options.all).then_some(budget),
            (!options.all).then_some(limit),
            options.all,
        );
        if options.all || expand_output_tokens(&rendered) <= budget {
            return Ok(rendered);
        }
        if !selected_stubs.is_empty() {
            selected_stubs.pop();
        } else if !selected_created.is_empty() {
            selected_created.pop();
        } else if !selected_locked.is_empty() {
            selected_locked.pop();
        } else {
            bail!("expand response exceeded its prevalidated token budget");
        }
    }
}

#[derive(Debug, Clone)]
pub struct SearchOptions {
    pub query: String,
    pub tag: Option<String>,
    pub limit: Option<usize>,
    pub tokens: Option<usize>,
    pub excerpt_lines: Option<usize>,
    pub cursor: usize,
    /// Exact retrieval-state hash returned by the previous window.
    pub context_hash: Option<String>,
    pub regex: bool,
    pub all: bool,
    /// Invocation directory used to select the active registered worktree.
    pub cwd: Option<PathBuf>,
}

fn search_context_hash(
    catalog_content_hash: &str,
    options: &SearchOptions,
    limit: usize,
    budget: usize,
    excerpt_lines: usize,
    freshness: &retrieval::FreshnessOutcome,
) -> String {
    let mut hash = Sha256::new();
    hash_field(&mut hash, b"wookie.search-context/v1");
    for field in [
        catalog_content_hash,
        options.query.as_str(),
        options.tag.as_deref().unwrap_or(""),
    ] {
        hash_field(&mut hash, field.as_bytes());
    }
    hash_field(&mut hash, &[u8::from(options.regex)]);
    for value in [limit, budget, excerpt_lines] {
        hash_field(
            &mut hash,
            &u64::try_from(value).unwrap_or(u64::MAX).to_be_bytes(),
        );
    }
    hash_field(
        &mut hash,
        serde_json::to_string(freshness)
            .unwrap_or_default()
            .as_bytes(),
    );
    format!("sha256:{:x}", hash.finalize())
}

fn search_continuation_argv(
    options: &SearchOptions,
    context_hash: &str,
    cursor: usize,
    limit: usize,
    budget: usize,
    excerpt_lines: usize,
) -> Vec<String> {
    let mut argv = vec![
        "wookie".to_string(),
        "search".to_string(),
        options.query.clone(),
    ];
    if let Some(tag) = &options.tag {
        argv.extend(["--tag".to_string(), tag.clone()]);
    }
    argv.extend([
        "--limit".to_string(),
        limit.to_string(),
        "--tokens".to_string(),
        budget.to_string(),
        "--excerpt-lines".to_string(),
        excerpt_lines.to_string(),
    ]);
    if options.regex {
        argv.push("--regex".to_string());
    }
    argv.extend([
        "--context-hash".to_string(),
        context_hash.to_string(),
        "--cursor".to_string(),
        cursor.to_string(),
    ]);
    argv
}

pub fn search_with_options(w: &Wiki, options: &SearchOptions, json: bool) -> Result<String> {
    let started = std::time::Instant::now();
    retrieval::validate_query(&options.query)?;
    if options.limit.is_some_and(|limit| limit > MAX_SEARCH_LIMIT) {
        bail!("search limit must not exceed {MAX_SEARCH_LIMIT}");
    }
    if options
        .tokens
        .is_some_and(|tokens| tokens > MAX_RETRIEVAL_TOKENS)
    {
        bail!("search token budget must not exceed {MAX_RETRIEVAL_TOKENS}");
    }
    if options
        .excerpt_lines
        .is_some_and(|lines| lines > MAX_EXCERPT_LINES)
    {
        bail!("search excerpt-lines must not exceed {MAX_EXCERPT_LINES}");
    }
    let limit = options.limit.unwrap_or(w.retrieval.search_limit);
    let budget = options.tokens.unwrap_or(w.retrieval.search_tokens);
    let excerpt_lines = options.excerpt_lines.unwrap_or(w.retrieval.excerpt_lines);
    if limit == 0 || budget == 0 || excerpt_lines == 0 {
        bail!("search limit, token budget, and excerpt-lines must be greater than zero");
    }
    if budget > MAX_RETRIEVAL_TOKENS {
        bail!("search token budget must not exceed {MAX_RETRIEVAL_TOKENS}");
    }
    if limit > MAX_SEARCH_LIMIT || excerpt_lines > MAX_EXCERPT_LINES {
        bail!(
            "search limit must not exceed {MAX_SEARCH_LIMIT} and excerpt-lines must not exceed {MAX_EXCERPT_LINES}"
        );
    }
    let catalog = retrieval_index::load(w)?;
    if options.all {
        return search_exhaustive(
            &catalog.pages,
            &catalog.cache,
            &options.query,
            options.tag.as_deref(),
            json,
            started,
        );
    }
    let all_pages = catalog.pages;
    let freshness = project_freshness(w, &all_pages, options.cwd.as_deref());
    let context_hash = search_context_hash(
        &catalog.content_hash,
        options,
        limit,
        budget,
        excerpt_lines,
        &freshness,
    );
    if options.cursor > 0 && options.context_hash.as_deref() != Some(context_hash.as_str()) {
        bail!(
            "search cursor {} is not bound to the current query and catalog; restart at cursor 0 or pass --context-hash {context_hash}",
            options.cursor
        );
    }
    let pages: Vec<Page> = all_pages
        .into_iter()
        .filter(|page| {
            options
                .tag
                .as_deref()
                .is_none_or(|tag| page.fm.tags.iter().any(|value| value == tag))
        })
        .collect();
    let projections: Vec<_> = pages
        .iter()
        .map(|page| retrieval::RetrievalPage::from_page(page, freshness.is_stale(&page.id)))
        .collect();
    let ranked = if options.regex {
        let re = regex::Regex::new(&format!("(?i){}", options.query))?;
        let mut matches: Vec<_> = pages
            .iter()
            .filter_map(|page| {
                let metadata = format!(
                    "{}\n{}\n{}\n{}\n{}",
                    page.id,
                    page.fm.title,
                    page.fm.description,
                    page.fm.tags.join(" "),
                    page.fm.sources.join(" ")
                );
                let body_lines = page.body.lines().filter(|line| re.is_match(line)).count();
                let meta = re.is_match(&metadata);
                (meta || body_lines > 0).then(|| retrieval::RankedPage {
                    id: page.id.clone(),
                    title: page.fm.title.clone(),
                    description: page.fm.description.clone(),
                    score: u32::try_from(body_lines).unwrap_or(u32::MAX)
                        + if meta { 100 } else { 0 },
                    reasons: vec![],
                    excerpt: page
                        .body
                        .lines()
                        .find(|line| re.is_match(line))
                        .map(|line| line.trim().to_string()),
                    stale: freshness.is_stale(&page.id),
                })
            })
            .collect();
        matches.sort_by(|left, right| right.score.cmp(&left.score).then(left.id.cmp(&right.id)));
        matches
    } else {
        retrieval::rank_pages(&options.query, &projections)
    };
    let selection = retrieval::select_ranked(
        &ranked,
        pages.len(),
        &options.query,
        retrieval::SelectionOptions {
            token_budget: budget,
            limit,
            offset: options.cursor,
        },
    )?;
    let line_matcher = if options.regex {
        regex::Regex::new(&format!("(?i){}", options.query))?
    } else {
        let terms = retrieval::query_terms(&options.query);
        let pattern = if terms.is_empty() {
            regex::escape(&options.query)
        } else {
            terms
                .iter()
                .map(|term| regex::escape(term))
                .collect::<Vec<_>>()
                .join("|")
        };
        regex::Regex::new(&format!("(?i)(?:{pattern})"))?
    };
    let expanded: Vec<_> = selection
        .results
        .iter()
        .map(|result| {
            let excerpts = pages
                .iter()
                .find(|page| page.id == result.id)
                .map(|page| {
                    page.body
                        .lines()
                        .enumerate()
                        .filter(|(_, line)| line_matcher.is_match(line))
                        .take(excerpt_lines)
                        .map(|(index, line)| {
                            serde_json::json!({
                                "line": index + 1,
                                "text": retrieval::compact_excerpt(line.trim())
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            serde_json::json!({
                "id": result.id,
                "title": result.title,
                "description": result.description,
                "score": result.score,
                "reasons": result.reasons,
                "stale": result.stale,
                "matches": excerpts,
            })
        })
        .collect::<Vec<_>>();
    if json {
        let mut hits = expanded.clone();
        let mut continuation = selection.next_offset;
        let mut budget_blocked_by = selection.budget_blocked_by.clone();
        loop {
            let pages_omitted = ranked
                .len()
                .saturating_sub(options.cursor.min(ranked.len()).saturating_add(hits.len()));
            let mut value = serde_json::json!({
                "schema": "wookie.search/v1",
                "query": options.query,
                "mode": if options.regex { "regex" } else { "ranked" },
                "context_hash": context_hash,
                "hits": hits,
                "continuation": continuation,
                "continuation_argv": continuation.map(|cursor| search_continuation_argv(options, &context_hash, cursor, limit, budget, excerpt_lines)),
                "budget_blocked_by": budget_blocked_by,
                "next_command": budget_blocked_by.as_deref().map(|id| format!("wookie read {id}")),
                "telemetry": {
                    "pages_considered": pages.len(),
                    "pages_matched": ranked.len(),
                    "pages_returned": hits.len(),
                    "pages_omitted": pages_omitted,
                    "estimated_tokens": 0,
                    "budget_tokens": budget,
                    "limit": limit,
                    "query_terms": retrieval::query_terms(&options.query).len(),
                    "retrieval_ms": started.elapsed().as_millis(),
                    "cache": &catalog.cache
                }
            });
            let initial = serde_json::to_string(&value)?;
            value["telemetry"]["estimated_tokens"] =
                serde_json::json!(retrieval::estimate_tokens(&initial));
            let rendered = serde_json::to_string(&value)?;
            if retrieval::estimate_tokens(&rendered) <= budget {
                return Ok(rendered);
            }
            if hits.is_empty() {
                bail!("search metadata exceeds the {budget}-token response budget");
            }
            let dropped_index = options.cursor.saturating_add(hits.len().saturating_sub(1));
            let dropped_id = hits
                .pop()
                .and_then(|hit| hit["id"].as_str().map(str::to_string));
            if hits.is_empty() {
                budget_blocked_by = dropped_id;
                continuation = None;
            } else {
                continuation = Some(dropped_index);
            }
        }
    }
    if expanded.is_empty() {
        if let Some(id) = selection.budget_blocked_by {
            return Ok(format!(
                "The highest-ranked result '{id}' cannot fit the {budget}-token budget; run `wookie read {id}` or raise --tokens."
            ));
        }
        return Ok(format!("No pages match '{}'.", options.query));
    }
    let prefix = format!(
        "Search: {} ({} mode)\n",
        options.query,
        if options.regex { "regex" } else { "ranked" }
    );
    let mut chunks = Vec::new();
    for hit in &expanded {
        let reasons = hit["reasons"]
            .as_array()
            .map(|_| {
                selection
                    .results
                    .iter()
                    .find(|result| result.id == hit["id"].as_str().unwrap_or_default())
                    .map(reason_text)
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        let mut chunk = format!(
            "{} — {} [score {}]{}\n  Match: {}\n",
            hit["id"].as_str().unwrap_or_default(),
            hit["description"].as_str().unwrap_or_default(),
            hit["score"],
            if hit["stale"].as_bool().unwrap_or(false) {
                " [stale]"
            } else {
                ""
            },
            if reasons.is_empty() {
                "regex"
            } else {
                &reasons
            },
        );
        for excerpt in hit["matches"].as_array().into_iter().flatten() {
            let _ = writeln!(
                chunk,
                "  {}: {}",
                excerpt["line"],
                excerpt["text"].as_str().unwrap_or_default()
            );
        }
        chunks.push(chunk);
    }
    let mut returned = chunks.len();
    let mut continuation = selection.next_offset;
    loop {
        let mut out = prefix.clone();
        for chunk in chunks.iter().take(returned) {
            out.push_str(chunk);
        }
        if returned == 0 {
            let id = expanded[0]["id"].as_str().unwrap_or("<id>");
            let _ = writeln!(
                out,
                "Highest-ranked result is too large for this response; run `wookie read {id}`."
            );
        } else if let Some(cursor) = continuation {
            let _ = writeln!(
                out,
                "Continuation: reuse the same query/options and context hash {context_hash} with cursor {cursor}."
            );
        }
        let omitted = ranked
            .len()
            .saturating_sub(options.cursor.min(ranked.len()).saturating_add(returned));
        let telemetry_prefix = format!(
            "Telemetry: considered {}, matched {}, returned {}, omitted {}, retrieval {} ms, cache {} ({} reused, {} refreshed); estimated ",
            pages.len(),
            ranked.len(),
            returned,
            omitted,
            started.elapsed().as_millis(),
            catalog.cache.state,
            catalog.cache.pages_reused,
            catalog.cache.pages_refreshed,
        );
        let provisional = format!("{out}{telemetry_prefix}0 / {budget} tokens.\n");
        let estimate = retrieval::estimate_tokens(&provisional);
        let mut rendered = format!("{out}{telemetry_prefix}{estimate} / {budget} tokens.\n");
        let final_estimate = retrieval::estimate_tokens(&rendered);
        if final_estimate != estimate {
            rendered = format!("{out}{telemetry_prefix}{final_estimate} / {budget} tokens.\n");
        }
        if retrieval::estimate_tokens(&rendered) <= budget {
            return Ok(rendered.trim_end().to_string());
        }
        if returned == 0 {
            bail!("search metadata exceeds the {budget}-token response budget");
        }
        returned -= 1;
        continuation = Some(options.cursor.saturating_add(returned));
    }
}

fn search_exhaustive(
    pages: &[Page],
    cache: &retrieval_index::CacheTelemetry,
    query: &str,
    tag: Option<&str>,
    json: bool,
    started: std::time::Instant,
) -> Result<String> {
    let re = regex::Regex::new(&format!("(?i){query}"))
        .or_else(|_| regex::Regex::new(&format!("(?i){}", regex::escape(query))))?;

    let mut hits = vec![];
    for p in pages {
        if let Some(tag) = tag {
            if !p.fm.tags.iter().any(|t| t == tag) {
                continue;
            }
        }
        let meta_hit =
            re.is_match(&p.id) || re.is_match(&p.fm.title) || re.is_match(&p.fm.description);
        let mut lines = vec![];
        for (n, line) in p.body.lines().enumerate() {
            if re.is_match(line) {
                lines.push((n + 1, line.trim().to_string()));
                if lines.len() >= 5 {
                    break;
                }
            }
        }
        if meta_hit || !lines.is_empty() {
            hits.push((p.id.clone(), p.fm.description.clone(), lines));
        }
    }

    if json {
        let items: Vec<_> = hits
            .iter()
            .map(|(id, desc, lines)| {
                serde_json::json!({
                    "id": id, "description": desc,
                    "matches": lines.iter().map(|(n, l)| serde_json::json!({"line": n, "text": l})).collect::<Vec<_>>(),
                })
            })
            .collect();
        return Ok(serde_json::json!({
            "query": query,
            "hits": items,
            "cache": cache,
            "telemetry": {"retrieval_ms": started.elapsed().as_millis(), "cache": cache}
        })
        .to_string());
    }
    if hits.is_empty() {
        return Ok(format!("No pages match '{query}'."));
    }
    let mut out = String::new();
    for (id, desc, lines) in &hits {
        let _ = writeln!(out, "{id} — {desc}");
        for (n, line) in lines {
            let _ = writeln!(out, "  {n}: {line}");
        }
    }
    let _ = writeln!(
        out,
        "Cache: {} ({} reused, {} refreshed); retrieval {} ms",
        cache.state,
        cache.pages_reused,
        cache.pages_refreshed,
        started.elapsed().as_millis()
    );
    Ok(out.trim_end().to_string())
}

pub fn links(w: &Wiki, id: &str, json: bool) -> Result<String> {
    let page = w.load_page(id)?;
    let out_links: Vec<(String, bool)> = page
        .links()
        .into_iter()
        .map(|l| {
            let ok = w.exists(&l);
            (l, ok)
        })
        .collect();
    let backlinks = w.backlinks(id);

    if json {
        return Ok(serde_json::json!({
            "id": id,
            "outlinks": out_links.iter().map(|(l, ok)| serde_json::json!({"id": l, "exists": ok})).collect::<Vec<_>>(),
            "backlinks": backlinks,
        })
        .to_string());
    }
    let mut out = String::new();
    let _ = writeln!(out, "Outlinks from {id}:");
    if out_links.is_empty() {
        let _ = writeln!(out, "  (none)");
    }
    for (l, ok) in &out_links {
        let _ = writeln!(out, "  [[{l}]]{}", if *ok { "" } else { "  BROKEN" });
    }
    let _ = writeln!(out, "Backlinks to {id}:");
    if backlinks.is_empty() {
        let _ = writeln!(out, "  (none)");
    }
    for b in &backlinks {
        let _ = writeln!(out, "  [[{b}]]");
    }
    Ok(out.trim_end().to_string())
}

#[derive(Clone, Copy, Debug, PartialEq, clap::ValueEnum)]
pub enum IngestLevel {
    /// Index + architecture overview + one page per top-level module
    Quick,
    /// Quick + significant submodules + key flows and concepts
    Standard,
    /// Standard + per-file/type pages, invariants, full cross-linking
    Deep,
}

#[derive(Clone, Copy, Debug, PartialEq, clap::ValueEnum)]
pub enum IngestRecoveryAction {
    /// Accept the recorded target config after verifying project state
    Accept,
    /// Restore the pre-mark config, adding an exact compensating commit if needed
    Rollback,
}

const MAX_INGEST_PATHS: usize = 200_000;
const MAX_INGEST_PATH_BYTES: usize = 32 * 1024 * 1024;

fn validate_ingest_inventory(paths: Vec<String>, label: &str) -> Result<Vec<String>> {
    if paths.len() > MAX_INGEST_PATHS {
        bail!("{label} exceeds the {MAX_INGEST_PATHS}-path ingest safety limit");
    }
    let bytes = paths.iter().try_fold(0_usize, |total, path| {
        total
            .checked_add(path.len())
            .and_then(|value| value.checked_add(1))
            .context("ingest path inventory size overflow")
    })?;
    if bytes > MAX_INGEST_PATH_BYTES {
        bail!("{label} exceeds the {MAX_INGEST_PATH_BYTES}-byte ingest safety limit");
    }
    Ok(paths)
}

fn bounded_git_path_output(root: &Path, args: &[&str], label: &str) -> Result<Vec<u8>> {
    use std::io::Read as _;
    use std::process::Stdio;

    let mut child = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .stdout(Stdio::piped())
        // Inventory errors are summarized below. Avoid a malicious Git
        // configuration filling a second unbounded pipe while stdout is read.
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("starting {label} in {}", root.display()))?;
    let mut stdout = Vec::new();
    child
        .stdout
        .take()
        .context("Git inventory stdout pipe is unavailable")?
        .take(u64::try_from(MAX_INGEST_PATH_BYTES).unwrap_or(u64::MAX) + 1)
        .read_to_end(&mut stdout)?;
    if stdout.len() > MAX_INGEST_PATH_BYTES {
        let _ = child.kill();
        let _ = child.wait();
        bail!("{label} exceeds the {MAX_INGEST_PATH_BYTES}-byte ingest safety limit");
    }
    let status = child.wait()?;
    if !status.success() {
        bail!("{label} failed in {}", root.display());
    }
    Ok(stdout)
}

fn head_commit(root: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn changed_since(root: &Path, since: &str) -> Result<Vec<String>> {
    let resolved = resolve_git_commit(root, since, "ingest base revision")?;
    // Compare the base to the current index and working tree, not merely to
    // HEAD, so staged and unstaged edits cannot be called "in sync".
    let diff = bounded_git_path_output(
        root,
        &[
            "diff",
            "--name-status",
            "-z",
            "--find-renames",
            "--find-copies",
            &resolved,
            "--",
        ],
        "Git changed-path inventory",
    )?;
    let mut changed = crate::git_paths::parse_name_status(&diff, "git diff name-status output")?;
    let untracked = bounded_git_path_output(
        root,
        &["ls-files", "-z", "--others", "--exclude-standard"],
        "Git untracked-path inventory",
    )?;
    changed.extend(crate::git_paths::parse_path_list(
        &untracked,
        "git ls-files output",
    )?);
    changed.sort();
    changed.dedup();
    validate_ingest_inventory(changed, "changed-path inventory")
}

/// Project files, relative paths. git ls-files when available (respects
/// .gitignore), else a walk that skips hidden and well-known junk dirs.
fn list_project_files(root: &Path) -> Result<Vec<String>> {
    let git_probe = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--is-inside-work-tree"])
        .output();
    let is_git_worktree = git_probe
        .as_ref()
        .ok()
        .is_some_and(|output| output.status.success() && output.stdout == b"true\n");
    match bounded_git_path_output(
        root,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
        "Git project inventory",
    ) {
        Ok(output) => {
            let files =
                crate::git_paths::parse_path_list(&output, "git ls-files project inventory")?;
            if !files.is_empty() {
                return validate_ingest_inventory(files, "project inventory");
            }
        }
        Err(error) if is_git_worktree => return Err(error),
        Err(_) => {}
    }
    const JUNK: &[&str] = &[
        "node_modules",
        "target",
        "dist",
        "build",
        "__pycache__",
        "venv",
        ".venv",
        "vendor",
    ];
    let mut files = Vec::new();
    let mut path_bytes = 0_usize;
    for entry in walkdir::WalkDir::new(root).into_iter().filter_entry(|e| {
        let name = e.file_name().to_string_lossy();
        e.depth() == 0 || (!name.starts_with('.') && !JUNK.contains(&name.as_ref()))
    }) {
        let entry =
            entry.with_context(|| format!("walking project inventory under {}", root.display()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry
            .path()
            .strip_prefix(root)
            .with_context(|| format!("project inventory path escaped {}", root.display()))?;
        let path = path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("project path is not valid UTF-8"))?
            .replace('\\', "/");
        let path = crate::git_paths::validate_path(&path, "project inventory")?;
        path_bytes = path_bytes
            .checked_add(path.len() + 1)
            .context("ingest path inventory size overflow")?;
        files.push(path);
        if files.len() > MAX_INGEST_PATHS || path_bytes > MAX_INGEST_PATH_BYTES {
            bail!("project inventory exceeds ingest safety limits");
        }
    }
    Ok(files)
}

/// The project root this wiki should ingest: the registered root containing
/// cwd, else the first registered root.
fn ingest_root(w: &Wiki, cwd: &Path) -> Result<PathBuf> {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    for r in &w.config.project_roots {
        let root = Path::new(r);
        let canon = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        if cwd.starts_with(&canon) {
            return Ok(canon);
        }
    }
    match w.config.project_roots.first() {
        Some(r) => Ok(PathBuf::from(r)),
        None => bail!("wiki '{}' has no project_roots configured", w.slug),
    }
}

fn code_page_id(dir: &str) -> String {
    let segs: Vec<String> = dir
        .split('/')
        .map(slugify)
        .filter(|s| !s.is_empty())
        .collect();
    format!("code/{}", segs.join("/"))
}

/// Group files by directory prefix at the given depth. Root-level files are
/// excluded (they belong to the index/architecture pages).
fn dirs_at_depth(files: &[String], depth: usize) -> BTreeMap<String, Vec<&String>> {
    let mut map: BTreeMap<String, Vec<&String>> = BTreeMap::new();
    for f in files {
        let segs: Vec<&str> = f.split('/').collect();
        if segs.len() > depth {
            map.entry(segs[..depth].join("/")).or_default().push(f);
        }
    }
    map
}

fn dir_stub_body(dir: &str, files: &[&String]) -> (String, String) {
    let mut exts: BTreeMap<String, usize> = BTreeMap::new();
    for f in files {
        if let Some(ext) = Path::new(f.as_str()).extension().and_then(|e| e.to_str()) {
            *exts.entry(format!(".{ext}")).or_default() += 1;
        }
    }
    let mut exts: Vec<(String, usize)> = exts.into_iter().collect();
    exts.sort_by_key(|entry| std::cmp::Reverse(entry.1));
    let main_exts: Vec<String> = exts.into_iter().take(3).map(|(e, _)| e).collect();

    let mut notable: Vec<&str> = files.iter().map(|f| f.as_str()).collect();
    notable.sort_by_key(|f| (f.matches('/').count(), f.to_string()));
    let notable: Vec<&str> = notable.into_iter().take(5).collect();

    let description = format!("TODO: describe the {dir} module");
    let body = format!(
        "**TODO: what `{dir}` is and why it exists.** Replace this with one bold-lead \
         paragraph that stands alone as the hover summary.\n\n\
         File: `{dir}/`\n\n\
         ## Role\n\n\
         TODO: main entry points, and how this module connects to the rest of the system \
         (link the other `[[wikilinks]]` code pages it touches).\n\n\
         ## Key files\n\n\
         {}\n\n\
         > [!note] {} files{}.",
        notable
            .iter()
            .map(|f| format!("- `{f}`"))
            .collect::<Vec<_>>()
            .join("\n"),
        files.len(),
        if main_exts.is_empty() {
            String::new()
        } else {
            format!(" (mostly {})", main_exts.join(", "))
        },
    );
    (description, body)
}

fn seed_code_stub(
    w: &Wiki,
    guard: &crate::publish::MutationGuard,
    dir: &str,
    files: &[&String],
) -> Result<Option<String>> {
    let id = code_page_id(dir);
    if wiki::validate_id(&id).is_err() || w.exists(&id) || w.assert_writable(&id).is_err() {
        return Ok(None);
    }
    let (description, body) = dir_stub_body(dir, files);
    let mut stub = Page {
        id: id.clone(),
        fm: crate::page::Frontmatter {
            title: humanize(&id),
            description,
            tags: vec!["code".into()],
            created: today(),
            updated: today(),
            status: Some("stub".into()),
            sources: vec![format!("{dir}/")],
            pin: false,
            pin_level: None,
            aliases: vec![humanize(&id)],
            extra: vec![],
        },
        body,
    };
    w.save_page_raw_guarded(guard, &mut stub, false)?;
    Ok(Some(id))
}

const INGEST_RECEIPT_SCHEMA: &str = "wookie.ingest-reconciliation/v1";
const INGEST_RECOVERY_PATH: &str = ".ingest-reconciliation-recovery.json";
const INGEST_MARK_COMMIT_MESSAGE: &str = "wookie: ingest --mark-reconciled";
const INGEST_ROLLBACK_COMMIT_MESSAGE: &str = "wookie: rollback ingest --mark-reconciled";
const MAX_INGEST_CONFIG_BYTES: usize = 4 * 1024 * 1024;
const MAX_INGEST_RECOVERY_BYTES: usize = 2 * MAX_INGEST_CONFIG_BYTES + 1024 * 1024;
const MAX_INGEST_PROJECT_ROOT_BYTES: usize = 4 * 1024;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct IngestRecoveryMarker {
    schema: String,
    state: String,
    wiki: String,
    base_head: String,
    observed_head: Option<String>,
    project_root: String,
    target_project_revision: String,
    worklist_receipt: String,
    wiki_content_hash: String,
    policy_hash: String,
    target_sha256: String,
    previous_config: String,
    target_config: String,
    error: String,
}

fn read_bounded_ingest_file(path: &Path, limit: usize, label: &str) -> Result<Vec<u8>> {
    let raw = snapshot::read_raw_page(path)
        .with_context(|| format!("reading {label} {}", path.display()))?;
    if raw.len() > limit {
        bail!("{label} exceeds the {limit}-byte safety limit");
    }
    Ok(raw)
}

/// Complete, deterministic identity of an ingest worklist. Display limits are
/// deliberately applied only after this value is hashed: a short terminal or
/// MCP response must never weaken the reconciliation check.
#[derive(Debug, Clone, serde::Serialize)]
struct IngestReconciliation {
    schema: &'static str,
    wiki: String,
    project_root: String,
    mode: &'static str,
    level: String,
    wiki_content_hash: String,
    policy_hash: String,
    prior_sync: Option<String>,
    base_revision: Option<String>,
    target_revision: Option<String>,
    changed: Vec<String>,
    stale: BTreeMap<String, Vec<String>>,
    uncovered: Vec<String>,
    /// Stub pages that are part of the current worklist. This is derived from
    /// current wiki state rather than from a previous command's side effects,
    /// so the receipt can be recomputed safely at mark time.
    seeded: Vec<String>,
    entry_points: Vec<String>,
}

impl IngestReconciliation {
    fn receipt(&self) -> Result<String> {
        let encoded = serde_json::to_vec(self)?;
        Ok(format!("sha256:{:x}", Sha256::digest(encoded)))
    }

    fn mark_command(&self, receipt: &str) -> Option<String> {
        self.target_revision.as_ref()?;
        let mut command = format!(
            "wookie ingest --mark-reconciled --expect-worklist {receipt} --level {}",
            self.level
        );
        if let Some(base) = &self.base_revision {
            command.push_str(" --since ");
            command.push_str(base);
        } else {
            command.push_str(" --full");
        }
        Some(command)
    }
}

fn ingest_entry_points(files: &[String]) -> Vec<String> {
    const ENTRY: &[&str] = &[
        "README.md",
        "README.rst",
        "ARCHITECTURE.md",
        "CONTRIBUTING.md",
        "CLAUDE.md",
        "Cargo.toml",
        "package.json",
        "pyproject.toml",
        "go.mod",
        "Makefile",
        "docker-compose.yml",
    ];
    ENTRY
        .iter()
        .copied()
        .filter(|entry| files.iter().any(|file| file == entry))
        .map(str::to_string)
        .collect()
}

fn ingest_targets(files: &[String], level: IngestLevel) -> Vec<(String, Vec<&String>)> {
    let top = dirs_at_depth(files, 1);
    let mut targets: Vec<_> = top.into_iter().collect();
    targets.sort_by_key(|(dir, files)| (std::cmp::Reverse(files.len()), dir.clone()));
    targets.truncate(15);
    if level != IngestLevel::Quick {
        let mut second: Vec<_> = dirs_at_depth(files, 2)
            .into_iter()
            .filter(|(_, files)| files.len() >= 3)
            .collect();
        second.sort_by_key(|(dir, files)| (std::cmp::Reverse(files.len()), dir.clone()));
        second.truncate(25);
        targets.extend(second);
    }
    targets
}

fn map_changed_pages(
    pages: &[Page],
    changed: &[String],
) -> (BTreeMap<String, Vec<String>>, Vec<String>) {
    // Per changed file, only the most-specific source prefix is authoritative.
    let mut matches: Vec<(&String, usize, &str)> = Vec::new();
    for page in pages {
        for source in audit::effective_page_sources(page) {
            let prefix = source.trim_end_matches('/');
            for file in changed {
                if file == prefix || file.starts_with(&format!("{prefix}/")) {
                    matches.push((file, prefix.len(), page.id.as_str()));
                }
            }
        }
    }
    let mut best: BTreeMap<&String, usize> = BTreeMap::new();
    for (file, length, _) in &matches {
        let current = best.entry(file).or_insert(0);
        *current = (*current).max(*length);
    }
    let mut stale: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut covered: HashSet<&String> = HashSet::new();
    for (file, length, id) in &matches {
        covered.insert(file);
        if length == &best[file] {
            stale
                .entry((*id).to_string())
                .or_default()
                .push((*file).clone());
        }
    }
    for files in stale.values_mut() {
        files.sort();
        files.dedup();
    }
    let uncovered = changed
        .iter()
        .filter(|file| !covered.contains(file))
        .cloned()
        .collect();
    (stale, uncovered)
}

fn strict_ingest_pages(w: &Wiki) -> Result<(String, Vec<Page>)> {
    let catalog = snapshot::capture_catalog(w)?;
    let pages = catalog
        .pages
        .iter()
        .map(|captured| {
            let raw = std::str::from_utf8(&captured.raw)
                .with_context(|| format!("page '{}' is not valid UTF-8", captured.id))?;
            let page = Page::parse(&captured.id, raw);
            page.validate_frontmatter()?;
            Ok(page)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((catalog.content_hash, pages))
}

fn ingest_policy_hash(w: &Wiki) -> Result<String> {
    let policy = serde_json::json!({
        "project_roots": &w.config.project_roots,
        "sections": w.sections(),
        "audit": &w.audit,
        "retrieval": &w.retrieval,
        "publish": &w.publish,
        "auto_commit": w.auto_commit,
        "history": &w.history,
    });
    Ok(format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(&policy)?)
    ))
}

fn ingest_reconciliation(
    w: &Wiki,
    root: &Path,
    base: Option<&str>,
    level: IngestLevel,
) -> Result<IngestReconciliation> {
    let mut files = list_project_files(root)?;
    files.sort();
    files.dedup();
    let (wiki_content_hash, pages) = strict_ingest_pages(w)?;
    let target_revision = head_commit(root);
    let base_revision = base
        .map(|base| resolve_git_commit(root, base, "ingest base revision"))
        .transpose()?;
    let (mode, changed, stale, uncovered, seeded) = if let Some(base) = &base_revision {
        let changed = changed_since(root, base)?;
        let (stale, uncovered) = map_changed_pages(&pages, &changed);
        let mut seeded = stale
            .keys()
            .filter(|id| {
                pages
                    .iter()
                    .find(|page| &page.id == *id)
                    .is_some_and(Page::is_stub)
            })
            .cloned()
            .collect::<Vec<_>>();
        seeded.sort();
        ("update", changed, stale, uncovered, seeded)
    } else {
        let mut seeded = ingest_targets(&files, level)
            .into_iter()
            .map(|(dir, _)| code_page_id(&dir))
            .filter(|id| {
                pages
                    .iter()
                    .find(|page| page.id == *id)
                    .is_some_and(Page::is_stub)
            })
            .collect::<Vec<_>>();
        seeded.sort();
        seeded.dedup();
        ("fresh", files.clone(), BTreeMap::new(), Vec::new(), seeded)
    };
    Ok(IngestReconciliation {
        schema: INGEST_RECEIPT_SCHEMA,
        wiki: w.slug.clone(),
        project_root: root
            .to_str()
            .context("ingest project root must be valid UTF-8")?
            .to_string(),
        mode,
        level: format!("{level:?}").to_lowercase(),
        wiki_content_hash,
        policy_hash: ingest_policy_hash(w)?,
        prior_sync: w.config.last_ingest_commit.clone(),
        base_revision,
        target_revision,
        changed,
        stale,
        uncovered,
        seeded,
        entry_points: ingest_entry_points(&files),
    })
}

fn validate_ingest_receipt(value: &str) -> Result<()> {
    let digest = value.strip_prefix("sha256:").ok_or_else(|| {
        anyhow::anyhow!("--expect-worklist must be a sha256: reconciliation receipt")
    })?;
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("--expect-worklist must be a sha256: reconciliation receipt");
    }
    Ok(())
}

fn canonical_git_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn canonical_sha256(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn validate_ingest_recovery_marker(w: &Wiki, marker: &IngestRecoveryMarker) -> Result<()> {
    for (label, revision) in [
        ("base_head", marker.base_head.as_str()),
        (
            "target_project_revision",
            marker.target_project_revision.as_str(),
        ),
    ] {
        if !canonical_git_object_id(revision) {
            bail!("ingest recovery marker {label} is not an exact canonical Git object ID");
        }
    }
    if let Some(observed) = &marker.observed_head {
        if !canonical_git_object_id(observed) {
            bail!("ingest recovery marker observed_head is not an exact canonical Git object ID");
        }
    }
    match marker.state.as_str() {
        "prepared" if marker.observed_head.as_deref() != Some(&marker.base_head) => {
            bail!("prepared ingest recovery marker has an inconsistent observed_head")
        }
        "rolling_back" if marker.observed_head.as_deref() == Some(&marker.base_head) => {
            bail!("rolling-back ingest recovery marker has no exact mark commit")
        }
        "rolling_back" if marker.observed_head.is_none() => {
            bail!("rolling-back ingest recovery marker has no observed_head")
        }
        "prepared" | "rolling_back" => {}
        _ => bail!("unknown ingest recovery marker state '{}'", marker.state),
    }

    for (label, digest) in [
        ("worklist_receipt", marker.worklist_receipt.as_str()),
        ("wiki_content_hash", marker.wiki_content_hash.as_str()),
        ("policy_hash", marker.policy_hash.as_str()),
        ("target_sha256", marker.target_sha256.as_str()),
    ] {
        if !canonical_sha256(digest) {
            bail!("ingest recovery marker {label} is not a canonical sha256 digest");
        }
    }

    if marker.project_root.is_empty()
        || marker.project_root.len() > MAX_INGEST_PROJECT_ROOT_BYTES
        || marker.project_root.chars().any(char::is_control)
    {
        bail!("ingest recovery marker project_root is invalid or exceeds its safety limit");
    }
    let recorded_root = Path::new(&marker.project_root);
    if !recorded_root.is_absolute() {
        bail!("ingest recovery marker project_root must be absolute");
    }
    let canonical_root = recorded_root.canonicalize().with_context(|| {
        format!(
            "resolving ingest recovery project root {}",
            recorded_root.display()
        )
    })?;
    if !canonical_root.is_dir()
        || canonical_root
            .to_str()
            .is_none_or(|canonical| canonical != marker.project_root)
    {
        bail!("ingest recovery marker project_root is not a canonical directory path");
    }

    if marker.previous_config.len() > MAX_INGEST_CONFIG_BYTES
        || marker.target_config.len() > MAX_INGEST_CONFIG_BYTES
    {
        bail!("ingest recovery marker config image exceeds its safety limit");
    }
    let mut previous: crate::wiki::WikiConfig = toml::from_str(&marker.previous_config)
        .context("parsing previous config image in ingest recovery marker")?;
    let mut target: crate::wiki::WikiConfig = toml::from_str(&marker.target_config)
        .context("parsing target config image in ingest recovery marker")?;
    previous.validate()?;
    target.validate()?;
    if previous.name != w.slug || target.name != w.slug {
        bail!("ingest recovery marker config image belongs to another wiki");
    }
    if target.last_ingest_commit.as_deref() != Some(&marker.target_project_revision) {
        bail!("ingest recovery marker target config has the wrong sync point");
    }
    if toml::to_string_pretty(&target)? != marker.target_config {
        bail!("ingest recovery marker target config is not canonical");
    }
    let actual_target_sha256 = format!(
        "sha256:{:x}",
        Sha256::digest(marker.target_config.as_bytes())
    );
    if actual_target_sha256 != marker.target_sha256 {
        bail!("ingest recovery marker target config digest does not match its image");
    }
    previous.last_ingest_commit = None;
    target.last_ingest_commit = None;
    if serde_json::to_value(previous)? != serde_json::to_value(target)? {
        bail!("ingest recovery marker config images differ beyond the sync point");
    }
    if !marker.error.is_empty() {
        bail!("ingest recovery marker contains an unsupported error payload");
    }
    Ok(())
}

fn git_output(root: &Path, args: &[&str]) -> Result<std::process::Output> {
    std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .with_context(|| format!("running git in {}", root.display()))
}

fn git_head(root: &Path) -> Result<String> {
    let output = git_output(root, &["rev-parse", "--verify", "HEAD"])?;
    if !output.status.success() {
        bail!(
            "cannot resolve wiki HEAD: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn ensure_wiki_config_clean(w: &Wiki) -> Result<()> {
    if !w.auto_commit {
        return Ok(());
    }
    let output = git_output(
        &w.dir,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--",
            "wookie.toml",
        ],
    )?;
    if !output.status.success() {
        bail!(
            "cannot inspect wookie.toml history state: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    if !output.stdout.is_empty() {
        bail!(
            "wookie.toml has pre-existing staged, unstaged, or untracked changes; commit or restore it before marking reconciliation"
        );
    }
    Ok(())
}

fn tree_entry_kind(entry: &[u8]) -> Option<(&[u8], &[u8])> {
    let mut fields = entry.split(|byte| *byte == b' ' || *byte == b'\t');
    Some((fields.next()?, fields.next()?))
}

fn exact_config_commit_landed(
    w: &Wiki,
    base: &str,
    expected: &[u8],
    expected_message: &str,
) -> Result<Option<String>> {
    let captured_head = git_head(&w.dir)?;
    if captured_head == base {
        return Ok(None);
    }
    let parent_ref = format!("{captured_head}^");
    let parent = git_output(&w.dir, &["rev-parse", "--verify", &parent_ref])?;
    if !parent.status.success() || String::from_utf8_lossy(&parent.stdout).trim() != base {
        return Ok(None);
    }
    let blob_ref = format!("{captured_head}:wookie.toml");
    let content =
        bounded_git_path_output(&w.dir, &["show", &blob_ref], "ingest config commit blob")?;
    if content != expected {
        return Ok(None);
    }
    let commit_object = bounded_git_path_output(
        &w.dir,
        &["cat-file", "commit", &captured_head],
        "ingest config commit object",
    )?;
    let Some(message_offset) = commit_object
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|offset| offset + 2)
    else {
        return Ok(None);
    };
    let mut canonical_message = expected_message.as_bytes().to_vec();
    canonical_message.push(b'\n');
    if commit_object[message_offset..] != canonical_message {
        return Ok(None);
    }
    let paths = bounded_git_path_output(
        &w.dir,
        &[
            "diff-tree",
            "--no-commit-id",
            "--name-only",
            "-r",
            "-z",
            &captured_head,
        ],
        "ingest config commit paths",
    )?;
    if paths != b"wookie.toml\0" {
        return Ok(None);
    }
    let base_tree = bounded_git_path_output(
        &w.dir,
        &["ls-tree", "-z", base, "--", "wookie.toml"],
        "base ingest config tree entry",
    )?;
    let target_tree = bounded_git_path_output(
        &w.dir,
        &["ls-tree", "-z", &captured_head, "--", "wookie.toml"],
        "target ingest config tree entry",
    )?;
    let base_kind = tree_entry_kind(&base_tree);
    let target_kind = tree_entry_kind(&target_tree);
    if base_kind.is_none() || base_kind != target_kind || base_kind.unwrap().1 != b"blob" {
        return Ok(None);
    }
    let status = git_output(
        &w.dir,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--",
            "wookie.toml",
        ],
    )?;
    if !status.status.success() || !status.stdout.is_empty() {
        return Ok(None);
    }
    if git_head(&w.dir)? != captured_head {
        return Ok(None);
    }
    Ok(Some(captured_head))
}

fn restore_ingest_config(
    w: &mut Wiki,
    _guard: &crate::publish::MutationGuard,
    previous_config: crate::wiki::WikiConfig,
    previous_raw: &[u8],
    base_head: Option<&str>,
) -> Result<()> {
    w.config = previous_config;
    let config_path = w.contained_path(Path::new("wookie.toml"))?;
    wiki::atomic_write(&config_path, previous_raw)?;
    if let Some(base_head) = base_head {
        let reset = git_output(&w.dir, &["reset", "-q", base_head, "--", "wookie.toml"])?;
        if !reset.status.success() {
            bail!(
                "restoring the wookie.toml index entry failed: {}",
                String::from_utf8_lossy(&reset.stderr).trim()
            );
        }
        ensure_wiki_config_clean(w)?;
    }
    Ok(())
}

fn write_ingest_recovery_marker(w: &Wiki, marker: &IngestRecoveryMarker) -> Result<PathBuf> {
    let path = w.contained_path(Path::new(INGEST_RECOVERY_PATH))?;
    let raw = serde_json::to_vec_pretty(marker)?;
    if raw.len() > MAX_INGEST_RECOVERY_BYTES {
        bail!("ingest recovery marker exceeds the {MAX_INGEST_RECOVERY_BYTES}-byte safety limit");
    }
    wiki::atomic_write(&path, raw)?;
    Ok(path)
}

fn read_ingest_recovery_marker(w: &Wiki) -> Result<IngestRecoveryMarker> {
    let path = w.contained_path(Path::new(INGEST_RECOVERY_PATH))?;
    let metadata = std::fs::symlink_metadata(&path)
        .with_context(|| format!("reading ingest recovery marker {}", path.display()))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > u64::try_from(MAX_INGEST_RECOVERY_BYTES).unwrap_or(u64::MAX)
    {
        bail!("ingest recovery marker must be a bounded regular file");
    }
    let raw = read_bounded_ingest_file(&path, MAX_INGEST_RECOVERY_BYTES, "ingest recovery marker")?;
    let marker: IngestRecoveryMarker = serde_json::from_slice(&raw)
        .with_context(|| format!("parsing ingest recovery marker {}", path.display()))?;
    if marker.schema != "wookie.ingest-reconciliation-recovery/v1" || marker.wiki != w.slug {
        bail!(
            "ingest recovery marker identity does not match wiki '{}'",
            w.slug
        );
    }
    if !matches!(marker.state.as_str(), "prepared" | "rolling_back") {
        bail!("unknown ingest recovery marker state '{}'", marker.state);
    }
    validate_ingest_recovery_marker(w, &marker)?;
    Ok(marker)
}

fn install_ingest_config_image(
    w: &mut Wiki,
    guard: &crate::publish::MutationGuard,
    raw: &str,
) -> Result<()> {
    if raw.len() > MAX_INGEST_CONFIG_BYTES {
        bail!("ingest config image exceeds the {MAX_INGEST_CONFIG_BYTES}-byte safety limit");
    }
    let config: crate::wiki::WikiConfig = toml::from_str(raw)?;
    config.validate()?;
    if config.name != w.slug {
        bail!("ingest config image belongs to wiki '{}'", config.name);
    }
    let path = w.contained_path(Path::new("wookie.toml"))?;
    wiki::atomic_write(&path, raw.as_bytes())?;
    w.reload_config_guarded(guard)
}

fn reset_ingest_config_index(w: &Wiki, revision: &str) -> Result<()> {
    let reset = git_output(&w.dir, &["reset", "-q", revision, "--", "wookie.toml"])?;
    if !reset.status.success() {
        bail!(
            "restoring the wookie.toml index entry failed: {}",
            String::from_utf8_lossy(&reset.stderr).trim()
        );
    }
    Ok(())
}

fn commit_ingest_config_image(
    w: &Wiki,
    parent: &str,
    expected: &[u8],
    message: &str,
) -> Result<String> {
    let result = crate::history::commit_paths(&w.dir, message, &["wookie.toml".into()], &w.history);
    if let Some(commit) = exact_config_commit_landed(w, parent, expected, message)? {
        return Ok(commit);
    }
    match result {
        Ok(_) => bail!("config history command completed but its exact commit did not land"),
        Err(error) => Err(error).context("config history command failed without an exact commit"),
    }
}

fn verify_ingest_recovery_environment(
    w: &Wiki,
    root: &Path,
    marker: &IngestRecoveryMarker,
) -> Result<()> {
    if head_commit(root).as_deref() != Some(&marker.target_project_revision)
        || !changed_since(root, &marker.target_project_revision)?.is_empty()
    {
        bail!(
            "project target is no longer the exact clean commit {}",
            marker.target_project_revision
        );
    }
    let (catalog_hash, _) = strict_ingest_pages(w)?;
    if catalog_hash != marker.wiki_content_hash {
        bail!(
            "wiki catalog changed after worklist receipt {}; restore or reconcile it before recovery",
            marker.worklist_receipt
        );
    }
    if ingest_policy_hash(w)? != marker.policy_hash {
        bail!("effective ingest/audit policy changed after the worklist receipt");
    }
    Ok(())
}

fn recover_ingest_reconciliation(
    w: &mut Wiki,
    root: &Path,
    action: IngestRecoveryAction,
    json: bool,
) -> Result<String> {
    let guard = publish::acquire_ingest_recovery_guard(w)?;
    let mut marker = read_ingest_recovery_marker(w)?;
    let root_text = root
        .to_str()
        .context("ingest project root must be valid UTF-8")?;
    if marker.project_root != root_text {
        bail!(
            "recovery marker belongs to project root '{}', not '{}'",
            marker.project_root,
            root.display()
        );
    }
    let marker_path = w.contained_path(Path::new(INGEST_RECOVERY_PATH))?;
    verify_ingest_recovery_environment(w, root, &marker)?;
    match action {
        IngestRecoveryAction::Accept => {
            if marker.state != "prepared" {
                bail!(
                    "cannot accept ingest recovery while '{}' is in progress; finish rollback",
                    marker.state
                );
            }
            let config_path = w.contained_path(Path::new("wookie.toml"))?;
            let current =
                read_bounded_ingest_file(&config_path, MAX_INGEST_CONFIG_BYTES, "wookie config")?;
            let head = git_head(&w.dir)?;
            if head == marker.base_head {
                if current != marker.previous_config.as_bytes()
                    && current != marker.target_config.as_bytes()
                {
                    bail!(
                        "cannot accept ingest recovery: wookie.toml matches neither recorded image"
                    );
                }
                reset_ingest_config_index(w, &marker.base_head)?;
                install_ingest_config_image(w, &guard, &marker.target_config)?;
                commit_ingest_config_image(
                    w,
                    &marker.base_head,
                    marker.target_config.as_bytes(),
                    INGEST_MARK_COMMIT_MESSAGE,
                )?;
            } else if exact_config_commit_landed(
                w,
                &marker.base_head,
                marker.target_config.as_bytes(),
                INGEST_MARK_COMMIT_MESSAGE,
            )?
            .is_none()
            {
                bail!(
                    "cannot accept ingest recovery: wiki history diverged from the exact mark child of {}",
                    marker.base_head
                );
            }
            let current =
                read_bounded_ingest_file(&config_path, MAX_INGEST_CONFIG_BYTES, "wookie config")?;
            if current != marker.target_config.as_bytes()
                || format!("sha256:{:x}", Sha256::digest(&current)) != marker.target_sha256
            {
                bail!(
                    "cannot accept ingest recovery: wookie.toml is not the recorded target image"
                );
            }
            ensure_wiki_config_clean(w)?;
            let config: crate::wiki::WikiConfig = toml::from_str(&marker.target_config)?;
            config.validate()?;
            if config.last_ingest_commit.as_deref() != Some(&marker.target_project_revision) {
                bail!("cannot accept ingest recovery: target config has the wrong sync point");
            }
            w.reload_config_guarded(&guard)?;
            verify_ingest_recovery_environment(w, root, &marker)?;
            std::fs::remove_file(&marker_path)?;
        }
        IngestRecoveryAction::Rollback => {
            let config_path = w.contained_path(Path::new("wookie.toml"))?;
            let current =
                read_bounded_ingest_file(&config_path, MAX_INGEST_CONFIG_BYTES, "wookie config")?;
            if current != marker.previous_config.as_bytes()
                && current != marker.target_config.as_bytes()
            {
                bail!(
                    "cannot roll back ingest recovery: wookie.toml matches neither recorded image"
                );
            }
            let head = git_head(&w.dir)?;
            if marker.state == "prepared" && head == marker.base_head {
                reset_ingest_config_index(w, &marker.base_head)?;
                install_ingest_config_image(w, &guard, &marker.previous_config)?;
                ensure_wiki_config_clean(w)?;
                w.reload_config_guarded(&guard)?;
                verify_ingest_recovery_environment(w, root, &marker)?;
                std::fs::remove_file(&marker_path)?;
                return if json {
                    Ok(serde_json::json!({
                        "schema": "wookie.ingest-recovery/v1",
                        "wiki": w.slug,
                        "action": "rollback",
                        "recovered": true,
                        "target_project_revision": marker.target_project_revision,
                    })
                    .to_string())
                } else {
                    Ok(format!(
                        "Recovered ingest reconciliation for '{}' with rollback.",
                        w.slug
                    ))
                };
            }

            let mark_head = if marker.state == "prepared" {
                let Some(mark_head) = exact_config_commit_landed(
                    w,
                    &marker.base_head,
                    marker.target_config.as_bytes(),
                    INGEST_MARK_COMMIT_MESSAGE,
                )?
                else {
                    bail!("cannot roll back ingest recovery: wiki history is not the exact mark child");
                };
                marker.state = "rolling_back".into();
                marker.observed_head = Some(mark_head.clone());
                write_ingest_recovery_marker(w, &marker)?;
                mark_head
            } else if marker.state == "rolling_back" {
                marker
                    .observed_head
                    .clone()
                    .context("rolling-back ingest marker has no mark commit")?
            } else {
                bail!("unknown ingest recovery state '{}'", marker.state);
            };

            let current_head = git_head(&w.dir)?;
            if current_head == mark_head {
                reset_ingest_config_index(w, &mark_head)?;
                install_ingest_config_image(w, &guard, &marker.previous_config)?;
                commit_ingest_config_image(
                    w,
                    &mark_head,
                    marker.previous_config.as_bytes(),
                    INGEST_ROLLBACK_COMMIT_MESSAGE,
                )?;
            } else if exact_config_commit_landed(
                w,
                &mark_head,
                marker.previous_config.as_bytes(),
                INGEST_ROLLBACK_COMMIT_MESSAGE,
            )?
            .is_none()
            {
                bail!("cannot finish ingest rollback: wiki history diverged from the recorded lineage");
            }
            let current =
                read_bounded_ingest_file(&config_path, MAX_INGEST_CONFIG_BYTES, "wookie config")?;
            if current != marker.previous_config.as_bytes() {
                bail!("cannot finish ingest rollback: previous config image is not active");
            }
            ensure_wiki_config_clean(w)?;
            w.reload_config_guarded(&guard)?;
            verify_ingest_recovery_environment(w, root, &marker)?;
            std::fs::remove_file(&marker_path)?;
        }
    }
    let action_name = match action {
        IngestRecoveryAction::Accept => "accept",
        IngestRecoveryAction::Rollback => "rollback",
    };
    if json {
        Ok(serde_json::json!({
            "schema": "wookie.ingest-recovery/v1",
            "wiki": w.slug,
            "action": action_name,
            "recovered": true,
            "target_project_revision": marker.target_project_revision,
        })
        .to_string())
    } else {
        Ok(format!(
            "Recovered ingest reconciliation for '{}' with {action_name}.",
            w.slug
        ))
    }
}

fn ingest_display_limit(w: &Wiki, options: &IngestOptions<'_>) -> Result<Option<usize>> {
    if options.all {
        if options.limit.is_some() || options.tokens.is_some() {
            bail!("--all cannot be combined with --limit or --tokens");
        }
        return Ok(None);
    }
    let limit = options.limit.unwrap_or(w.retrieval.search_limit);
    if !(1..=MAX_SEARCH_LIMIT).contains(&limit) {
        bail!("--limit must be between 1 and {MAX_SEARCH_LIMIT}");
    }
    let tokens = options.tokens.unwrap_or(w.publish.output_tokens);
    if tokens < 256 {
        bail!("--tokens must be at least 256");
    }
    // Leave most of the budget for report scaffolding, receipt metadata, and
    // nested changed-file lists. The selection remains count-bounded too.
    let token_limited = (tokens.saturating_sub(192) / 96).max(1);
    Ok(Some(limit.min(token_limited)))
}

fn ingest_projection(total: usize, returned: usize, limit: Option<usize>) -> serde_json::Value {
    serde_json::json!({
        "total": total,
        "returned": returned,
        "omitted": total.saturating_sub(returned),
        "limit": limit,
    })
}

fn ingest_category_byte_budget(w: &Wiki, options: &IngestOptions<'_>) -> Result<Option<usize>> {
    if options.all {
        return Ok(None);
    }
    let tokens = options.tokens.unwrap_or(w.publish.output_tokens);
    let response_bytes = tokens
        .checked_mul(4)
        .context("ingest output budget overflow")?;
    Ok(Some((response_bytes / 16).max(32)))
}

fn ingest_take<T: Clone + serde::Serialize>(
    items: &[T],
    limit: Option<usize>,
    byte_budget: Option<usize>,
) -> Vec<T> {
    let count = limit.unwrap_or(usize::MAX);
    let mut used = 0_usize;
    items
        .iter()
        .take(count)
        .take_while(|item| {
            let size = serde_json::to_vec(item)
                .map(|encoded| encoded.len().saturating_add(1))
                .unwrap_or(usize::MAX);
            let next = used.saturating_add(size);
            let fits = byte_budget.is_none_or(|budget| next <= budget);
            if fits {
                used = next;
            }
            fits
        })
        .cloned()
        .collect()
}

fn ingest_json_report(
    w: &Wiki,
    root: &Path,
    data: serde_json::Value,
    diagnostics: Vec<report::Diagnostic>,
) -> Result<String> {
    let mut project = report::ProjectSnapshot::new(
        root.to_string_lossy(),
        report::ProjectSnapshotMode::WorkingTree,
    );
    let bound_revision = data
        .get("target_revision")
        .or_else(|| data.get("marked"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    if let Some(revision) = bound_revision.or_else(|| head_commit(root)) {
        project = project.revision(revision);
    }
    let content_hash = data
        .get("wiki_content_hash")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .map(Ok)
        .unwrap_or_else(|| snapshot::wiki_content_hash(w))?;
    let mut snapshot = report::Snapshot::new(&w.slug)
        .wiki_content_hash(content_hash)
        .with_project(project);
    if let Some(revision) = wiki_revision(w) {
        snapshot = snapshot.wiki_revision(revision);
    }
    let mut report = report::Report::with_diagnostics("ingest", snapshot, diagnostics);
    if let Some(object) = data.as_object() {
        for (key, value) in object {
            report.insert_data(key, value.clone());
        }
    }
    Ok(serde_json::to_string(&report)?)
}

pub struct IngestOptions<'a> {
    pub project_root: Option<&'a Path>,
    pub level: IngestLevel,
    pub mark: bool,
    pub recover: Option<IngestRecoveryAction>,
    pub expect_worklist: Option<&'a str>,
    pub full: bool,
    pub since: Option<&'a str>,
    pub limit: Option<usize>,
    pub tokens: Option<usize>,
    pub all: bool,
    pub json: bool,
}

pub fn ingest(w: &mut Wiki, cwd: &Path, options: &IngestOptions<'_>) -> Result<String> {
    if options.full && options.since.is_some() {
        bail!("--full cannot be combined with --since");
    }
    if !options.mark && options.expect_worklist.is_some() {
        bail!("--expect-worklist is only valid with --mark-reconciled");
    }
    if options.recover.is_some()
        && (options.mark
            || options.expect_worklist.is_some()
            || options.full
            || options.since.is_some()
            || options.limit.is_some()
            || options.tokens.is_some()
            || options.all)
    {
        bail!("--recover cannot be combined with mark, selection, or display options");
    }
    let root = match options.project_root {
        Some(root) => root
            .canonicalize()
            .with_context(|| format!("resolving project root {}", root.display()))?,
        None => ingest_root(w, cwd)?,
    };

    if let Some(action) = options.recover {
        return recover_ingest_reconciliation(w, &root, action, options.json);
    }
    let _display_limit = ingest_display_limit(w, options)?;

    if options.mark {
        let expected = options.expect_worklist.ok_or_else(|| {
            anyhow::anyhow!(
                "--mark-reconciled requires --expect-worklist <sha256:...>; rerun `wookie ingest` and use its exact mark command"
            )
        })?;
        validate_ingest_receipt(expected)?;
        let guard = w.acquire_mutation_guard()?;
        w.reload_config_guarded(&guard)?;
        let recovery_path = w.contained_path(Path::new(INGEST_RECOVERY_PATH))?;
        if recovery_path.exists() {
            bail!(
                "an unresolved ingest reconciliation recovery marker exists at {}; inspect wiki history before marking again",
                recovery_path.display()
            );
        }
        ensure_wiki_config_clean(w)?;
        let base = options.since.map(str::to_string).or_else(|| {
            if options.full {
                None
            } else {
                w.config.last_ingest_commit.clone()
            }
        });
        let reconciliation = ingest_reconciliation(w, &root, base.as_deref(), options.level)?;
        let actual = reconciliation.receipt()?;
        if actual != expected {
            bail!(
                "ingest reconciliation receipt changed (expected {expected}, current {actual}); rerun `wookie ingest` and review the current worklist before marking"
            );
        }
        let head = reconciliation.target_revision.clone().ok_or_else(|| {
            anyhow::anyhow!(
                "{} is not a git repo — --mark needs git history to diff against later",
                root.display()
            )
        })?;
        let dirty = changed_since(&root, &head)?;
        if !dirty.is_empty() {
            bail!(
                "cannot mark reconciliation while the project differs from target HEAD {} ({} path(s)); commit or clean those changes, rerun `wookie ingest`, and review the new worklist",
                &head[..8.min(head.len())],
                dirty.len()
            );
        }

        let (_, audit_pages) = strict_ingest_pages(w)?;
        let audit_report = audit::audit_pages(
            w,
            &audit::AuditOptions {
                project_root: Some(root.clone()),
                project_revision: None,
            },
            "ingest-mark",
            &audit_pages,
        )?;
        if audit_report.has_errors() {
            let first = audit_report
                .diagnostics
                .iter()
                .find(|diagnostic| diagnostic.severity == report::Severity::Error)
                .map(|diagnostic| diagnostic.message.as_str())
                .unwrap_or("unknown audit error");
            bail!(
                "cannot mark reconciliation: doctor found {} error(s); first error: {first}. Run `wookie doctor`, fix every error, then rerun `wookie ingest` for a new receipt",
                audit_report.summary.errors
            );
        }

        // Auditing may be slow enough for the project checkout to move. The
        // wiki guard prevents page/config races; recomputing here also closes
        // the project-side check/use window as far as a local tool can.
        let rechecked = ingest_reconciliation(w, &root, base.as_deref(), options.level)?;
        let rechecked_receipt = rechecked.receipt()?;
        if rechecked_receipt != expected || rechecked.target_revision.as_deref() != Some(&head) {
            bail!(
                "ingest reconciliation changed during validation; rerun `wookie ingest` and review the new worklist"
            );
        }
        if !changed_since(&root, &head)?.is_empty() {
            bail!(
                "project changed during reconciliation validation; rerun `wookie ingest` and review the new worklist"
            );
        }

        let config_path = w.contained_path(Path::new("wookie.toml"))?;
        let previous_raw =
            read_bounded_ingest_file(&config_path, MAX_INGEST_CONFIG_BYTES, "wookie config")?;
        let previous_config = w.config.clone();
        let wiki_head_before = w.auto_commit.then(|| git_head(&w.dir)).transpose()?;
        let mut target_config = previous_config.clone();
        target_config.last_ingest_commit = Some(head.clone());
        target_config.validate()?;
        let target_raw = toml::to_string_pretty(&target_config)?.into_bytes();
        if target_raw.len() > MAX_INGEST_CONFIG_BYTES {
            bail!("target wookie config exceeds the {MAX_INGEST_CONFIG_BYTES}-byte safety limit");
        }
        let target_sha256 = format!("sha256:{:x}", Sha256::digest(&target_raw));
        let previous_config_text = std::str::from_utf8(&previous_raw)
            .context("previous wookie.toml is not valid UTF-8")?;
        let target_config_text =
            std::str::from_utf8(&target_raw).context("target wookie.toml is not valid UTF-8")?;
        let project_root_text = root
            .to_str()
            .context("ingest project root must be valid UTF-8")?;
        let recovery_marker = wiki_head_before
            .as_deref()
            .map(|base_head| IngestRecoveryMarker {
                schema: "wookie.ingest-reconciliation-recovery/v1".into(),
                state: "prepared".into(),
                wiki: w.slug.clone(),
                base_head: base_head.into(),
                observed_head: Some(base_head.into()),
                project_root: project_root_text.into(),
                target_project_revision: head.clone(),
                worklist_receipt: expected.into(),
                wiki_content_hash: reconciliation.wiki_content_hash.clone(),
                policy_hash: reconciliation.policy_hash.clone(),
                target_sha256: target_sha256.clone(),
                previous_config: previous_config_text.into(),
                target_config: target_config_text.into(),
                error: String::new(),
            });
        let marker_path = w.contained_path(Path::new(INGEST_RECOVERY_PATH))?;
        if let Some(marker) = &recovery_marker {
            write_ingest_recovery_marker(w, marker)?;
        }
        w.config = target_config;
        if let Err(error) = w.save_config_guarded(&guard) {
            let restored = restore_ingest_config(
                w,
                &guard,
                previous_config.clone(),
                &previous_raw,
                wiki_head_before.as_deref(),
            );
            if restored.is_ok() && recovery_marker.is_some() {
                let _ = std::fs::remove_file(&marker_path);
            }
            return Err(error).context("writing reconciliation sync point");
        }
        if head_commit(&root).as_deref() != Some(&head) || !changed_since(&root, &head)?.is_empty()
        {
            restore_ingest_config(
                w,
                &guard,
                previous_config.clone(),
                &previous_raw,
                wiki_head_before.as_deref(),
            )?;
            if recovery_marker.is_some() {
                std::fs::remove_file(&marker_path)?;
            }
            bail!(
                "project changed while recording reconciliation; the previous sync point was restored"
            );
        }
        if let Some(base_head) = wiki_head_before.as_deref() {
            let commit = crate::history::commit_paths(
                &w.dir,
                INGEST_MARK_COMMIT_MESSAGE,
                &["wookie.toml".into()],
                &w.history,
            );
            match commit {
                Ok(changed) => {
                    if changed {
                        if exact_config_commit_landed(
                            w,
                            base_head,
                            &target_raw,
                            INGEST_MARK_COMMIT_MESSAGE,
                        )?
                        .is_none()
                        {
                            bail!(
                                "ingest config commit could not be verified; recovery marker retained at {}",
                                marker_path.display()
                            );
                        }
                    } else if previous_raw != target_raw {
                        restore_ingest_config(
                            w,
                            &guard,
                            previous_config.clone(),
                            &previous_raw,
                            Some(base_head),
                        )?;
                        std::fs::remove_file(&marker_path)?;
                        bail!(
                            "ingest sync-point history commit reported no change; config restored"
                        );
                    }
                }
                Err(error) => {
                    if exact_config_commit_landed(
                        w,
                        base_head,
                        &target_raw,
                        INGEST_MARK_COMMIT_MESSAGE,
                    )?
                    .is_none()
                    {
                        if git_head(&w.dir).ok().as_deref() == Some(base_head) {
                            restore_ingest_config(
                                w,
                                &guard,
                                previous_config.clone(),
                                &previous_raw,
                                Some(base_head),
                            )?;
                            std::fs::remove_file(&marker_path)?;
                            return Err(error).context(
                                "committing reconciliation sync point (config and index restored)",
                            );
                        }
                        bail!(
                            "ingest config commit failed with ambiguous wiki history; recovery marker retained at {}: {error:#}",
                            marker_path.display()
                        );
                    }
                    // A post-commit hook can return failure after the exact
                    // one-edge commit has landed. Verification is stronger
                    // evidence than the process exit code, so accept it.
                }
            }
        }
        if let Err(error) = w.reload_config_guarded(&guard) {
            if recovery_marker.is_some() {
                return Err(error).context(format!(
                    "reloading effective policy after the metadata commit; prepared recovery marker retained at {}",
                    marker_path.display()
                ));
            }
            restore_ingest_config(w, &guard, previous_config.clone(), &previous_raw, None)?;
            return Err(error).context(
                "reloading effective policy after recording reconciliation; config restored",
            );
        }
        let final_config =
            read_bounded_ingest_file(&config_path, MAX_INGEST_CONFIG_BYTES, "wookie config")?;
        let (final_catalog_hash, _) = strict_ingest_pages(w)?;
        let final_project_matches =
            head_commit(&root).as_deref() == Some(&head) && changed_since(&root, &head)?.is_empty();
        if final_config != target_raw
            || final_catalog_hash != reconciliation.wiki_content_hash
            || ingest_policy_hash(w)? != reconciliation.policy_hash
            || !final_project_matches
        {
            if recovery_marker.is_some() {
                bail!(
                    "wiki or project state changed during the metadata commit; prepared recovery marker retained at {}",
                    marker_path.display()
                );
            }
            restore_ingest_config(w, &guard, previous_config, &previous_raw, None)?;
            bail!("wiki or project state changed while recording reconciliation; config restored");
        }
        if recovery_marker.is_some() {
            std::fs::remove_file(&marker_path)?;
        }
        if options.json {
            return ingest_json_report(
                w,
                &root,
                serde_json::json!({
                    "mode": "mark",
                    "marked": head,
                    "receipt_schema": INGEST_RECEIPT_SCHEMA,
                    "worklist_receipt": expected,
                    "wiki_content_hash": reconciliation.wiki_content_hash,
                    "policy_hash": reconciliation.policy_hash,
                    "audit_gate": {"errors": 0, "passed": true}
                }),
                Vec::new(),
            );
        }
        return Ok(format!(
            "Marked wiki '{}' as synced to commit {}.",
            w.slug,
            &head[..8.min(head.len())]
        ));
    }

    let base = options.since.map(str::to_string).or_else(|| {
        if options.full {
            None
        } else {
            w.config.last_ingest_commit.clone()
        }
    });

    match base {
        Some(base) => ingest_update(w, &root, &base, options),
        None => ingest_fresh(w, &root, options),
    }
}

fn ingest_fresh(w: &Wiki, root: &Path, options: &IngestOptions<'_>) -> Result<String> {
    let level = options.level;
    let json = options.json;
    let mut files = list_project_files(root)?;
    files.sort();
    files.dedup();
    if files.is_empty() {
        bail!("no files found under {}", root.display());
    }

    let entries = ingest_entry_points(&files);

    // Seed stubs: top-level dirs always; significant second-level dirs for
    // standard/deep. Capped so a monorepo doesn't explode into stubs.
    let targets = ingest_targets(&files, level);

    let guard = w.acquire_mutation_guard()?;
    let mut created = vec![];
    for (dir, dir_files) in &targets {
        if let Some(id) = seed_code_stub(w, &guard, dir, dir_files)? {
            created.push(id);
        }
    }
    if !created.is_empty() {
        let paths = created
            .iter()
            .map(|id| format!("pages/{id}.md"))
            .collect::<Vec<_>>();
        w.commit_paths(
            &format!("wookie: ingest seed ({} stubs)", created.len()),
            &paths,
        )?;
    }

    let reconciliation = ingest_reconciliation(w, root, None, level)?;
    let receipt = reconciliation.receipt()?;
    let mark_command = reconciliation.mark_command(&receipt);
    let limit = ingest_display_limit(w, options)?;
    let byte_budget = ingest_category_byte_budget(w, options)?;
    let shown_changed = ingest_take(&reconciliation.changed, limit, byte_budget);
    let shown_seeded = ingest_take(&reconciliation.seeded, limit, byte_budget);
    let shown_created = ingest_take(&created, limit, byte_budget);

    if json {
        let data = serde_json::json!({
            "mode": "fresh", "level": format!("{level:?}").to_lowercase(),
            "root": root, "files": files.len(),
            "target_revision": &reconciliation.target_revision,
            "wiki_content_hash": &reconciliation.wiki_content_hash,
            "policy_hash": &reconciliation.policy_hash,
            "changed": &shown_changed,
            "entry_points": &entries, "seeded": &shown_seeded,
            "newly_seeded": &shown_created,
            "receipt_schema": INGEST_RECEIPT_SCHEMA,
            "worklist_receipt": &receipt,
            "mark_command": &mark_command,
            "worklist": {
                "read_entry_points": &entries,
                "fill_pages": &shown_seeded,
                "mark_command": &mark_command
            },
            "projection": {
                "exhaustive": limit.is_none(),
                "byte_budget_per_category": byte_budget,
                "changed": ingest_projection(reconciliation.changed.len(), shown_changed.len(), limit),
                "seeded": ingest_projection(reconciliation.seeded.len(), shown_seeded.len(), limit),
                "newly_seeded": ingest_projection(created.len(), shown_created.len(), limit),
                "continuation": if limit.is_some() { Some("wookie ingest --full --all") } else { None::<&str> }
            },
        });
        return ingest_json_report(w, root, data, Vec::new());
    }

    let mut out = String::new();
    let _ = writeln!(
        out,
        "Ingest ({:?}, fresh) — {} files under {}\n",
        level,
        files.len(),
        root.display()
    );
    if reconciliation.seeded.is_empty() {
        let _ = writeln!(out, "No new stubs seeded (module pages already exist).\n");
    } else {
        let _ = writeln!(
            out,
            "Module stub worklist ({} page(s)):",
            reconciliation.seeded.len()
        );
        for c in &shown_seeded {
            let _ = writeln!(out, "- {c}");
        }
        if shown_seeded.len() < reconciliation.seeded.len() {
            let _ = writeln!(
                out,
                "- ... {} omitted (rerun `wookie ingest --full --all` for the exhaustive list)",
                reconciliation.seeded.len() - shown_seeded.len()
            );
        }
        let _ = writeln!(out);
    }
    let _ = writeln!(out, "Worklist — do these now:");
    let _ = writeln!(
        out,
        "1. Read the entry points: {}.",
        if entries.is_empty() {
            "(none found — skim the file tree)".into()
        } else {
            entries.join(", ")
        }
    );
    let missing_required: Vec<String> = w
        .sections()
        .iter()
        .flat_map(|(s, cfg)| cfg.required.iter().map(move |r| format!("{s}/{r}")))
        .filter(|id| !w.exists(id))
        .collect();
    let _ = writeln!(
        out,
        "2. Write 'index' (what the project is, how it is laid out, link every code/* page){}.",
        if missing_required.is_empty() {
            String::new()
        } else {
            format!(" and the required pages: {}", missing_required.join(", "))
        }
    );
    let _ = writeln!(
        out,
        "3. Fill each seeded stub: read the module's key files, then pipe a body with `wookie write <id>`. File flow/concept pages under the sections shown by `wookie context` (workflow/ for commit+PR rules, style/ for conventions)."
    );
    match level {
        IngestLevel::Quick => {}
        IngestLevel::Standard => {
            let _ = writeln!(
                out,
                "4. Document the 3-5 most important flows/concepts as their own pages (e.g. request lifecycle, config loading). Link them with [[wikilinks]], then run `wookie expand` and fill what it stubs."
            );
        }
        IngestLevel::Deep => {
            let _ = writeln!(
                out,
                "4. Document every significant flow/concept as its own page; link them with [[wikilinks]], then run `wookie expand` and fill what it stubs."
            );
            let _ = writeln!(
                out,
                "5. For each key file or type inside a module, add a sub-page under the module's code/ path (set --sources to the file). Capture invariants, gotchas and edge cases, not just structure."
            );
        }
    }
    let last_step = match level {
        IngestLevel::Quick => 4,
        IngestLevel::Standard => 5,
        IngestLevel::Deep => 6,
    };
    if let Some(mark_command) = &mark_command {
        let _ = writeln!(
            out,
            "{last_step}. Run `wookie doctor`, fix every error, rerun ingest if the worklist changed, then record this exact sync point: `{mark_command}`."
        );
    } else {
        let _ = writeln!(
            out,
            "{last_step}. Run `wookie doctor` and fix what it reports. ({} is not a git repo, so wookie cannot track code changes; future ingests re-run fresh and `--mark` is unavailable.)",
            root.display()
        );
    }
    let _ = write!(
        out,
        "\nReconciliation receipt: {receipt}\nConventions: every page's first paragraph is a standalone summary; set `--sources` to the paths a page documents so future ingests can flag it when that code changes."
    );
    Ok(out.trim_end().to_string())
}

fn ingest_update(w: &Wiki, root: &Path, base: &str, options: &IngestOptions<'_>) -> Result<String> {
    let level = options.level;
    let json = options.json;
    let initial = ingest_reconciliation(w, root, Some(base), level)?;
    let changed = initial.changed.clone();
    let guard = (!changed.is_empty())
        .then(|| w.acquire_mutation_guard())
        .transpose()?;

    // Map changed files onto pages via their sources prefixes. Per file,
    // only the most specific (longest) matching prefix counts, so a change
    // in src/scheduler/ marks code/src/scheduler stale without also
    // dragging in the code/src parent page.
    let mut uncovered = initial.uncovered.clone();

    // New modules that appeared since last ingest get stubs; standard/deep
    // also look one level down, mirroring fresh-ingest seeding.
    let all_files = list_project_files(root)?;
    let mut seeded = vec![];
    let mut depths = vec![1];
    if level != IngestLevel::Quick {
        depths.push(2);
    }
    for depth in depths {
        for (dir, dir_files) in dirs_at_depth(&all_files, depth) {
            if depth == 2 && dir_files.len() < 3 {
                continue;
            }
            if uncovered.iter().any(|f| f.starts_with(&format!("{dir}/"))) {
                if let Some(id) = seed_code_stub(
                    w,
                    guard.as_ref().context("missing ingest mutation guard")?,
                    &dir,
                    &dir_files,
                )? {
                    seeded.push(id);
                }
            }
        }
    }
    if !seeded.is_empty() {
        let paths = seeded
            .iter()
            .map(|id| format!("pages/{id}.md"))
            .collect::<Vec<_>>();
        w.commit_paths(
            &format!("wookie: ingest seed ({} stubs)", seeded.len()),
            &paths,
        )?;
    }

    let reconciliation = ingest_reconciliation(w, root, Some(base), level)?;
    let receipt = reconciliation.receipt()?;
    let mark_command = reconciliation.mark_command(&receipt);
    let stale = reconciliation.stale.clone();
    uncovered = reconciliation.uncovered.clone();
    let limit = ingest_display_limit(w, options)?;
    let byte_budget = ingest_category_byte_budget(w, options)?;
    let shown_changed = ingest_take(&reconciliation.changed, limit, byte_budget);
    let stale_entries = stale
        .iter()
        .map(|(id, files)| (id.clone(), files.clone()))
        .collect::<Vec<_>>();
    let shown_stale = ingest_take(&stale_entries, limit, byte_budget);
    let shown_uncovered = ingest_take(&uncovered, limit, byte_budget);
    let shown_seeded = ingest_take(&reconciliation.seeded, limit, byte_budget);
    let shown_newly_seeded = ingest_take(&seeded, limit, byte_budget);

    if json {
        let mut diagnostics = Vec::new();
        let pages = w.all_pages();
        let stale_worklist = shown_stale
            .iter()
            .map(|(id, files)| {
                let page = pages.iter().find(|page| page.id == *id);
                let exact_source_match = page.is_some_and(|page| {
                    audit::effective_page_sources(page)
                        .iter()
                        .any(|source| files.iter().any(|file| file == source))
                });
                let confidence = if exact_source_match { "high" } else { "medium" };
                let section = id.split('/').next().unwrap_or("code");
                let suggested_sections = vec![section.to_string()];
                let shown_files = ingest_take(files, limit, byte_budget);
                diagnostics.push(
                    report::Diagnostic::new(
                        report::code::STALE_PAGE,
                        report::Severity::Warning,
                        format!("sources changed for page {id}"),
                    )
                    .page(id)
                    .suggestion(format!(
                        "Review the changed files and update {id}; then rerun ingest and use its exact receipt-bound mark command."
                    ))
                    .data("changed_files", serde_json::json!(&shown_files))
                    .data("changed_files_total", files.len())
                    .data("confidence", confidence),
                );
                serde_json::json!({
                    "id": id,
                    "files": &shown_files,
                    "files_total": files.len(),
                    "files_omitted": files.len().saturating_sub(shown_files.len()),
                    "confidence": confidence,
                    "suggested_sections": suggested_sections,
                    "reconcile_command": format!("wookie read {id}")
                })
            })
            .collect::<Vec<_>>();
        for file in &shown_uncovered {
            diagnostics.push(
                report::Diagnostic::new(
                    report::code::SOURCE_MISSING,
                    report::Severity::Info,
                    format!("changed file is not covered by any page source: {file}"),
                )
                .source(file)
                .suggestion("Add or update a page with project-relative --sources metadata."),
            );
        }
        let data = serde_json::json!({
            "mode": "update",
            "level": &reconciliation.level,
            "since": &reconciliation.base_revision,
            "target_revision": &reconciliation.target_revision,
            "wiki_content_hash": &reconciliation.wiki_content_hash,
            "policy_hash": &reconciliation.policy_hash,
            "changed": &shown_changed,
            "stale": &stale_worklist,
            "uncovered": &shown_uncovered,
            "seeded": &shown_seeded,
            "newly_seeded": &shown_newly_seeded,
            "worklist": &stale_worklist,
            "receipt_schema": INGEST_RECEIPT_SCHEMA,
            "worklist_receipt": &receipt,
            "mark_command": &mark_command,
            "projection": {
                "exhaustive": limit.is_none(),
                "byte_budget_per_category": byte_budget,
                "changed": ingest_projection(reconciliation.changed.len(), shown_changed.len(), limit),
                "stale": ingest_projection(stale_entries.len(), shown_stale.len(), limit),
                "uncovered": ingest_projection(uncovered.len(), shown_uncovered.len(), limit),
                "seeded": ingest_projection(reconciliation.seeded.len(), shown_seeded.len(), limit),
                "newly_seeded": ingest_projection(seeded.len(), shown_newly_seeded.len(), limit),
                "continuation": if limit.is_some() {
                    Some(format!(
                        "wookie ingest --since {} --level {} --all",
                        reconciliation.base_revision.as_deref().unwrap_or(base),
                        reconciliation.level
                    ))
                } else {
                    None
                }
            }
        });
        return ingest_json_report(w, root, data, diagnostics);
    }

    let mut out = String::new();
    let _ = writeln!(
        out,
        "Ingest ({:?}, update) — {} file(s) changed since {}\n",
        level,
        reconciliation.changed.len(),
        &base[..8.min(base.len())]
    );
    if reconciliation.changed.is_empty() {
        let _ = writeln!(
            out,
            "No code changes; the wiki is in sync with this base.\n"
        );
    } else if stale.is_empty() {
        let _ = writeln!(
            out,
            "No existing pages claim the changed files via sources."
        );
    } else {
        let _ = writeln!(out, "Stale pages (their sources changed):");
        for (id, files) in &shown_stale {
            let shown_files = ingest_take(files, limit, byte_budget);
            let shown = shown_files
                .iter()
                .map(|path| report::terminal_safe(path))
                .collect::<Vec<_>>()
                .join(", ");
            let more = if files.len() > shown_files.len() {
                format!(" (+{} more)", files.len() - shown_files.len())
            } else {
                String::new()
            };
            let _ = writeln!(out, "- {id}  <- {shown}{more}");
        }
        if shown_stale.len() < stale.len() {
            let _ = writeln!(
                out,
                "- ... {} stale page(s) omitted",
                stale.len() - shown_stale.len()
            );
        }
    }
    if !shown_newly_seeded.is_empty() {
        let _ = writeln!(out, "\nNew module stub(s) seeded:");
        for s in &shown_newly_seeded {
            let _ = writeln!(out, "- {s}");
        }
        if shown_newly_seeded.len() < seeded.len() {
            let _ = writeln!(
                out,
                "- ... {} omitted",
                seeded.len() - shown_newly_seeded.len()
            );
        }
    }
    if !uncovered.is_empty() {
        let shown = shown_uncovered
            .iter()
            .map(|path| report::terminal_safe(path))
            .collect::<Vec<_>>()
            .join(", ");
        let more = if uncovered.len() > shown_uncovered.len() {
            format!(" (+{} more)", uncovered.len() - shown_uncovered.len())
        } else {
            String::new()
        };
        let _ = writeln!(
            out,
            "\nChanged but not covered by any page's sources: {shown}{more}\nIf any deserve documentation, add pages for them (with --sources)."
        );
    }
    let deep_step = if level == IngestLevel::Deep {
        "\n3. For heavily changed files, add or update per-file sub-pages under their module's code/ path."
    } else {
        ""
    };
    let mark_guidance = mark_command
        .as_deref()
        .unwrap_or("(unavailable: this project has no Git HEAD, so no sync point can be recorded)");
    let _ = write!(
        out,
        "\nWorklist — do these now:\n1. For each stale page: `wookie read <id>`, review the changed files (git diff {base} -- <files>), update the page with `wookie write <id>`.\n2. Fill any seeded stubs.{deep_step}\n{}. Run `wookie doctor`, fix every error, rerun ingest if this worklist changed, then use the exact command: `{mark_guidance}`.\n\nReconciliation receipt: {receipt}{}",
        if level == IngestLevel::Deep { 4 } else { 3 },
        if limit.is_some() {
            "\nOutput is bounded; rerun with `--all` for the exhaustive display. The receipt always covers the complete worklist."
        } else {
            ""
        }
    );
    Ok(out.trim_end().to_string())
}

fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

pub fn unlock(w: &Wiki, section: &str, minutes: u64, json: bool) -> Result<String> {
    let msg = w.unlock(section, minutes)?;
    if json {
        return Ok(serde_json::json!({"section": section, "minutes": minutes}).to_string());
    }
    Ok(msg)
}

pub fn lock(w: &Wiki, section: &str, json: bool) -> Result<String> {
    let msg = w.relock(section)?;
    if json {
        return Ok(serde_json::json!({"section": section, "locked": true}).to_string());
    }
    Ok(msg)
}

fn wiki_revision(w: &Wiki) -> Option<String> {
    std::process::Command::new("git")
        .arg("-C")
        .arg(&w.dir)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|revision| !revision.is_empty())
}

fn project_source_exists(root: &Path, source: &str) -> Result<bool> {
    let root = root
        .canonicalize()
        .with_context(|| format!("resolving project root {}", root.display()))?;
    if !root.is_dir() {
        bail!("project root {} is not a directory", root.display());
    }
    let candidate = root.join(source.trim_end_matches('/'));
    let resolved = match candidate.canonicalize() {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| format!("resolving source {}", candidate.display()))
        }
    };
    if !resolved.starts_with(&root) {
        bail!(
            "source '{}' resolves outside project root {}",
            source,
            root.display()
        );
    }
    Ok(true)
}

fn overlay_diagnostics(w: &Wiki, overlay: &publish::PublishOverlay) -> Vec<report::Diagnostic> {
    let mut diagnostics = Vec::new();
    let live_pages = w.all_pages();
    let live_ids: HashSet<&str> = live_pages.iter().map(|page| page.id.as_str()).collect();
    let live_broken: HashSet<(String, String)> = live_pages
        .iter()
        .flat_map(|page| {
            page.links()
                .into_iter()
                .filter(|target| !live_ids.contains(target.as_str()))
                .map(|target| (page.id.clone(), target))
                .collect::<Vec<_>>()
        })
        .collect();
    let ids: HashSet<&str> = overlay.page_ids().collect();
    let linked: HashSet<String> = overlay.pages().flat_map(Page::links).collect();
    let sections = w.sections();
    for page in overlay.pages() {
        for target in page.links() {
            if !ids.contains(target.as_str()) {
                let severity = if live_broken.contains(&(page.id.clone(), target.clone())) {
                    report::Severity::Warning
                } else {
                    report::Severity::Error
                };
                diagnostics.push(
                    report::Diagnostic::new(
                        report::code::BROKEN_LINK,
                        severity,
                        format!("proposed page '{}' links to missing '{target}'", page.id),
                    )
                    .page(&page.id)
                    .source(target)
                    .suggestion("Create the target in the same change set or correct the link."),
                );
            }
        }
        if page.fm.description.is_empty() || page.fm.description.starts_with("TODO") {
            diagnostics.push(
                report::Diagnostic::new(
                    "page_description_missing",
                    report::Severity::Warning,
                    format!("proposed page '{}' has no useful description", page.id),
                )
                .page(&page.id),
            );
        }
        if page.summary().is_empty() || page.summary().starts_with("TODO") {
            diagnostics.push(
                report::Diagnostic::new(
                    report::code::MISSING_SUMMARY,
                    report::Severity::Warning,
                    format!("proposed page '{}' has no standalone summary", page.id),
                )
                .page(&page.id),
            );
        }
        if page.id != "index" && !linked.contains(&page.id) {
            let severity = match w.publish.orphan_policy {
                crate::config::OrphanPolicy::Warn => report::Severity::Warning,
                crate::config::OrphanPolicy::Error => report::Severity::Error,
            };
            diagnostics.push(
                report::Diagnostic::new(
                    report::code::ORPHAN_PAGE,
                    severity,
                    format!("proposed page '{}' has no inbound wiki link", page.id),
                )
                .page(&page.id),
            );
        }
        match section_of(&page.id) {
            Some(section) if sections.contains_key(section) => {}
            _ if page.id == "index" => {}
            _ => diagnostics.push(
                report::Diagnostic::new(
                    "page_unfiled",
                    report::Severity::Warning,
                    format!("proposed page '{}' is outside configured sections", page.id),
                )
                .page(&page.id),
            ),
        }
        let file_line_sources = audit::file_line_sources(page);
        let provenance_severity = if w.audit.source_provenance {
            report::Severity::Error
        } else {
            report::Severity::Warning
        };
        if page.id.starts_with("code/") && page.fm.sources.is_empty() {
            diagnostics.push(
                report::Diagnostic::new(
                    "source_metadata_missing",
                    provenance_severity,
                    format!("proposed code page '{}' declares no sources", page.id),
                )
                .page(&page.id),
            );
        }
        if page.id.starts_with("code/") && file_line_sources.is_empty() {
            diagnostics.push(
                report::Diagnostic::new(
                    "file_source_missing",
                    provenance_severity,
                    format!(
                        "proposed code page '{}' has no File: provenance line",
                        page.id
                    ),
                )
                .page(&page.id),
            );
        }
        if w.audit.source_provenance {
            let metadata_sources: BTreeSet<_> = page
                .fm
                .sources
                .iter()
                .filter_map(|source| audit::normalize_source(source).ok())
                .collect();
            let file_sources: BTreeSet<_> = file_line_sources
                .iter()
                .filter_map(|source| audit::normalize_source(source).ok())
                .collect();
            if !file_sources.is_empty() && file_sources != metadata_sources {
                diagnostics.push(
                    report::Diagnostic::new(
                        "source_metadata_mismatch",
                        report::Severity::Error,
                        format!(
                            "proposed page '{}' has different File: and frontmatter sources",
                            page.id
                        ),
                    )
                    .page(&page.id),
                );
            }
            if let Some(root) = w.config.project_roots.first() {
                let mut sources = page.fm.sources.clone();
                sources.extend(file_line_sources);
                sources.sort();
                sources.dedup();
                for source in &sources {
                    let source_path = Path::new(source);
                    let valid = !source.is_empty()
                        && !source_path.is_absolute()
                        && source_path
                            .components()
                            .all(|component| matches!(component, std::path::Component::Normal(_)));
                    if !valid {
                        diagnostics.push(
                            report::Diagnostic::new(
                                report::code::INVALID_SOURCE,
                                report::Severity::Error,
                                format!("page '{}' has invalid source '{source}'", page.id),
                            )
                            .page(&page.id)
                            .source(source),
                        );
                    } else {
                        let exists = project_source_exists(Path::new(root), source);
                        let existed_before = live_pages.iter().any(|live| {
                            live.id == page.id && live.fm.sources.iter().any(|item| item == source)
                        });
                        match exists {
                            Ok(true) => {}
                            Ok(false) => diagnostics.push(
                                report::Diagnostic::new(
                                    report::code::SOURCE_MISSING,
                                    if existed_before {
                                        report::Severity::Warning
                                    } else {
                                        report::Severity::Error
                                    },
                                    format!("page '{}' source '{source}' does not exist", page.id),
                                )
                                .page(&page.id)
                                .source(source),
                            ),
                            Err(error) => diagnostics.push(
                                report::Diagnostic::new(
                                    report::code::INVALID_SOURCE,
                                    report::Severity::Error,
                                    format!(
                                        "page '{}' source '{source}' cannot be verified: {error:#}",
                                        page.id
                                    ),
                                )
                                .page(&page.id)
                                .source(source),
                            ),
                        }
                    }
                }
            }
        }
    }
    for (section, config) in sections {
        for required in &config.required {
            let id = format!("{section}/{required}");
            if !ids.contains(id.as_str()) {
                diagnostics.push(
                    report::Diagnostic::new(
                        "required_page_missing",
                        if w.exists(&id) {
                            report::Severity::Error
                        } else {
                            report::Severity::Warning
                        },
                        format!("proposed catalog is missing required page '{id}'"),
                    )
                    .page(id),
                );
            }
        }
        if config.kind == wiki::SectionKind::Rules {
            let checks = format!("{section}/checks");
            if !ids.contains(checks.as_str()) {
                diagnostics.push(
                    report::Diagnostic::new(
                        report::code::MISSING_CHECKS,
                        if w.exists(&checks) {
                            report::Severity::Error
                        } else {
                            report::Severity::Warning
                        },
                        format!("rules section '{section}' has no checks page"),
                    )
                    .page(checks),
                );
            }
        }
    }
    diagnostics
}

fn rule_sections_for_plan(w: &Wiki, plan: &publish::PublishPlan) -> Vec<String> {
    let sections = w.sections();
    let mut affected = HashSet::new();
    for operation in &plan.operations {
        if let Some(section) = section_of(&operation.page) {
            if sections
                .get(section)
                .is_some_and(|config| config.kind == wiki::SectionKind::Rules)
            {
                affected.insert(section.to_string());
            }
        }
    }
    let mut affected: Vec<_> = affected.into_iter().collect();
    affected.sort();
    affected
}

#[derive(Debug, Clone, Copy, Default)]
struct RuleBriefingProjection {
    sections_total: usize,
    sections_returned: usize,
    checks_total: usize,
    checks_returned: usize,
    rules_total: usize,
    rules_returned: usize,
}

struct IndexedRuleSection<'a> {
    name: &'a str,
    description: &'a str,
    checks: Option<&'a Page>,
    rules: Vec<&'a Page>,
}

struct SelectedRuleSection<'a> {
    name: &'a str,
    description: &'a str,
    checks: Option<&'a Page>,
    checks_available: bool,
    rules: Vec<&'a Page>,
    rules_total: usize,
}

/// Index rule pages once so repeatedly tightening a compact projection is
/// proportional to the material returned, not the size of the whole wiki.
struct RuleBriefingIndex<'a> {
    sections: Vec<IndexedRuleSection<'a>>,
    diagnostics: Vec<report::Diagnostic>,
    counts: RuleBriefingProjection,
}

impl<'a> RuleBriefingIndex<'a> {
    fn new(pages: &'a [Page], rules: &'a [(String, wiki::SectionConfig)]) -> Self {
        let lookup = rules
            .iter()
            .enumerate()
            .map(|(index, (name, _))| (name.as_str(), index))
            .collect::<HashMap<_, _>>();
        let mut sections = rules
            .iter()
            .map(|(name, config)| IndexedRuleSection {
                name,
                description: &config.description,
                checks: None,
                rules: Vec::new(),
            })
            .collect::<Vec<_>>();

        for page in pages {
            let Some((section, relative)) = page.id.split_once('/') else {
                continue;
            };
            let Some(index) = lookup.get(section).copied() else {
                continue;
            };
            if relative == "checks" {
                sections[index].checks = Some(page);
            } else {
                sections[index].rules.push(page);
            }
        }
        for section in &mut sections {
            section.rules.sort_by(|left, right| left.id.cmp(&right.id));
        }

        let diagnostics = sections
            .iter()
            .filter(|section| section.checks.is_none())
            .map(|section| {
                let checks_id = format!("{}/checks", section.name);
                report::Diagnostic::new(
                    report::code::MISSING_CHECKS,
                    report::Severity::Error,
                    format!("rules section '{}' has no checks page", section.name),
                )
                .page(&checks_id)
                .suggestion("Add the checks workflow before treating critique as executable.")
            })
            .collect::<Vec<_>>();
        let counts = RuleBriefingProjection {
            sections_total: sections.len(),
            checks_total: sections
                .iter()
                .filter(|section| section.checks.is_some())
                .count(),
            rules_total: sections.iter().map(|section| section.rules.len()).sum(),
            ..Default::default()
        };
        Self {
            sections,
            diagnostics,
            counts,
        }
    }

    fn project(
        &self,
        include_bodies: bool,
        section_limit: usize,
        check_limit: usize,
        rule_limit: usize,
    ) -> (Vec<serde_json::Value>, RuleBriefingProjection) {
        let (selected, projection) = self.select(section_limit, check_limit, rule_limit);
        let documents = selected
            .into_iter()
            .map(|section| {
                serde_json::json!({
                    "section": section.name,
                    "description": section.description,
                    "checks": section.checks.map(|page| rule_page_document(page, include_bodies)),
                    "rules": section.rules.into_iter().map(|page| rule_page_document(page, include_bodies)).collect::<Vec<_>>(),
                })
            })
            .collect();
        (documents, projection)
    }

    fn select(
        &self,
        section_limit: usize,
        check_limit: usize,
        rule_limit: usize,
    ) -> (Vec<SelectedRuleSection<'a>>, RuleBriefingProjection) {
        let mut projection = self.counts;
        let mut selected = Vec::new();
        for section in self.sections.iter().take(section_limit) {
            let checks = if projection.checks_returned < check_limit {
                section.checks.inspect(|_| {
                    projection.checks_returned += 1;
                })
            } else {
                None
            };
            let remaining = rule_limit.saturating_sub(projection.rules_returned);
            let rule_pages = section
                .rules
                .iter()
                .take(remaining)
                .copied()
                .collect::<Vec<_>>();
            projection.rules_returned += rule_pages.len();
            selected.push(SelectedRuleSection {
                name: section.name,
                description: section.description,
                checks,
                checks_available: section.checks.is_some(),
                rules: rule_pages,
                rules_total: section.rules.len(),
            });
        }
        projection.sections_returned = selected.len();
        (selected, projection)
    }
}

fn rule_page_document(page: &Page, include_body: bool) -> serde_json::Value {
    if include_body {
        serde_json::json!({"id": page.id, "body": page.body})
    } else {
        serde_json::json!({
            "id": page.id,
            "title": page.fm.title,
            "description": page.fm.description,
            "summary": page.summary(),
            "body_omitted": true,
            "read_command": format!("wookie read {}", page.id),
        })
    }
}

/// Build the exhaustive structured rules/checks briefing used by
/// transactional publish preflight. Critique applies a bounded projection of
/// the same index unless the operator explicitly selects `--all`.
fn rule_briefing(
    pages: &[Page],
    rules: &[(String, wiki::SectionConfig)],
    include_bodies: bool,
) -> (Vec<serde_json::Value>, Vec<report::Diagnostic>) {
    let index = RuleBriefingIndex::new(pages, rules);
    let (documents, _) = index.project(include_bodies, usize::MAX, usize::MAX, usize::MAX);
    (documents, index.diagnostics)
}

fn locked_scope_for_plan(w: &Wiki, plan: &publish::PublishPlan) -> (Vec<String>, BTreeSet<String>) {
    let sections = w.sections();
    let mut affected_sections = BTreeSet::new();
    let mut affected_pages = BTreeSet::new();
    for operation in &plan.operations {
        let Some(section) = section_of(&operation.page) else {
            continue;
        };
        if sections
            .get(section)
            .is_some_and(wiki::SectionConfig::is_locked)
        {
            affected_sections.insert(section.to_string());
            affected_pages.insert(operation.page.clone());
        }
    }
    (affected_sections.into_iter().collect(), affected_pages)
}

fn prepare_publish(
    w: &Wiki,
    raw: &str,
    user_approved: bool,
) -> Result<(publish::ChangeSet, publish::Preflight, Option<String>)> {
    let change_set = publish::ChangeSet::parse(raw)?;
    let revision = wiki_revision(w);
    let mut snapshot =
        report::Snapshot::new(&w.slug).wiki_content_hash(publish::raw_catalog_sha256(w)?);
    if let Some(revision) = &revision {
        snapshot = snapshot.wiki_revision(revision);
    }
    let mut checked = publish::preflight(w, &change_set, revision.as_deref(), snapshot)?;
    let overlay_diagnostics = overlay_diagnostics(w, &checked.overlay);
    let overlay_pages = checked.overlay.pages().cloned().collect::<Vec<_>>();
    let expected_doctor = audit::audit_pages(
        w,
        &audit::AuditOptions::default(),
        "publish-doctor",
        &overlay_pages,
    )?;
    checked.add_diagnostics(overlay_diagnostics);
    let affected_rules = rule_sections_for_plan(w, &checked.plan);
    let section_catalog = w.sections();
    let critique_rules = affected_rules
        .iter()
        .filter_map(|section| {
            section_catalog
                .get(section)
                .cloned()
                .map(|config| (section.clone(), config))
        })
        .collect::<Vec<_>>();
    let (critique_documents, critique_diagnostics) =
        rule_briefing(&overlay_pages, &critique_rules, false);
    let affected_checks = affected_rules
        .iter()
        .map(|section| format!("{section}/checks"))
        .collect::<BTreeSet<_>>();
    for diagnostic in &mut checked.report.diagnostics {
        if diagnostic.code == report::code::MISSING_CHECKS
            && diagnostic
                .page
                .as_ref()
                .is_some_and(|page| affected_checks.contains(page))
        {
            diagnostic.severity = report::Severity::Error;
        }
    }
    checked
        .report
        .insert_data("applicable_rule_checks", serde_json::json!(affected_checks));
    checked.report.insert_data(
        "critique_required",
        serde_json::json!(!affected_rules.is_empty()),
    );
    let critique_status = if affected_rules.is_empty() {
        "not_required"
    } else if critique_diagnostics
        .iter()
        .any(|diagnostic| diagnostic.severity == report::Severity::Error)
    {
        "blocked"
    } else {
        "manual_review_required"
    };
    let mut expected_critique = report::Report::with_diagnostics(
        "publish-critique",
        expected_doctor.snapshot.clone(),
        critique_diagnostics,
    );
    expected_critique.insert_data("status", serde_json::json!(critique_status));
    expected_critique.insert_data("affected_rule_sections", serde_json::json!(affected_rules));
    expected_critique.insert_data("rules", serde_json::json!(critique_documents));
    expected_critique.insert_data(
        "evaluation_contract",
        serde_json::json!(if critique_status == "manual_review_required" {
            "The deterministic briefing is complete. A reviewer must execute each checks page against the proposed page diff; Wookie does not infer compliance with natural-language rules."
        } else if critique_status == "blocked" {
            "Critique cannot be executed until every affected rules section has a checks page."
        } else {
            "No rules section is changed by this publication."
        }),
    );
    checked
        .report
        .insert_data("expected_doctor", serde_json::to_value(&expected_doctor)?);
    checked.report.insert_data(
        "expected_critique",
        serde_json::to_value(&expected_critique)?,
    );
    if user_approved {
        checked
            .report
            .diagnostics
            .retain(|diagnostic| diagnostic.code != report::code::RULE_LOCKED);
    }
    checked.report.recompute_summary();
    Ok((change_set, checked, revision))
}

/// Output controls for publish checks and stored rule reviews. The normal
/// path is deliberately bounded; exact replacement images are available only
/// when the caller explicitly opts into an exhaustive response.
#[derive(Debug, Clone, Copy, Default)]
pub struct PublishOutputOptions {
    pub tokens: Option<usize>,
    pub full_diff: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
struct DiffLine {
    line: usize,
    text: String,
}

#[derive(Debug, Clone, serde::Serialize)]
struct DiffRange {
    start: usize,
    end: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
struct CompactPageDiff {
    page: String,
    kind: &'static str,
    before_lines: usize,
    after_lines: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    before_changed: Option<DiffRange>,
    #[serde(skip_serializing_if = "Option::is_none")]
    after_changed: Option<DiffRange>,
    before_excerpt: Vec<DiffLine>,
    after_excerpt: Vec<DiffLine>,
    omitted_before_changed_lines: usize,
    omitted_after_changed_lines: usize,
}

const PUBLISH_DIFF_EXCERPT_LINES: usize = 3;
const PUBLISH_PREVIEW_SCHEMA: &str = "wookie.publish-preview/v1";

fn changed_range(start: usize, len: usize) -> Option<DiffRange> {
    (len > 0).then(|| DiffRange {
        start: start + 1,
        end: start + len,
    })
}

fn compact_page_diff(diff: publish::PageDiff) -> CompactPageDiff {
    let before: Vec<&str> = diff
        .before
        .as_deref()
        .map(str::lines)
        .into_iter()
        .flatten()
        .collect();
    let after: Vec<&str> = diff
        .after
        .as_deref()
        .map(str::lines)
        .into_iter()
        .flatten()
        .collect();
    let mut prefix = 0;
    while prefix < before.len() && prefix < after.len() && before[prefix] == after[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < before.len().saturating_sub(prefix)
        && suffix < after.len().saturating_sub(prefix)
        && before[before.len() - suffix - 1] == after[after.len() - suffix - 1]
    {
        suffix += 1;
    }
    let before_changed_len = before.len().saturating_sub(prefix + suffix);
    let after_changed_len = after.len().saturating_sub(prefix + suffix);
    let before_excerpt = before
        .iter()
        .skip(prefix)
        .take(before_changed_len.min(PUBLISH_DIFF_EXCERPT_LINES))
        .enumerate()
        .map(|(offset, line)| DiffLine {
            line: prefix + offset + 1,
            text: retrieval::compact_excerpt(line),
        })
        .collect::<Vec<_>>();
    let after_excerpt = after
        .iter()
        .skip(prefix)
        .take(after_changed_len.min(PUBLISH_DIFF_EXCERPT_LINES))
        .enumerate()
        .map(|(offset, line)| DiffLine {
            line: prefix + offset + 1,
            text: retrieval::compact_excerpt(line),
        })
        .collect::<Vec<_>>();
    let kind = match (&diff.before, &diff.after) {
        (None, Some(_)) => "create",
        (Some(_), None) => "delete",
        _ => "update",
    };
    CompactPageDiff {
        page: diff.page,
        kind,
        before_lines: before.len(),
        after_lines: after.len(),
        before_changed: changed_range(prefix, before_changed_len),
        after_changed: changed_range(prefix, after_changed_len),
        omitted_before_changed_lines: before_changed_len.saturating_sub(before_excerpt.len()),
        omitted_after_changed_lines: after_changed_len.saturating_sub(after_excerpt.len()),
        before_excerpt,
        after_excerpt,
    }
}

fn publish_budget(w: &Wiki, options: &PublishOutputOptions) -> Result<usize> {
    if options.full_diff && options.tokens.is_some() {
        bail!("--tokens and --full-diff are mutually exclusive");
    }
    let budget = options.tokens.unwrap_or(w.publish.output_tokens);
    if budget < 256 {
        bail!("publish preview token budget must be at least 256");
    }
    Ok(budget)
}

fn preview_item_cap(budget: usize) -> usize {
    // Prevent a 10,000-operation manifest from first materializing an equally
    // large response only to trim it. The subsequent exact budget loop may
    // reduce this conservative cap further.
    (budget / 12).clamp(1, 512)
}

fn bounded_publish_json(
    checked: &publish::Preflight,
    budget: usize,
    extra: Option<(&str, serde_json::Value)>,
) -> Result<String> {
    let review_token = checked.review_token()?;
    let cap = preview_item_cap(budget);
    let compact_diffs = checked
        .diffs_limited(cap)
        .into_iter()
        .map(compact_page_diff)
        .collect::<Vec<_>>();
    let compact_diagnostics = [
        report::Severity::Error,
        report::Severity::Warning,
        report::Severity::Info,
    ]
    .into_iter()
    .flat_map(|severity| {
        checked
            .report
            .diagnostics
            .iter()
            .filter(move |diagnostic| diagnostic.severity == severity)
    })
    .take(cap)
    .cloned()
    .collect::<Vec<_>>();
    let mut diagnostics = compact_diagnostics.len();
    let mut requested = checked.plan.requested.len().min(cap);
    let mut operations = checked.plan.operations.len().min(cap);
    let mut diffs = compact_diffs.len();
    let mut include_report_data = true;

    loop {
        let report = report::Report {
            schema: checked.report.schema.clone(),
            command: checked.report.command.clone(),
            generated_at: checked.report.generated_at.clone(),
            snapshot: checked.report.snapshot.clone(),
            summary: checked.report.summary.clone(),
            diagnostics: compact_diagnostics
                .iter()
                .take(diagnostics)
                .cloned()
                .collect(),
            data: if include_report_data {
                checked.report.data.clone()
            } else {
                BTreeMap::new()
            },
        };
        // Keep the original summary: it describes the full deterministic
        // preflight even when detail rows are omitted from this projection.
        let plan = publish::PublishPlan {
            schema: checked.plan.schema.clone(),
            base_revision: checked.plan.base_revision.clone(),
            observed_revision: checked.plan.observed_revision.clone(),
            observed_content_hash: checked.plan.observed_content_hash.clone(),
            requested: checked
                .plan
                .requested
                .iter()
                .take(requested)
                .cloned()
                .collect(),
            operations: checked
                .plan
                .operations
                .iter()
                .take(operations)
                .cloned()
                .collect(),
        };
        let shown_diffs = compact_diffs
            .iter()
            .take(diffs)
            .cloned()
            .collect::<Vec<_>>();
        let omitted_excerpt_lines = shown_diffs
            .iter()
            .map(|diff| {
                diff.omitted_before_changed_lines
                    .saturating_add(diff.omitted_after_changed_lines)
            })
            .sum::<usize>();
        let mut value = serde_json::json!({
            "schema": PUBLISH_PREVIEW_SCHEMA,
            "report": report,
            "plan": plan,
            "diff_mode": "compact",
            "diffs": shown_diffs,
            "applied": false,
            "review_token": review_token,
            "omissions": {
                "diagnostics": checked.report.diagnostics.len().saturating_sub(diagnostics),
                "report_data_fields": if include_report_data { 0 } else { checked.report.data.len() },
                "requested_changes": checked.plan.requested.len().saturating_sub(requested),
                "operations": checked.plan.operations.len().saturating_sub(operations),
                "diffs": checked.diff_count().saturating_sub(diffs),
                "diff_excerpt_lines": omitted_excerpt_lines,
            },
            "next_command": "wookie publish --check --full-diff < changes.toml",
            "telemetry": {
                "estimated_tokens": 0,
                "budget_tokens": budget,
            },
        });
        if let Some((key, extra_value)) = extra.as_ref() {
            value
                .as_object_mut()
                .expect("publish preview is an object")
                .insert((*key).to_string(), extra_value.clone());
        }
        for _ in 0..3 {
            let rendered = serde_json::to_string(&value)?;
            value["telemetry"]["estimated_tokens"] =
                serde_json::json!(retrieval::estimate_tokens(&rendered));
        }
        let rendered = serde_json::to_string(&value)?;
        if retrieval::estimate_tokens(&rendered) <= budget {
            return Ok(rendered);
        }

        if include_report_data && !checked.report.data.is_empty() {
            include_report_data = false;
        } else if requested > 0 {
            requested -= 1;
        } else if diffs > 0 {
            diffs -= 1;
        } else if operations > 0 {
            operations -= 1;
        } else if diagnostics > 0 {
            diagnostics -= 1;
        } else {
            bail!(
                "publish preview metadata exceeds the {budget}-token response budget; raise --tokens"
            );
        }
    }
}

fn range_text(range: &Option<DiffRange>) -> String {
    match range {
        Some(range) if range.start == range.end => range.start.to_string(),
        Some(range) => format!("{}-{}", range.start, range.end),
        None => "none".to_string(),
    }
}

fn human_diff_chunk(diff: &CompactPageDiff) -> String {
    let mut out = format!(
        "{} {} — {} before / {} after lines; changed before {}, after {}\n",
        diff.kind,
        diff.page,
        diff.before_lines,
        diff.after_lines,
        range_text(&diff.before_changed),
        range_text(&diff.after_changed)
    );
    for line in &diff.before_excerpt {
        let _ = writeln!(out, "  -{}: {}", line.line, line.text);
    }
    for line in &diff.after_excerpt {
        let _ = writeln!(out, "  +{}: {}", line.line, line.text);
    }
    let omitted = diff
        .omitted_before_changed_lines
        .saturating_add(diff.omitted_after_changed_lines);
    if omitted > 0 {
        let _ = writeln!(out, "  … {omitted} changed line(s) omitted");
    }
    out
}

fn nested_report_counts(value: Option<&serde_json::Value>) -> (u64, u64) {
    let summary = value.and_then(|value| value.get("summary"));
    (
        summary
            .and_then(|summary| summary.get("errors"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        summary
            .and_then(|summary| summary.get("warnings"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
    )
}

/// Compact projection of the two deterministic reports produced for every
/// publish preflight. The complete stable schemas remain available in JSON;
/// human checks must still make their outcome visible by default.
fn publish_expected_checks_human(checked: &publish::Preflight) -> String {
    let doctor = checked.report.data.get("expected_doctor");
    let critique = checked.report.data.get("expected_critique");
    let (doctor_errors, doctor_warnings) = nested_report_counts(doctor);
    let (critique_errors, critique_warnings) = nested_report_counts(critique);
    let status = critique
        .and_then(|value| value.get("data"))
        .and_then(|data| data.get("status"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unavailable");
    let sections = critique
        .and_then(|value| value.get("data"))
        .and_then(|data| data.get("affected_rule_sections"))
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let section_names = sections
        .iter()
        .filter_map(serde_json::Value::as_str)
        .take(8)
        .map(report::terminal_safe)
        .collect::<Vec<_>>();
    let omitted_sections = sections.len().saturating_sub(section_names.len());
    let section_detail = if section_names.is_empty() {
        String::new()
    } else if omitted_sections == 0 {
        format!(" [{}]", section_names.join(", "))
    } else {
        format!(" [{}; {omitted_sections} more]", section_names.join(", "))
    };
    let check_count = checked
        .report
        .data
        .get("applicable_rule_checks")
        .and_then(serde_json::Value::as_array)
        .map_or(0, Vec::len);
    let check_readiness = match status {
        "not_required" => "not required".to_string(),
        "manual_review_required" => format!("ready for manual review ({check_count} applicable)"),
        "blocked" => format!("blocked by missing or invalid checks ({check_count} applicable)"),
        _ => "unavailable".to_string(),
    };

    format!(
        "Expected checks:\n- Doctor: {doctor_errors} error(s), {doctor_warnings} warning(s).\n- Critique: {}; {critique_errors} error(s), {critique_warnings} warning(s); {} affected rule section(s){section_detail}; checks {check_readiness}.\n",
        status.replace('_', " "),
        sections.len(),
    )
}

fn bounded_publish_human(
    checked: &publish::Preflight,
    budget: usize,
    prefix: Option<&str>,
) -> Result<String> {
    let review_token = checked.review_token()?;
    let cap = preview_item_cap(budget);
    let operation_chunks = checked
        .plan
        .operations
        .iter()
        .take(cap)
        .map(|operation| {
            format!(
                "{:?} {} — {}\n",
                operation.kind,
                operation.page,
                retrieval::compact_excerpt(&operation.reason)
            )
        })
        .collect::<Vec<_>>();
    let diagnostic_chunks = checked
        .report
        .diagnostics
        .iter()
        .filter(|diagnostic| diagnostic.severity == report::Severity::Error)
        .chain(
            checked
                .report
                .diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.severity == report::Severity::Warning),
        )
        .chain(
            checked
                .report
                .diagnostics
                .iter()
                .filter(|diagnostic| diagnostic.severity == report::Severity::Info),
        )
        .take(cap)
        .map(|diagnostic| {
            let location = diagnostic
                .page
                .as_deref()
                .or(diagnostic.source.as_deref())
                .map(|value| format!(" [{value}]"))
                .unwrap_or_default();
            let mut chunk = format!(
                "{} {}{}: {}\n",
                diagnostic.severity.as_str().to_uppercase(),
                diagnostic.code,
                location,
                retrieval::compact_excerpt(&diagnostic.message)
            );
            if let Some(suggestion) = &diagnostic.suggestion {
                let _ = writeln!(
                    chunk,
                    "  Suggestion: {}",
                    retrieval::compact_excerpt(suggestion)
                );
            }
            chunk
        })
        .collect::<Vec<_>>();
    let diff_chunks = checked
        .diffs_limited(cap)
        .into_iter()
        .map(compact_page_diff)
        .map(|diff| human_diff_chunk(&diff))
        .collect::<Vec<_>>();
    let mut operations = operation_chunks.len();
    let mut diagnostics = diagnostic_chunks.len();
    let mut diffs = diff_chunks.len();

    loop {
        let mut out = String::new();
        if let Some(prefix) = prefix {
            let _ = writeln!(out, "{}", prefix.trim_end());
        }
        let _ = writeln!(
            out,
            "Publish plan: {} operation(s), {} error(s), {} warning(s)",
            checked.plan.operations.len(),
            checked.report.summary.errors,
            checked.report.summary.warnings
        );
        let _ = writeln!(out, "Review token: {review_token}");
        out.push('\n');
        out.push_str(&publish_expected_checks_human(checked));
        if operations > 0 {
            out.push_str("\nOperations:\n");
            for chunk in operation_chunks.iter().take(operations) {
                out.push_str(chunk);
            }
        }
        if diagnostics > 0 {
            out.push_str("\nDiagnostics:\n");
            for chunk in diagnostic_chunks.iter().take(diagnostics) {
                out.push_str(chunk);
            }
        }
        if diffs > 0 {
            out.push_str("\nCompact diff summaries:\n");
            for chunk in diff_chunks.iter().take(diffs) {
                out.push_str(chunk);
            }
        }
        let omitted_operations = checked.plan.operations.len().saturating_sub(operations);
        let omitted_diagnostics = checked.report.diagnostics.len().saturating_sub(diagnostics);
        let omitted_diffs = checked.diff_count().saturating_sub(diffs);
        if omitted_operations + omitted_diagnostics + omitted_diffs > 0 {
            let _ = writeln!(
                out,
                "\nOmitted: {omitted_operations} operation(s), {omitted_diagnostics} diagnostic(s), {omitted_diffs} diff summary(s)."
            );
        }
        out.push_str(
            "Exact replacement images: `wookie publish --check --full-diff < changes.toml`.\nMachine-readable expected reports: `wookie --json publish --check < changes.toml`.\n",
        );
        let telemetry = format!("Estimated 0 / {budget} tokens.\n");
        let estimate = retrieval::estimate_tokens(&format!("{out}{telemetry}"));
        let mut rendered = format!("{out}Estimated {estimate} / {budget} tokens.\n");
        let final_estimate = retrieval::estimate_tokens(&rendered);
        if final_estimate != estimate {
            rendered = format!("{out}Estimated {final_estimate} / {budget} tokens.\n");
        }
        if retrieval::estimate_tokens(&rendered) <= budget {
            return Ok(rendered.trim_end().to_string());
        }
        if diffs > 0 {
            diffs -= 1;
        } else if operations > 0 {
            operations -= 1;
        } else if diagnostics > 0 {
            diagnostics -= 1;
        } else {
            bail!(
                "publish preview metadata exceeds the {budget}-token response budget; raise --tokens"
            );
        }
    }
}

fn render_publish_preview(
    w: &Wiki,
    checked: &publish::Preflight,
    options: &PublishOutputOptions,
    json: bool,
    human_prefix: Option<&str>,
    json_extra: Option<(&str, serde_json::Value)>,
) -> Result<String> {
    if options.full_diff {
        if options.tokens.is_some() {
            bail!("--tokens and --full-diff are mutually exclusive");
        }
        if json {
            let mut value = serde_json::json!({
                "schema": PUBLISH_PREVIEW_SCHEMA,
                "report": checked.report,
                "plan": checked.plan,
                "diff_mode": "full",
                "diffs": checked.diffs(),
                "applied": false,
                "review_token": checked.review_token()?,
                "omissions": {
                    "diagnostics": 0,
                    "report_data_fields": 0,
                    "requested_changes": 0,
                    "operations": 0,
                    "diffs": 0,
                    "diff_excerpt_lines": 0,
                },
            });
            if let Some((key, extra_value)) = json_extra {
                value
                    .as_object_mut()
                    .expect("publish preview is an object")
                    .insert(key.to_string(), extra_value);
            }
            return Ok(serde_json::to_string(&value)?);
        }
        let rendered = format!(
            "Review token: {}\n{}\n{}",
            checked.review_token()?,
            checked.render_human(true),
            publish_expected_checks_human(checked).trim_end(),
        );
        return Ok(match human_prefix {
            Some(prefix) => format!("{}\n{rendered}", prefix.trim_end()),
            None => rendered,
        });
    }

    let budget = publish_budget(w, options)?;
    if json {
        bounded_publish_json(checked, budget, json_extra)
    } else {
        bounded_publish_human(checked, budget, human_prefix)
    }
}

fn bounded_publish_apply_json(plan: &publish::PublishPlan, budget: usize) -> String {
    let cap = preview_item_cap(budget);
    let mut requested = plan.requested.len().min(cap);
    let mut operations = plan.operations.len().min(cap);
    loop {
        let compact_plan = publish::PublishPlan {
            schema: plan.schema.clone(),
            base_revision: plan.base_revision.clone(),
            observed_revision: plan.observed_revision.clone(),
            observed_content_hash: plan.observed_content_hash.clone(),
            requested: plan.requested.iter().take(requested).cloned().collect(),
            operations: plan.operations.iter().take(operations).cloned().collect(),
        };
        let mut value = serde_json::json!({
            "schema": "wookie.publish-result/v1",
            "applied": true,
            "summary": {
                "requested_changes": plan.requested.len(),
                "operations": plan.operations.len(),
            },
            "plan": compact_plan,
            "omissions": {
                "requested_changes": plan.requested.len().saturating_sub(requested),
                "operations": plan.operations.len().saturating_sub(operations),
            },
            "next_command": "wookie status",
            "telemetry": {
                "estimated_tokens": 0,
                "budget_tokens": budget,
            },
        });
        for _ in 0..3 {
            let rendered = serde_json::to_string(&value).expect("publish result is serializable");
            value["telemetry"]["estimated_tokens"] =
                serde_json::json!(retrieval::estimate_tokens(&rendered));
        }
        let rendered = serde_json::to_string(&value).expect("publish result is serializable");
        if retrieval::estimate_tokens(&rendered) <= budget {
            return rendered;
        }
        if requested > 0 {
            requested -= 1;
        } else if operations > 0 {
            operations -= 1;
        } else {
            // Applying has already mutated durable state. Never turn a
            // successful transaction into an apparent failure solely because
            // unusually long revision metadata cannot fit the configured
            // response envelope.
            return serde_json::json!({
                "schema": "wookie.publish-result/v1",
                "applied": true,
                "summary": {
                    "requested_changes": plan.requested.len(),
                    "operations": plan.operations.len(),
                },
                "plan_omitted": true,
                "next_command": "wookie status",
                "telemetry": {"budget_tokens": budget},
            })
            .to_string();
        }
    }
}

pub fn publish_changes(
    w: &Wiki,
    raw: &str,
    apply: bool,
    user_approved: bool,
    expect_plan: Option<&str>,
    output: &PublishOutputOptions,
    json: bool,
) -> Result<String> {
    if apply && (output.tokens.is_some() || output.full_diff) {
        bail!("--tokens and --full-diff apply only to publish checks");
    }
    let (change_set, checked, revision) = prepare_publish(w, raw, user_approved)?;
    if !apply {
        if expect_plan.is_some() {
            bail!("--expect-plan applies only with --apply");
        }
        return render_publish_preview(w, &checked, output, json, None, None);
    }
    if let Some(expected) = expect_plan {
        if !valid_sha256(expected) {
            bail!("--expect-plan must be a complete sha256: review token");
        }
        if checked.review_token()? != expected {
            bail!(
                "publish review token does not match the current manifest, catalog, configuration, revision, or plan; run --check again"
            );
        }
    }
    apply_prepared_publish(w, change_set, checked, revision, user_approved, json)
}

fn apply_prepared_publish(
    w: &Wiki,
    change_set: publish::ChangeSet,
    checked: publish::Preflight,
    revision: Option<String>,
    user_approved: bool,
    json: bool,
) -> Result<String> {
    if !checked.is_publishable() {
        bail!(
            "publish check found {} error(s); rerun with --check for the full report",
            checked.report.summary.errors
        );
    }
    let (locked_sections, approved_locked_pages) = locked_scope_for_plan(w, &checked.plan);
    if !locked_sections.is_empty() && !user_approved {
        bail!(
            "publish changes locked sections ({}); apply only after explicit approval with --user-approved",
            locked_sections.join(", ")
        );
    }
    let paths = checked.plan.relative_paths();
    if paths.is_empty() {
        bail!("publish plan contains no effective page operations");
    }
    let sections = w.sections();
    let relock_rule_sections = locked_sections
        .iter()
        .filter(|section| {
            sections
                .get(*section)
                .is_some_and(|config| config.kind == wiki::SectionKind::Rules)
        })
        .cloned()
        .collect::<BTreeSet<_>>();
    let message = crate::history::canonical_commit_message(
        &change_set
            .message
            .clone()
            .unwrap_or_else(|| format!("wookie: publish {} page operation(s)", paths.len())),
    );
    let result = publish::transact_with_approved_locked_pages(
        w,
        checked,
        revision.as_deref(),
        &approved_locked_pages,
        &relock_rule_sections,
        w.auto_commit.then_some(message.as_str()),
        |plan| {
            if !w.auto_commit {
                return Ok(());
            }
            let commit_paths = plan.relative_paths();
            if commit_paths.is_empty() {
                bail!("refusing to commit a publish plan without page paths");
            }
            let result = crate::history::commit_paths(&w.dir, &message, &commit_paths, &w.history);
            if let Err(error) = result {
                return match crate::history::reset_paths(&w.dir, &commit_paths) {
                    Ok(()) => Err(error),
                    Err(reset_error) => Err(anyhow::anyhow!(
                        "{error:#}; restoring the Git index failed: {reset_error}"
                    )),
                };
            }
            Ok(())
        },
    );
    let plan = result?;
    if json {
        Ok(bounded_publish_apply_json(&plan, w.publish.output_tokens))
    } else {
        Ok(format!(
            "Published {} page operation(s){}.",
            plan.operations.len(),
            if w.auto_commit {
                " in one wiki commit"
            } else {
                ""
            }
        ))
    }
}

pub fn publish_recover(
    w: &Wiki,
    action: publish::RecoveryAction,
    force_stale_lock: bool,
    json: bool,
) -> Result<String> {
    let before = publish::recovery_status(w)?;
    publish::recover(w, action, force_stale_lock)?;
    if json {
        Ok(serde_json::json!({"recovered": true, "previous": before}).to_string())
    } else {
        Ok("Recovered interrupted publication; the journal and lock were cleared.".into())
    }
}

fn validate_rule_changes_only(w: &Wiki, change_set: &publish::ChangeSet) -> Result<()> {
    let rules = w.sections();
    for change in &change_set.changes {
        let ids: Vec<&str> = match change {
            publish::Change::Create { id, .. }
            | publish::Change::Update { id, .. }
            | publish::Change::Delete { id } => vec![id],
            publish::Change::Move { from, to, .. } => vec![from, to],
        };
        for id in ids {
            let valid = section_of(id).is_some_and(|section| {
                rules
                    .get(section)
                    .is_some_and(|config| config.kind == wiki::SectionKind::Rules)
            });
            if !valid {
                bail!("rule proposal directly changes non-rules page '{id}'");
            }
        }
    }
    Ok(())
}

fn rule_proposal_path(w: &Wiki, id: &str) -> Result<PathBuf> {
    wiki::validate_id(id)?;
    if id.contains('/') {
        bail!("rule proposal id must be one page-id segment");
    }
    w.contained_path(&Path::new("proposals/rules").join(format!("{id}.toml")))
}

const RULE_REVIEW_RECEIPT_SCHEMA: &str = "wookie.rules-review-receipt/v1";
const MAX_RULE_REVIEW_RECEIPT_BYTES: u64 = 16 * 1024;
const MAX_RULE_PROPOSAL_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleReviewReceipt {
    schema: String,
    proposal: String,
    manifest_sha256: String,
    catalog_sha256: String,
    config_sha256: String,
    effective_policy_sha256: String,
    plan_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    observed_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    base_revision: Option<String>,
}

fn rule_review_receipt_path(w: &Wiki, id: &str) -> Result<PathBuf> {
    wiki::validate_id(id)?;
    if id.contains('/') {
        bail!("rule proposal id must be one page-id segment");
    }
    w.contained_path(&Path::new("proposals/rules").join(format!("{id}.review.json")))
}

fn sha256(raw: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(raw))
}

fn valid_sha256(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn build_rule_review_receipt(
    id: &str,
    raw: &str,
    checked: &publish::Preflight,
) -> Result<RuleReviewReceipt> {
    Ok(RuleReviewReceipt {
        schema: RULE_REVIEW_RECEIPT_SCHEMA.to_string(),
        proposal: id.to_string(),
        manifest_sha256: sha256(raw.as_bytes()),
        catalog_sha256: checked.plan.observed_content_hash.clone(),
        config_sha256: checked.config_sha256()?,
        effective_policy_sha256: checked.effective_policy_sha256().to_string(),
        plan_sha256: checked.plan_sha256()?,
        observed_revision: checked.plan.observed_revision.clone(),
        base_revision: checked.plan.base_revision.clone(),
    })
}

fn validate_rule_review_receipt(receipt: &RuleReviewReceipt, id: &str) -> Result<()> {
    if receipt.schema != RULE_REVIEW_RECEIPT_SCHEMA {
        bail!(
            "unsupported rule review receipt schema '{}': review the proposal again",
            receipt.schema
        );
    }
    if receipt.proposal != id {
        bail!("rule review receipt belongs to a different proposal; review this proposal again");
    }
    for (label, digest) in [
        ("manifest", &receipt.manifest_sha256),
        ("catalog", &receipt.catalog_sha256),
        ("config", &receipt.config_sha256),
        ("effective policy", &receipt.effective_policy_sha256),
        ("plan", &receipt.plan_sha256),
    ] {
        if !valid_sha256(digest) {
            bail!("rule review receipt has an invalid {label} SHA-256; review the proposal again");
        }
    }
    for revision in [
        receipt.observed_revision.as_deref(),
        receipt.base_revision.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if revision.is_empty() || revision.len() > 512 || revision.chars().any(char::is_control) {
            bail!("rule review receipt has an invalid revision; review the proposal again");
        }
    }
    Ok(())
}

fn read_rule_review_receipt(w: &Wiki, id: &str) -> Result<RuleReviewReceipt> {
    let path = rule_review_receipt_path(w, id)?;
    let raw = read_bounded_regular_file(
        &path,
        MAX_RULE_REVIEW_RECEIPT_BYTES,
        "rule review receipt",
    )
    .with_context(|| {
        format!("no usable review receipt for rule proposal '{id}'; run `wookie rules review {id}` first")
    })?;
    let receipt: RuleReviewReceipt = serde_json::from_slice(&raw).with_context(|| {
        format!("invalid rule review receipt for '{id}'; review the proposal again")
    })?;
    validate_rule_review_receipt(&receipt, id)?;
    Ok(receipt)
}

fn rule_proposal_digest(raw: &str) -> String {
    format!("{:x}", Sha256::digest(raw.as_bytes()))
}

fn read_rule_proposal(w: &Wiki, id: &str) -> Result<String> {
    let path = rule_proposal_path(w, id)?;
    let bytes = read_bounded_regular_file(&path, MAX_RULE_PROPOSAL_BYTES, "rule proposal")
        .with_context(|| format!("reading rule proposal '{id}'"))?;
    let raw = String::from_utf8(bytes)
        .with_context(|| format!("rule proposal '{id}' is not valid UTF-8"))?;
    let expected_suffix = format!("-{}", rule_proposal_digest(&raw));
    if !id.ends_with(&expected_suffix) {
        bail!(
            "rule proposal '{id}' no longer matches its content hash; discard it and propose the exact manifest again"
        );
    }
    Ok(raw)
}

fn read_bounded_regular_file(path: &Path, max_bytes: u64, label: &str) -> Result<Vec<u8>> {
    use std::io::Read;

    let before = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspecting {label} at {}", path.display()))?;
    if before.file_type().is_symlink() || !before.is_file() {
        bail!("{label} must be a regular file");
    }
    if before.len() > max_bytes {
        bail!("{label} exceeds the {max_bytes}-byte limit");
    }
    let file = std::fs::File::open(path)
        .with_context(|| format!("opening {label} at {}", path.display()))?;
    let opened = file.metadata()?;
    if !opened.is_file() || !same_opened_file(&before, &opened) {
        bail!("{label} changed while it was opened");
    }
    let mut bytes = Vec::with_capacity(usize::try_from(before.len()).unwrap_or(0));
    file.take(max_bytes + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        bail!("{label} exceeds the {max_bytes}-byte limit");
    }
    let after = std::fs::symlink_metadata(path)
        .with_context(|| format!("rechecking {label} at {}", path.display()))?;
    if !same_opened_file(&opened, &after) {
        bail!("{label} changed while it was read");
    }
    Ok(bytes)
}

#[cfg(unix)]
fn same_opened_file(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    right.is_file()
        && !right.file_type().is_symlink()
        && left.dev() == right.dev()
        && left.ino() == right.ino()
        && left.len() == right.len()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
}

#[cfg(not(unix))]
fn same_opened_file(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    right.is_file()
        && !right.file_type().is_symlink()
        && left.len() == right.len()
        && left.modified().ok() == right.modified().ok()
        && left.created().ok() == right.created().ok()
}

pub fn rules_propose(
    w: &Wiki,
    raw: &str,
    output: &PublishOutputOptions,
    json: bool,
) -> Result<String> {
    let change_set = publish::ChangeSet::parse(raw)?;
    validate_rule_changes_only(w, &change_set)?;
    let (_, checked, _) = prepare_publish(w, raw, true)?;
    if checked.report.has_errors() {
        let first = checked
            .report
            .diagnostics
            .iter()
            .find(|diagnostic| diagnostic.severity == report::Severity::Error)
            .map(|diagnostic| retrieval::compact_excerpt(&diagnostic.message))
            .unwrap_or_else(|| "preflight failed".to_string());
        bail!(
            "rule proposal has {} deterministic error(s); first error: {first}. Run the manifest through `wookie publish --check` for the bounded report",
            checked.report.summary.errors,
        );
    }
    let digest = rule_proposal_digest(raw);
    let id = format!(
        "rule-{}-{digest}",
        chrono::Utc::now().format("%Y%m%dt%H%M%Sz")
    );
    let rendered = render_publish_preview(
        w,
        &checked,
        output,
        json,
        Some(&format!(
            "Stored rule proposal '{id}'. Review with `wookie rules review {id}`; apply only after explicit approval."
        )),
        Some(("proposal", serde_json::json!(id))),
    )?;
    let path = rule_proposal_path(w, &id)?;
    let guard = w.acquire_mutation_guard()?;
    w.ensure_gitignore_guarded(&guard)?;
    wiki::create_contained_dir_all(&w.dir, Path::new("proposals/rules"))?;
    wiki::atomic_write(&path, raw)?;
    Ok(rendered)
}

pub fn rules_review(
    w: &Wiki,
    id: &str,
    output: &PublishOutputOptions,
    json: bool,
) -> Result<String> {
    let raw = read_rule_proposal(w, id)?;
    let guard = w.acquire_mutation_guard()?;
    w.ensure_gitignore_guarded(&guard)?;
    let (change_set, checked, revision) = prepare_publish(w, &raw, true)?;
    validate_rule_changes_only(w, &change_set)?;
    if !checked.is_publishable() {
        bail!(
            "rule review found {} error(s); fix the manifest and propose it again",
            checked.report.summary.errors
        );
    }
    let receipt = build_rule_review_receipt(id, &raw, &checked)?;
    let rendered = render_publish_preview(
        w,
        &checked,
        output,
        json,
        Some(&format!(
            "Stored review receipt for '{id}'. It binds the complete manifest, catalog, configuration, revision, and plan; bounded output may omit details, so use `wookie rules review {id} --full-diff` before approval when omissions are reported."
        )),
        Some(("review_receipt", serde_json::json!(receipt))),
    )?;
    // Verify after rendering, immediately before the receipt becomes durable,
    // so an editor that bypasses Wookie cannot silently stale the review.
    checked.verify_current_state(w, revision.as_deref())?;
    let path = rule_review_receipt_path(w, id)?;
    wiki::create_contained_dir_all(&w.dir, Path::new("proposals/rules"))?;
    wiki::atomic_write(&path, serde_json::to_vec_pretty(&receipt)?)?;
    Ok(rendered)
}

pub fn rules_apply(w: &Wiki, id: &str, user_approved: bool, json: bool) -> Result<String> {
    if !user_approved {
        bail!("rules apply requires --user-approved after explicit user permission");
    }
    let raw = read_rule_proposal(w, id)?;
    let receipt = read_rule_review_receipt(w, id)?;
    // Reprepare exactly once. This exact preflight is both receipt-checked and
    // moved into the transaction; there is no unchecked second computation.
    let (change_set, checked, revision) = prepare_publish(w, &raw, true)?;
    validate_rule_changes_only(w, &change_set)?;
    if !checked.is_publishable() {
        bail!("rule proposal is no longer publishable; run `wookie rules review {id}` again");
    }
    let expected = build_rule_review_receipt(id, &raw, &checked)?;
    if receipt != expected {
        bail!(
            "rule review receipt is stale: the manifest, catalog, revision, or deterministic plan changed; run `wookie rules review {id}` again"
        );
    }
    apply_prepared_publish(w, change_set, checked, revision, true, json)
}

/// Assemble the critique briefing: target files + every rules section's
/// checks page and rule pages + the output contract. The agent executes it;
/// wookie only gathers.
fn resolve_git_commit(root: &Path, value: &str, label: &str) -> Result<String> {
    if value.is_empty()
        || value.starts_with('-')
        || value.chars().any(char::is_control)
        || value.len() > 512
    {
        bail!("{label} contains an unsafe or invalid Git revision");
    }
    let output = crate::git_paths::bounded_git_stdout(
        root,
        &["rev-parse", "--verify", &format!("{value}^{{commit}}")],
        &format!("Git critique {label} resolution"),
        MAX_CRITIQUE_GIT_TEXT_BYTES,
    )
    .with_context(|| format!("cannot resolve {label} '{value}' in {}", root.display()))?;
    let resolved = String::from_utf8(output)
        .with_context(|| format!("resolved {label} is not valid UTF-8"))?
        .trim()
        .to_string();
    if resolved.is_empty() || resolved.len() > 512 || resolved.chars().any(char::is_control) {
        bail!("resolved {label} is not a safe Git commit id");
    }
    Ok(resolved)
}

const MAX_CRITIQUE_PATHS: usize = 1_024;
const MAX_CRITIQUE_PATH_BYTES: usize = 4_096;
const MAX_CRITIQUE_PATH_BYTES_TOTAL: usize = 1024 * 1024;
const MAX_CRITIQUE_GIT_TEXT_BYTES: usize = 64 * 1024;
const MAX_CRITIQUE_GIT_PATHS: usize = 200_000;
const MAX_CRITIQUE_GIT_PATH_BYTES: usize = 32 * 1024 * 1024;
const MAX_COMPACT_CRITIQUE_FILES: usize = 50;
const MAX_COMPACT_CRITIQUE_RULE_SECTIONS: usize = 20;
const MAX_COMPACT_CRITIQUE_CHECKS: usize = 20;
const MAX_COMPACT_CRITIQUE_RULES: usize = 50;

fn validate_critique_paths(paths: &[String]) -> Result<Vec<String>> {
    if paths.len() > MAX_CRITIQUE_PATHS {
        bail!(
            "critique accepts at most {MAX_CRITIQUE_PATHS} explicit paths (got {})",
            paths.len()
        );
    }
    let mut total = 0usize;
    for (index, path) in paths.iter().enumerate() {
        if path.is_empty() {
            bail!("critique path {} must not be empty", index + 1);
        }
        if path.len() > MAX_CRITIQUE_PATH_BYTES {
            bail!(
                "critique path {} exceeds {MAX_CRITIQUE_PATH_BYTES} bytes",
                index + 1
            );
        }
        if path.chars().any(char::is_control) {
            bail!("critique path {} contains a control character", index + 1);
        }
        if Path::new(path).is_absolute() {
            bail!("critique path {} must be project-relative", index + 1);
        }
        if path.contains('\\') {
            bail!(
                "critique path {} contains a backslash; use normalized forward-slash components",
                index + 1
            );
        }
        if path
            .split('/')
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
        {
            bail!(
                "critique path {} contains an empty, '.' or '..' component",
                index + 1
            );
        }
        total = total
            .checked_add(path.len())
            .ok_or_else(|| anyhow::anyhow!("critique path input is too large"))?;
        if total > MAX_CRITIQUE_PATH_BYTES_TOTAL {
            bail!("critique paths exceed the {MAX_CRITIQUE_PATH_BYTES_TOTAL}-byte total limit");
        }
    }
    Ok(paths.to_vec())
}

fn bounded_critique_git_paths<S: AsRef<std::ffi::OsStr>>(
    root: &Path,
    args: &[S],
    label: &str,
) -> Result<Vec<u8>> {
    let mut literal_args = Vec::with_capacity(args.len() + 1);
    literal_args.push(std::ffi::OsString::from("--literal-pathspecs"));
    literal_args.extend(args.iter().map(|arg| arg.as_ref().to_os_string()));
    crate::git_paths::bounded_git_stdout(root, &literal_args, label, MAX_CRITIQUE_GIT_PATH_BYTES)
}

fn validate_critique_git_paths(paths: Vec<String>, label: &str) -> Result<Vec<String>> {
    crate::git_paths::validate_path_inventory(
        paths,
        label,
        MAX_CRITIQUE_GIT_PATHS,
        MAX_CRITIQUE_GIT_PATH_BYTES,
    )
}

fn git_argv(root: &Path, args: Vec<String>) -> Vec<String> {
    let mut argv = vec![
        "git".to_string(),
        "-C".to_string(),
        root.to_string_lossy().into_owned(),
        "--literal-pathspecs".to_string(),
    ];
    argv.extend(args);
    argv
}

pub struct CritiqueOptions<'a> {
    pub project_root: Option<&'a Path>,
    pub revision: Option<&'a str>,
    pub section: Option<&'a str>,
    pub since: Option<&'a str>,
    pub staged: bool,
    pub paths: &'a [String],
    pub tokens: Option<usize>,
    pub all: bool,
    pub json: bool,
}

fn critique_all_argv(
    w: &Wiki,
    root: &Path,
    options: &CritiqueOptions<'_>,
    target_revision: Option<&str>,
    base_revision: Option<&str>,
) -> Vec<String> {
    let mut argv = vec![
        "wookie".to_string(),
        "--wiki".to_string(),
        w.slug.clone(),
        "critique".to_string(),
        "--project-root".to_string(),
        root.to_string_lossy().into_owned(),
    ];
    if let Some(section) = options.section {
        argv.extend(["--section".to_string(), section.to_string()]);
    }
    if options.since.is_some() {
        if let Some(base) = base_revision {
            argv.extend(["--since".to_string(), base.to_string()]);
        }
    }
    if options.staged {
        argv.push("--staged".to_string());
    }
    if options.revision.is_some() || options.since.is_some() {
        if let Some(target) = target_revision {
            argv.extend(["--revision".to_string(), target.to_string()]);
        }
    }
    argv.push("--all".to_string());
    if !options.paths.is_empty() {
        argv.push("--paths".to_string());
        argv.extend(options.paths.iter().cloned());
    }
    argv
}

fn critique_budget(w: &Wiki, options: &CritiqueOptions<'_>) -> Result<Option<usize>> {
    if options.all {
        if options.tokens.is_some() {
            bail!("critique --all and --tokens are mutually exclusive");
        }
        return Ok(None);
    }
    let budget = options.tokens.unwrap_or(w.audit.critique_tokens);
    if !(crate::config::MIN_CRITIQUE_TOKENS..=crate::config::MAX_CRITIQUE_TOKENS).contains(&budget)
    {
        bail!(
            "critique token budget must be between {} and {}",
            crate::config::MIN_CRITIQUE_TOKENS,
            crate::config::MAX_CRITIQUE_TOKENS,
        );
    }
    Ok(Some(budget))
}

fn enforce_critique_budget(rendered: String, budget: Option<usize>) -> Result<String> {
    let Some(budget) = budget else {
        return Ok(rendered);
    };
    let estimated = retrieval::estimate_tokens(&rendered);
    if estimated > budget {
        bail!(
            "compact critique briefing requires about {estimated} tokens, exceeding the {budget}-token budget; raise --tokens to at least {estimated} or use --all for explicit exhaustive output"
        );
    }
    Ok(rendered)
}

pub fn critique(w: &Wiki, cwd: &Path, options: &CritiqueOptions<'_>) -> Result<String> {
    let budget = critique_budget(w, options)?;
    let root = match options.project_root {
        Some(root) => root
            .canonicalize()
            .with_context(|| format!("resolving project root {}", root.display()))?,
        None => ingest_root(w, cwd)?,
    };

    if options.revision.is_some() && options.staged {
        bail!("--revision cannot be combined with --staged");
    }
    if options.since.is_some() && options.staged {
        bail!("--since cannot be combined with --staged");
    }
    if !options.paths.is_empty() && options.staged {
        bail!("explicit --paths cannot be combined with --staged");
    }
    if !options.paths.is_empty() && options.since.is_some() && options.revision.is_none() {
        bail!("explicit --paths with --since requires an exact --revision target");
    }
    let critique_paths = validate_critique_paths(options.paths)?;
    // Target: what is being critiqued, and how the agent views it.
    let (target_desc, files, diff_argv, inspection_note, target_revision, base_revision) =
        if let Some(revision) = options.revision {
            let revision = resolve_git_commit(&root, revision, "revision")?;
            let (mut args, mut view_args, target_desc, base_revision) =
                if let Some(base) = options.since {
                    let base = resolve_git_commit(&root, base, "--since")?;
                    (
                        vec![
                            "diff".to_string(),
                            "--name-status".to_string(),
                            "-z".to_string(),
                            "--find-renames".to_string(),
                            "--find-copies".to_string(),
                            base.clone(),
                            revision.clone(),
                            "--".to_string(),
                        ],
                        vec![
                            "diff".to_string(),
                            base.clone(),
                            revision.clone(),
                            "--".to_string(),
                        ],
                        format!("revision {revision} since {base}"),
                        Some(base),
                    )
                } else {
                    (
                        vec![
                            "diff-tree".to_string(),
                            "--root".to_string(),
                            "--no-commit-id".to_string(),
                            "--name-status".to_string(),
                            "-z".to_string(),
                            "--find-renames".to_string(),
                            "--find-copies".to_string(),
                            "-r".to_string(),
                            revision.clone(),
                            "--".to_string(),
                        ],
                        vec!["show".to_string(), revision.clone(), "--".to_string()],
                        format!("exact revision {revision}"),
                        None,
                    )
                };
            args.extend(critique_paths.iter().cloned());
            view_args.extend(critique_paths.iter().cloned());
            let output = bounded_critique_git_paths(
                &root,
                &args,
                "Git critique revision changed-path inventory",
            )
            .with_context(|| {
                format!(
                    "cannot inspect revision '{revision}'{} in {}",
                    options
                        .since
                        .map(|base| format!(" since '{base}'"))
                        .unwrap_or_default(),
                    root.display()
                )
            })?;
            let files = validate_critique_git_paths(
                crate::git_paths::parse_name_status(&output, "git critique name-status output")?,
                "Git critique revision changed-path inventory",
            )?;
            (
                target_desc,
                files,
                Some(git_argv(&root, view_args)),
                None,
                Some(revision),
                base_revision,
            )
        } else if !critique_paths.is_empty() {
            (
                format!("{} explicitly given path(s)", critique_paths.len()),
                critique_paths,
                None,
                Some("Read the listed files directly from the resolved project root.".to_string()),
                head_commit(&root),
                None,
            )
        } else {
            let (range, view_args, label, target_revision, base_revision) =
                match (options.since, options.staged) {
                    (Some(r), _) => {
                        let base = resolve_git_commit(&root, r, "--since")?;
                        let target = resolve_git_commit(&root, "HEAD", "target revision")?;
                        (
                            vec![
                                "diff".to_string(),
                                "--name-status".to_string(),
                                "-z".to_string(),
                                "--find-renames".to_string(),
                                "--find-copies".to_string(),
                                base.clone(),
                                target.clone(),
                                "--".to_string(),
                            ],
                            vec![
                                "diff".to_string(),
                                base.clone(),
                                target.clone(),
                                "--".to_string(),
                            ],
                            format!("revision {target} since {base}"),
                            Some(target),
                            Some(base),
                        )
                    }
                    (None, true) => {
                        let target = head_commit(&root);
                        (
                            vec![
                                "diff".to_string(),
                                "--name-status".to_string(),
                                "-z".to_string(),
                                "--find-renames".to_string(),
                                "--find-copies".to_string(),
                                "--cached".to_string(),
                                "--".to_string(),
                            ],
                            vec!["diff".to_string(), "--cached".to_string(), "--".to_string()],
                            "staged changes".to_string(),
                            target,
                            None,
                        )
                    }
                    (None, false) => {
                        let target = resolve_git_commit(&root, "HEAD", "target revision")?;
                        (
                            vec![
                                "diff".to_string(),
                                "--name-status".to_string(),
                                "-z".to_string(),
                                "--find-renames".to_string(),
                                "--find-copies".to_string(),
                                target.clone(),
                                "--".to_string(),
                            ],
                            vec!["diff".to_string(), target.clone(), "--".to_string()],
                            "uncommitted changes".to_string(),
                            Some(target),
                            None,
                        )
                    }
                };
            let out = bounded_critique_git_paths(
                &root,
                &range,
                "Git critique changed-path inventory",
            )
            .with_context(|| {
                format!(
                    "cannot compute target in {} — pass explicit paths: wookie critique --paths <files>",
                    root.display()
                )
            })?;
            let mut files = validate_critique_git_paths(
                crate::git_paths::parse_name_status(&out, "git critique name-status output")?,
                "Git critique changed-path inventory",
            )?;
            // Untracked files are the most common critique target; a plain
            // `git diff` never shows them.
            if !options.staged {
                let unt = bounded_critique_git_paths(
                    &root,
                    &["ls-files", "-z", "--others", "--exclude-standard"],
                    "Git critique untracked-path inventory",
                )
                .with_context(|| format!("listing untracked files in {}", root.display()))?;
                files.extend(validate_critique_git_paths(
                    crate::git_paths::parse_path_list(&unt, "git critique untracked-file output")?,
                    "Git critique untracked-path inventory",
                )?);
            }
            files.sort();
            files.dedup();
            let files = validate_critique_git_paths(files, "combined Git critique path inventory")?;
            (
                label,
                files,
                Some(git_argv(&root, view_args)),
                Some(
                    "Untracked files are listed separately and must be read directly.".to_string(),
                ),
                target_revision,
                base_revision,
            )
        };
    let rendered_diff_argv = diff_argv.as_ref().map(serde_json::to_string).transpose()?;

    // Rules sections, optionally narrowed to one.
    let sections = w.sections();
    let rules: Vec<(String, wiki::SectionConfig)> = sections
        .into_iter()
        .filter(|(name, cfg)| {
            cfg.kind == wiki::SectionKind::Rules
                && options.section.is_none_or(|section| section == name)
        })
        .collect();
    if rules.is_empty() {
        bail!(
            "no rules sections{} — mark one in wookie.toml with kind = \"rules\"",
            options
                .section
                .map(|s| format!(" matching '{s}'"))
                .unwrap_or_default()
        );
    }
    let (pages, catalog_hash, catalog_revision) = {
        let _guard = w.acquire_mutation_guard()?;
        let captured = snapshot::capture_catalog(w)?;
        let pages = captured
            .pages
            .iter()
            .map(|page| {
                let raw = std::str::from_utf8(&page.raw)
                    .with_context(|| format!("page '{}' is not valid UTF-8", page.id))?;
                Ok(Page::parse(&page.id, raw))
            })
            .collect::<Result<Vec<_>>>()?;
        (pages, captured.content_hash, wiki_revision(w))
    };
    let briefing = RuleBriefingIndex::new(&pages, &rules);

    if options.json {
        let mode = if options.revision.is_some() {
            report::ProjectSnapshotMode::Revision
        } else if options.staged {
            report::ProjectSnapshotMode::Staged
        } else {
            report::ProjectSnapshotMode::WorkingTree
        };
        let mut project = report::ProjectSnapshot::new(root.to_string_lossy(), mode);
        if let Some(revision) = target_revision.clone() {
            project = project.revision(revision);
        }
        let mut snapshot = report::Snapshot::new(&w.slug)
            .wiki_content_hash(catalog_hash)
            .with_project(project);
        if let Some(revision) = catalog_revision {
            snapshot = snapshot.wiki_revision(revision);
        }
        let mut report = report::Report::new("critique", snapshot);
        report.extend(briefing.diagnostics.clone());
        report.insert_data("target", serde_json::json!(target_desc));
        report.insert_data("target_revision", serde_json::json!(target_revision));
        report.insert_data("base_revision", serde_json::json!(base_revision));
        // Keep the legacy key, but its value is now a JSON-encoded argv array,
        // never executable shell text. New consumers should use `diff_argv`.
        report.insert_data("diff_command", serde_json::json!(rendered_diff_argv));
        report.insert_data("diff_argv", serde_json::json!(diff_argv));
        report.insert_data("inspection_note", serde_json::json!(inspection_note));
        report.insert_data("evaluation", serde_json::json!("not_executed"));
        report.insert_data(
            "output_mode",
            serde_json::json!(if options.all { "exhaustive" } else { "compact" }),
        );
        let file_total = files.len();
        let mut file_count = if options.all {
            file_total
        } else {
            file_total.min(MAX_COMPACT_CRITIQUE_FILES)
        };
        let mut section_limit = if options.all {
            usize::MAX
        } else {
            MAX_COMPACT_CRITIQUE_RULE_SECTIONS
        };
        let mut check_limit = if options.all {
            usize::MAX
        } else {
            MAX_COMPACT_CRITIQUE_CHECKS
        };
        let mut rule_limit = if options.all {
            usize::MAX
        } else {
            MAX_COMPACT_CRITIQUE_RULES
        };
        loop {
            let files_omitted = file_total.saturating_sub(file_count);
            let (documents, rule_projection) =
                briefing.project(options.all, section_limit, check_limit, rule_limit);
            let sections_omitted = rule_projection
                .sections_total
                .saturating_sub(rule_projection.sections_returned);
            let checks_omitted = rule_projection
                .checks_total
                .saturating_sub(rule_projection.checks_returned);
            let rules_omitted = rule_projection
                .rules_total
                .saturating_sub(rule_projection.rules_returned);
            let rule_material_omitted =
                sections_omitted > 0 || checks_omitted > 0 || rules_omitted > 0;
            let mut projected = report.clone();
            projected.insert_data("files", serde_json::json!(&files[..file_count]));
            projected.insert_data("rules", serde_json::json!(documents));
            projected.insert_data(
                "file_projection",
                serde_json::json!({
                    "total": file_total,
                    "returned": file_count,
                    "omitted": files_omitted,
                    "exhaustive": options.all,
                    "continuation": (files_omitted > 0).then_some(
                        "Rerun the identical critique selection with --all to include every target file."
                    ),
                }),
            );
            projected.insert_data(
                "rule_projection",
                serde_json::json!({
                    "sections": {
                        "total": rule_projection.sections_total,
                        "returned": rule_projection.sections_returned,
                        "omitted": sections_omitted,
                    },
                    "checks": {
                        "total": rule_projection.checks_total,
                        "returned": rule_projection.checks_returned,
                        "omitted": checks_omitted,
                    },
                    "rules": {
                        "total": rule_projection.rules_total,
                        "returned": rule_projection.rules_returned,
                        "omitted": rules_omitted,
                    },
                    "exhaustive": options.all,
                    "continuation": rule_material_omitted.then_some(
                        "Rerun the identical critique selection with --all to include every rules section, checks entry, and rule entry."
                    ),
                }),
            );
            projected.insert_data(
                "omissions",
                serde_json::json!({
                    "files": files_omitted,
                    "rule_sections": sections_omitted,
                    "checks_entries": checks_omitted,
                    "rule_entries": rules_omitted,
                    "checks_bodies": if options.all { 0 } else { briefing.counts.checks_total },
                    "rule_bodies": if options.all { 0 } else { briefing.counts.rules_total },
                    "continuation": "Run each rules[].checks.read_command and rules[].rules[].read_command, or rerun the identical critique selection with --all for every target file and the explicit exhaustive briefing."
                }),
            );
            let rendered = serde_json::to_string(&projected)?;
            let fits = budget.is_none_or(|budget| retrieval::estimate_tokens(&rendered) <= budget);
            if fits {
                return Ok(rendered);
            }
            if file_count > 0 {
                file_count -= 1;
                continue;
            }
            if rule_projection.rules_returned > 0 {
                rule_limit = rule_projection.rules_returned - 1;
                continue;
            }
            if rule_projection.checks_returned > 0 {
                check_limit = rule_projection.checks_returned - 1;
                continue;
            }
            if rule_projection.sections_returned > 0 {
                section_limit = rule_projection.sections_returned - 1;
                continue;
            }
            return enforce_critique_budget(rendered, budget);
        }
    }

    if files.is_empty() {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "Critique briefing — wiki '{}', target: {target_desc}",
            w.slug
        );
        let _ = writeln!(out, "No target files — nothing to critique.");
        return enforce_critique_budget(out.trim_end().to_string(), budget);
    }
    let exhaustive_argv = serde_json::to_string(&critique_all_argv(
        w,
        &root,
        options,
        target_revision.as_deref(),
        base_revision.as_deref(),
    ))?;
    let file_total = files.len();
    let mut file_count = if options.all {
        file_total
    } else {
        file_total.min(MAX_COMPACT_CRITIQUE_FILES)
    };
    let mut section_limit = if options.all {
        usize::MAX
    } else {
        MAX_COMPACT_CRITIQUE_RULE_SECTIONS
    };
    let mut check_limit = if options.all {
        usize::MAX
    } else {
        MAX_COMPACT_CRITIQUE_CHECKS
    };
    let mut rule_limit = if options.all {
        usize::MAX
    } else {
        MAX_COMPACT_CRITIQUE_RULES
    };
    loop {
        let (selected, projection) = briefing.select(section_limit, check_limit, rule_limit);
        let files_omitted = file_total.saturating_sub(file_count);
        let sections_omitted = projection
            .sections_total
            .saturating_sub(projection.sections_returned);
        let checks_omitted = projection
            .checks_total
            .saturating_sub(projection.checks_returned);
        let rules_omitted = projection
            .rules_total
            .saturating_sub(projection.rules_returned);
        let mut out = String::new();
        let _ = writeln!(
            out,
            "Critique briefing — wiki '{}', target: {target_desc}",
            w.slug
        );
        let _ = writeln!(
            out,
            "Files: {} returned / {file_total} total / {files_omitted} omitted",
            file_count
        );
        for file in &files[..file_count] {
            let _ = writeln!(out, "- {}", report::terminal_safe(file));
        }
        if let Some(argv) = &rendered_diff_argv {
            let _ = writeln!(
                out,
                "Git argv (invoke directly; do not evaluate with a shell): {argv}"
            );
        }
        if let Some(note) = &inspection_note {
            let _ = writeln!(out, "{note}");
        }
        if !briefing.diagnostics.is_empty() {
            let _ = writeln!(
                out,
                "Missing checks: {} rules section(s); returned sections identify each visible missing checks page.",
                briefing.diagnostics.len()
            );
        }

        for section in selected {
            let _ = writeln!(
                out,
                "\n== Rules: {}/ — {} ==",
                section.name,
                report::terminal_safe(section.description)
            );
            let checks_id = format!("{}/checks", section.name);
            match (section.checks, section.checks_available, options.all) {
                (Some(checks), _, true) => {
                    let _ = writeln!(
                        out,
                        "\n--- How to verify ({checks_id}) ---\n{}",
                        report::terminal_safe(checks.body.trim_end())
                    );
                }
                (Some(checks), _, false) => {
                    let _ = writeln!(
                        out,
                        "\n--- How to verify ({checks_id}) ---\n{}\nRead full: `wookie read {checks_id}`",
                        report::terminal_safe(&checks.summary())
                    );
                }
                (None, false, _) => {
                    let _ = writeln!(
                        out,
                        "\nERROR: {checks_id} is missing. This rules section is not executable; add its required checks workflow before treating critique as a pass."
                    );
                }
                (None, true, _) => {
                    let _ = writeln!(
                        out,
                        "\n(checks entry omitted by the compact projection; use the continuation below)"
                    );
                }
            }

            for page in section.rules {
                if options.all {
                    let _ = writeln!(
                        out,
                        "\n--- Rule ({}) ---\n{}",
                        page.id,
                        report::terminal_safe(page.body.trim_end())
                    );
                } else {
                    let _ = writeln!(
                        out,
                        "\n--- Rule ({}) ---\n{}\nRead full: `wookie read {}`",
                        page.id,
                        report::terminal_safe(&page.summary()),
                        page.id
                    );
                }
            }
            if section.rules_total == 0 {
                let _ = writeln!(out, "\n(no rule pages in this section yet)");
            }
        }

        let _ = writeln!(
            out,
            "\nProjection: sections {}/{} ({} omitted); checks {}/{} ({} omitted); rules {}/{} ({} omitted).",
            projection.sections_returned,
            projection.sections_total,
            sections_omitted,
            projection.checks_returned,
            projection.checks_total,
            checks_omitted,
            projection.rules_returned,
            projection.rules_total,
            rules_omitted,
        );
        let _ = write!(
            out,
             "\n== Output contract ==\n\
             Now EXECUTE this critique against the target:\n\
             1. Review the changes using the Git argv above when present (invoke it directly, without a shell), and read untracked or explicit files as needed.\n\
             2. Check every rule. Report each violation as: severity (error|warn) | rule page id | file:line | what is wrong | suggested fix.\n\
             3. End with a verdict per rules section: pass or fail.\n\
             4. If a rule was unclear or seems outdated, say so — but do NOT edit rules sections; they are locked and changing them needs explicit user permission."
        );
        if !options.all {
            let _ = write!(
                out,
                "\n\nRule and checks bodies were omitted whole from this compact briefing. Read each returned page with its exact `wookie read <id>` command above. Exhaustive continuation argv (invoke directly; do not evaluate with a shell): {exhaustive_argv}"
            );
        }

        let rendered = out.trim_end().to_string();
        let fits = budget.is_none_or(|budget| retrieval::estimate_tokens(&rendered) <= budget);
        if fits {
            return Ok(rendered);
        }
        if file_count > 0 {
            file_count -= 1;
            continue;
        }
        if projection.rules_returned > 0 {
            rule_limit = projection.rules_returned - 1;
            continue;
        }
        if projection.checks_returned > 0 {
            check_limit = projection.checks_returned - 1;
            continue;
        }
        if projection.sections_returned > 0 {
            section_limit = projection.sections_returned - 1;
            continue;
        }
        return enforce_critique_budget(rendered, budget);
    }
}

/// List or edit the wiki's project roots (the resolution source of truth).
pub fn roots(
    w: &mut Wiki,
    add: Option<PathBuf>,
    remove: Option<PathBuf>,
    json: bool,
) -> Result<String> {
    let add = add.map(|path| {
        path.canonicalize()
            .unwrap_or(path)
            .to_string_lossy()
            .to_string()
    });
    let remove = remove.map(|path| {
        path.canonicalize()
            .unwrap_or(path)
            .to_string_lossy()
            .to_string()
    });
    if add.is_some() || remove.is_some() {
        let slug = w.slug.clone();
        let home = w
            .dir
            .parent()
            .context("wiki directory has no Wookie home")?
            .to_path_buf();
        GlobalConfig::with_home_lock(&home, |_home_guard| {
            if let Some(path) = add.as_ref() {
                for other in wiki::all_wikis(&home) {
                    if other == slug {
                        continue;
                    }
                    if let Ok(other_wiki) = wiki::open(&home, &other) {
                        if other_wiki.config.project_roots.contains(path) {
                            bail!("{path} is already registered to wiki '{other}'");
                        }
                    }
                }
            }

            let edit_slug = slug.clone();
            w.update_config("wookie: update project roots", move |config| {
                if let Some(path) = add {
                    if !config.project_roots.contains(&path) {
                        config.project_roots.push(path);
                    }
                }
                if let Some(path) = remove {
                    let before = config.project_roots.len();
                    config.project_roots.retain(|root| root != &path);
                    if config.project_roots.len() == before {
                        bail!(
                            "{path} is not a project root of '{edit_slug}' (current: {})",
                            config.project_roots.join(", ")
                        );
                    }
                }
                Ok(())
            })
        })?;
    }
    if json {
        return Ok(
            serde_json::json!({"wiki": w.slug, "project_roots": w.config.project_roots})
                .to_string(),
        );
    }
    Ok(format!(
        "Project roots of '{}':\n{}",
        w.slug,
        w.config
            .project_roots
            .iter()
            .map(|r| format!("- {r}"))
            .collect::<Vec<_>>()
            .join("\n")
    ))
}

pub fn remove_wiki(home: &Path, slug: &str, force: bool, json: bool) -> Result<String> {
    let (dir, pages) = GlobalConfig::with_home_lock(home, |_home_guard| {
        let w = wiki::open(home, slug)?;
        let pages = w.page_ids().len();
        if !force {
            bail!(
                "this permanently deletes wiki '{slug}' and its {pages} page(s) at {} — rerun with --force to confirm",
                w.dir.display()
            );
        }

        let original = w.dir.clone();
        let managed_home = original
            .parent()
            .context("wiki directory has no Wookie home")?;
        let suffix = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default();
        let tombstone =
            managed_home.join(format!("removed-{slug}-{}-{suffix}", std::process::id()));
        let mut guard = w.acquire_mutation_guard()?;
        std::fs::rename(&original, &tombstone).with_context(|| {
            format!(
                "moving wiki '{}' out of the registered namespace before removal",
                original.display()
            )
        })?;
        if let Err(error) = guard.relocate_after_rename(&original, &tombstone) {
            let _ = std::fs::rename(&tombstone, &original);
            return Err(error).context("tracking removal lock after directory move");
        }
        drop(guard);
        std::fs::remove_dir_all(&tombstone).with_context(|| {
            format!(
                "removing wiki tombstone {} (the wiki is no longer registered)",
                tombstone.display()
            )
        })?;
        Ok((original, pages))
    })?;
    if json {
        return Ok(serde_json::json!({"removed": slug}).to_string());
    }
    Ok(format!(
        "Removed wiki '{slug}' ({}; {pages} pages).",
        dir.display()
    ))
}

pub fn rename_wiki(home: &Path, old: &str, new: &str, json: bool) -> Result<String> {
    let new = slugify(new);
    if new.is_empty() {
        bail!("new slug is empty after slugification");
    }
    GlobalConfig::with_home_lock(home, |_home_guard| {
        let mut w = wiki::open(home, old)?;
        if wiki::all_wikis(home).contains(&new) {
            bail!("wiki '{new}' already exists");
        }
        let old_dir = w.dir.clone();
        let new_dir = old_dir
            .parent()
            .context("wiki directory has no Wookie home")?
            .join(&new);
        let mut guard = w.acquire_mutation_guard()?;
        w.reload_config_guarded(&guard)?;
        std::fs::rename(&old_dir, &new_dir).with_context(|| {
            format!(
                "renaming wiki directory {} to {}",
                old_dir.display(),
                new_dir.display()
            )
        })?;
        if let Err(error) = guard.relocate_after_rename(&old_dir, &new_dir) {
            let _ = std::fs::rename(&new_dir, &old_dir);
            return Err(error).context("tracking mutation lock after wiki rename");
        }

        w.dir = new_dir.clone();
        w.slug = new.clone();
        w.config.name = new.clone();
        if let Err(error) = w.save_config_guarded(&guard) {
            let rollback = std::fs::rename(&new_dir, &old_dir);
            if rollback.is_ok() {
                let _ = guard.relocate_after_rename(&new_dir, &old_dir);
            }
            return Err(error)
                .context("writing renamed wiki config (directory rollback attempted)");
        }
        w.commit_paths(
            &format!("wookie: rename {old} -> {new}"),
            &["wookie.toml".into()],
        )
    })?;
    if json {
        return Ok(serde_json::json!({"from": old, "to": new}).to_string());
    }
    Ok(format!("Renamed wiki '{old}' -> '{new}'."))
}

fn obsidian_app_config() -> Result<PathBuf> {
    let home = crate::config::user_home()?;
    #[cfg(target_os = "macos")]
    return Ok(home.join("Library/Application Support/obsidian/obsidian.json"));
    #[cfg(target_os = "linux")]
    return Ok(home.join(".config/obsidian/obsidian.json"));
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    return Ok(std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or(home)
        .join("obsidian/obsidian.json"));
}

fn fnv1a_hex(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// Obsidian only opens vaults listed in its own obsidian.json, so an
/// unregistered folder gives "vault not found". Register ours there.
/// Returns true if it was newly registered.
fn register_obsidian_vault(vault: &Path) -> Result<bool> {
    let cfg_path = obsidian_app_config()?;
    let raw = crate::config::read_optional_bounded_regular_utf8(
        &cfg_path,
        16 * 1024 * 1024,
        "Obsidian application configuration",
    )?
    .unwrap_or_else(|| "{}".into());
    let mut cfg: serde_json::Value =
        serde_json::from_str(&raw).unwrap_or_else(|_| serde_json::json!({}));
    if !cfg.is_object() {
        cfg = serde_json::json!({});
    }
    let obj = cfg.as_object_mut().unwrap();
    if !obj.get("vaults").map(|v| v.is_object()).unwrap_or(false) {
        obj.insert("vaults".into(), serde_json::json!({}));
    }
    let target = vault.to_string_lossy().to_string();
    let vaults = obj["vaults"].as_object_mut().unwrap();
    if vaults
        .values()
        .any(|e| e.get("path").and_then(|p| p.as_str()) == Some(target.as_str()))
    {
        return Ok(false);
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    vaults.insert(
        fnv1a_hex(&target),
        serde_json::json!({ "path": target, "ts": ts }),
    );
    if let Some(parent) = cfg_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&cfg_path, serde_json::to_string(&cfg)?)?;
    Ok(true)
}

/// Open the wiki's pages/ folder as an Obsidian vault.
pub fn obsidian(w: &Wiki, print_only: bool, json: bool) -> Result<String> {
    let vault = w.contained_path(Path::new("pages"))?;
    let uri = format!(
        "obsidian://open?path={}",
        percent_encode(&vault.to_string_lossy())
    );
    if print_only {
        return if json {
            Ok(serde_json::json!({"vault": vault, "uri": uri, "opened": false}).to_string())
        } else {
            Ok(uri)
        };
    }

    // Only actual opening prepares and registers the vault; --print remains
    // a side-effect-free URI operation.
    wiki::create_contained_dir_all(&w.dir, Path::new("pages/.obsidian"))?;
    w.ensure_gitignore()?;
    let newly_registered = register_obsidian_vault(&vault).unwrap_or(false);

    #[cfg(target_os = "macos")]
    let status = std::process::Command::new("open").arg(&uri).status();
    #[cfg(target_os = "linux")]
    let status = std::process::Command::new("xdg-open").arg(&uri).status();
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let status = std::process::Command::new("cmd")
        .args(["/C", "start", ""])
        .arg(&uri)
        .status();
    match status {
        Ok(s) if s.success() => {
            if json {
                return Ok(serde_json::json!({
                    "vault": vault,
                    "uri": uri,
                    "opened": true,
                    "registered": newly_registered,
                })
                .to_string());
            }
            let mut out = format!("Opened wiki '{}' in Obsidian: {}", w.slug, vault.display());
            if newly_registered {
                out.push_str(
                    "\nRegistered the vault with Obsidian. If Obsidian was already running and shows 'vault not found', quit and reopen it once.",
                );
            }
            Ok(out)
        }
        _ => bail!("could not launch Obsidian — open this URI manually: {uri}"),
    }
}

pub fn doctor_with_options(
    w: &Wiki,
    fix: bool,
    options: &audit::AuditOptions,
    json: bool,
) -> Result<(String, usize)> {
    let fixed = if fix {
        let (text, _) = doctor_legacy(w, true, false)?;
        text.lines()
            .filter_map(|line| line.strip_prefix("fixed: ").map(str::to_string))
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let mut report = audit::audit(w, options)?;
    if !fixed.is_empty() {
        report.insert_data("fixed", serde_json::json!(fixed));
    }
    let errors = report.summary.errors;
    if json {
        let mut value = serde_json::to_value(&report)?;
        // Additive compatibility for pre-v1 consumers while `schema`,
        // `command`, codes, and severities are the stable CI contract.
        value["issues"] = serde_json::json!(report
            .diagnostics
            .iter()
            .map(|diagnostic| diagnostic.message.clone())
            .collect::<Vec<_>>());
        value["fixed"] = serde_json::json!(fixed);
        Ok((serde_json::to_string(&value)?, errors))
    } else if report.summary.total == 0 && fixed.is_empty() {
        Ok((
            format!(
                "Wiki '{}' is healthy: {} pages, no issues.",
                w.slug,
                report
                    .data
                    .get("page_count")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or_default()
            ),
            0,
        ))
    } else {
        let mut rendered = String::new();
        for diagnostic in &report.diagnostics {
            let message = match diagnostic.code.as_str() {
                "missing_required_page" => format!(
                    "missing required page: '{}'",
                    diagnostic.page.as_deref().unwrap_or("unknown")
                ),
                "page_unfiled" => format!(
                    "unfiled page (not under any section): '{}'",
                    diagnostic.page.as_deref().unwrap_or("unknown")
                ),
                report::code::STALE_PAGE => format!(
                    "code changed since last ingest — stale page '{}'",
                    diagnostic.page.as_deref().unwrap_or("unknown")
                ),
                report::code::ORPHAN_PAGE => format!(
                    "orphan page (no inbound wiki link): '{}'",
                    diagnostic.page.as_deref().unwrap_or("unknown")
                ),
                _ => diagnostic.message.clone(),
            };
            let _ = writeln!(rendered, "issue: {}", report::terminal_safe(&message));
        }
        let _ = write!(
            rendered,
            "\n{} issue(s): {} error(s), {} warning(s).",
            report.summary.total, report.summary.errors, report.summary.warnings
        );
        for item in fixed.iter().rev() {
            rendered.insert_str(0, &format!("fixed: {}\n", report::terminal_safe(item)));
        }
        Ok((rendered.trim_end().to_string(), errors))
    }
}

pub fn status(w: &Wiki, options: &audit::AuditOptions, json: bool) -> Result<(String, usize)> {
    let mut report = audit::audit_for(w, options, "status")?;
    let errors = report.summary.errors;
    if json {
        // `status` is the operator dashboard, not the exhaustive audit dump.
        // Preserve full summary/count semantics while bounding path lists and
        // diagnostic detail. `doctor --json` remains the explicit complete
        // CI report.
        const STATUS_DIAGNOSTICS: usize = 20;
        let total_diagnostics = report.diagnostics.len();
        report
            .diagnostics
            .sort_by_key(|diagnostic| match diagnostic.severity {
                report::Severity::Error => 0,
                report::Severity::Warning => 1,
                report::Severity::Info => 2,
            });
        report.diagnostics.truncate(STATUS_DIAGNOSTICS);
        for diagnostic in &mut report.diagnostics {
            diagnostic.message = retrieval::compact_excerpt(&diagnostic.message);
            diagnostic.suggestion = diagnostic
                .suggestion
                .as_deref()
                .map(retrieval::compact_excerpt);
            diagnostic.data.clear();
        }
        for key in [
            "changed_project_paths",
            "project_dirty_paths",
            "project_staged_paths",
        ] {
            if let Some(value) = report.data.get_mut(key) {
                let count = value.as_array().map_or(0, Vec::len);
                *value = serde_json::json!({"count": count, "items": []});
            }
        }
        report.data.insert(
            "status_projection".into(),
            serde_json::json!({
                "diagnostics_returned": report.diagnostics.len(),
                "diagnostics_omitted": total_diagnostics.saturating_sub(report.diagnostics.len()),
                "full_report_command": "wookie doctor --json"
            }),
        );
        Ok((serde_json::to_string(&report)?, errors))
    } else {
        Ok((audit::render_status(&report), errors))
    }
}

fn doctor_legacy(w: &Wiki, fix: bool, json: bool) -> Result<(String, usize)> {
    // A repair is one logical mutation. Hold the shared writer guard while
    // inspecting, rewriting, and recording its page set so publishers and
    // other commands cannot observe or commit a partially repaired wiki.
    let mutation_guard = if fix {
        Some(w.acquire_mutation_guard()?)
    } else {
        None
    };
    let mut issues: Vec<String> = vec![];
    let mut fixed: Vec<String> = vec![];
    let mut fixed_paths: Vec<String> = vec![];
    let mut pages = vec![];
    let (_, notification_warnings) = sessions::inspect_notifications(w);
    issues.extend(notification_warnings.into_iter().map(|warning| {
        format!(
            "invalid notification storage '{}': {}",
            warning.path, warning.message
        )
    }));
    let session_listing = sessions::list_with_options(w, &sessions::SessionListRequest::default())?;
    issues.extend(session_listing.warnings.into_iter().map(|warning| {
        format!(
            "invalid session storage '{}': {}",
            warning.path, warning.message
        )
    }));
    for id in w.page_ids() {
        match w.load_page(&id) {
            Ok(page) => pages.push(page),
            Err(error) => issues.push(format!("invalid or unreadable page '{id}': {error:#}")),
        }
    }
    // Check every rules boundary before making the first repair. Without this
    // preflight, an ordinary page could be rewritten before a later locked
    // rules page aborts the command, leaving an unexpected partial repair.
    if fix {
        for page in &pages {
            let needs_repair = if page.fm.created.is_empty() {
                true
            } else {
                let path = w.page_path(&page.id)?;
                let on_disk =
                    String::from_utf8(snapshot::read_raw_page(&path)?).with_context(|| {
                        format!("reading page '{}' at {} as UTF-8", page.id, path.display())
                    })?;
                on_disk != page.render()
            };
            if needs_repair {
                w.assert_writable(&page.id)?;
            }
        }
    }
    let ids: HashSet<String> = pages.iter().map(|p| p.id.clone()).collect();
    let linked: HashSet<String> = pages.iter().flat_map(|p| p.links()).collect();

    for p in &pages {
        for l in p.links() {
            if !ids.contains(&l) {
                issues.push(format!("broken link: [[{l}]] in '{}'", p.id));
            }
        }
        if p.fm.created.is_empty() {
            if fix {
                w.assert_writable(&p.id)?;
                let mut p2 = p.clone();
                w.save_page_raw_guarded(
                    mutation_guard
                        .as_ref()
                        .expect("doctor --fix holds a mutation guard"),
                    &mut p2,
                    true,
                )?;
                fixed.push(format!("normalized frontmatter of '{}'", p.id));
                fixed_paths.push(format!("pages/{}.md", p.id));
            } else {
                issues.push(format!("missing/invalid frontmatter: '{}'", p.id));
            }
        } else if fix {
            let path = w.page_path(&p.id)?;
            let on_disk =
                String::from_utf8(snapshot::read_raw_page(&path)?).with_context(|| {
                    format!("reading page '{}' at {} as UTF-8", p.id, path.display())
                })?;
            if on_disk != p.render() {
                w.assert_writable(&p.id)?;
                let mut p2 = p.clone();
                w.save_page_raw_guarded(
                    mutation_guard
                        .as_ref()
                        .expect("doctor --fix holds a mutation guard"),
                    &mut p2,
                    false,
                )?;
                fixed.push(format!("reserialized '{}' to canonical format", p.id));
                fixed_paths.push(format!("pages/{}.md", p.id));
            }
        }
        if p.fm.description.is_empty() || p.fm.description.starts_with("TODO") {
            issues.push(format!("missing description: '{}'", p.id));
        }
        if p.summary().is_empty() || p.summary().starts_with("TODO") {
            issues.push(format!("missing summary paragraph: '{}'", p.id));
        }
        if p.is_stub() {
            issues.push(format!("stub awaiting content: '{}'", p.id));
        }
        if p.id != "index" && !linked.contains(&p.id) {
            issues.push(format!("orphan (no page links to it): '{}'", p.id));
        }
        match section_of(&p.id) {
            Some(s) if w.sections().contains_key(s) => {}
            _ if p.id == "index" => {}
            _ => issues.push(format!("unfiled page (not under any section): '{}'", p.id)),
        }
    }
    for (section, cfg) in w.sections() {
        for required in &cfg.required {
            let id = format!("{section}/{required}");
            if !ids.contains(&id) {
                issues.push(format!("missing required page: '{id}'"));
            }
        }
        if cfg.kind == wiki::SectionKind::Rules && !ids.contains(&format!("{section}/checks")) {
            issues.push(format!(
                "rules section '{section}' has no checks page ('{section}/checks' tells critique how to verify its rules)"
            ));
        }
    }
    if let (Some(last), Some(root)) = (&w.config.last_ingest_commit, w.config.project_roots.first())
    {
        match changed_since(Path::new(root), last) {
            Ok(changed) if !changed.is_empty() => issues.push(
                "code changed since last ingest — run `wookie ingest` for a stale-page worklist"
                    .into(),
            ),
            Err(error) => issues.push(format!("cannot compare code to last ingest: {error:#}")),
            _ => {}
        }
    }
    if !fixed.is_empty() {
        w.commit_paths("wookie: doctor --fix", &fixed_paths)?;
    }

    if json {
        return Ok((
            serde_json::json!({"issues": issues, "fixed": fixed}).to_string(),
            issues.len(),
        ));
    }
    let mut out = String::new();
    if issues.is_empty() && fixed.is_empty() {
        return Ok((
            format!(
                "Wiki '{}' is healthy: {} pages, no issues.",
                w.slug,
                pages.len()
            ),
            0,
        ));
    }
    for f in &fixed {
        let _ = writeln!(out, "fixed: {f}");
    }
    for i in &issues {
        let _ = writeln!(out, "issue: {i}");
    }
    let _ = write!(
        out,
        "\n{} issue(s). Broken links: `wookie expand`. Stubs/summaries: `wookie write <id>`. Orphans: link them from a related page.",
        issues.len()
    );
    Ok((out.trim_end().to_string(), issues.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_wiki(label: &str) -> (PathBuf, Wiki) {
        let sequence = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "wookie-commands-{label}-{}-{sequence}",
            std::process::id()
        ));
        let home = base.join("home");
        let dir = home.join("test");
        std::fs::create_dir_all(dir.join("pages")).unwrap();
        std::fs::write(
            dir.join("wookie.toml"),
            "name = \"test\"\nproject_roots = []\nauto_commit = false\n",
        )
        .unwrap();
        let wiki = wiki::open(&home, "test").unwrap();
        (base, wiki)
    }

    #[test]
    fn protocol_sections_must_exist_in_the_effective_wiki_config() {
        let (base, mut wiki) = temp_wiki("protocol-sections");

        validate_protocol_section(&wiki, "operations/deploy", Some("guides")).unwrap();
        let missing =
            validate_protocol_section(&wiki, "operations/custom-runbook", Some("runbooks"))
                .unwrap_err()
                .to_string();
        assert!(missing.contains("section 'runbooks', which is not configured"));

        wiki.config.sections.insert(
            "runbooks".into(),
            crate::wiki::SectionConfig {
                description: "Operational runbooks".into(),
                ..Default::default()
            },
        );
        validate_protocol_section(&wiki, "operations/custom-runbook", Some("runbooks")).unwrap();
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn description_derivation_handles_bold_lead_sentences() {
        assert_eq!(
            first_sentence("**Compact lead.** Later detail must stay out."),
            "Compact lead."
        );
        assert_eq!(
            first_sentence("**Question lead?** Later detail."),
            "Question lead?"
        );
        assert_eq!(first_sentence("No punctuation"), "No punctuation.");
    }

    #[test]
    fn protocol_and_section_reads_happen_after_the_shared_guard() {
        let (base, wiki) = temp_wiki("protocol-guard-order");
        let held = wiki.acquire_mutation_guard().unwrap();

        let write_error = protocol_write(
            &wiki,
            "operations/custom-runbook",
            "+++\nsection = \"missing\"\n+++\nBody.",
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(write_error.contains("not reentrant"), "{write_error}");
        assert!(!write_error.contains("not configured"), "{write_error}");

        let new_error = new_page(
            &wiki,
            "page",
            None,
            None,
            Vec::new(),
            Vec::new(),
            None,
            Some("missing"),
            None,
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(new_error.contains("not reentrant"), "{new_error}");
        assert!(!new_error.contains("no protocol"), "{new_error}");

        drop(held);
        std::fs::remove_dir_all(base).unwrap();
    }

    fn git_ok(root: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn init_test_project(root: &Path) -> String {
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub fn live() {}\n").unwrap();
        git_ok(root, &["init", "-q"]);
        git_ok(root, &["config", "user.name", "Wookie Tests"]);
        git_ok(
            root,
            &["config", "user.email", "wookie-tests@example.invalid"],
        );
        git_ok(root, &["add", "src/lib.rs"]);
        git_ok(root, &["commit", "-qm", "initial"]);
        git_ok(root, &["rev-parse", "HEAD"])
    }

    #[test]
    fn concurrent_root_additions_merge_from_latest_config() {
        let (base, first) = temp_wiki("concurrent-roots");
        let home = base.join("home");
        let second = wiki::open(&home, "test").unwrap();
        let first_root = base.join("project-a");
        let second_root = base.join("project-b");
        std::fs::create_dir_all(&first_root).unwrap();
        std::fs::create_dir_all(&second_root).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));

        let first_barrier = std::sync::Arc::clone(&barrier);
        let first_thread = std::thread::spawn(move || {
            let mut wiki = first;
            first_barrier.wait();
            roots(&mut wiki, Some(first_root), None, false)
        });
        let second_barrier = std::sync::Arc::clone(&barrier);
        let second_thread = std::thread::spawn(move || {
            let mut wiki = second;
            second_barrier.wait();
            roots(&mut wiki, Some(second_root), None, false)
        });
        barrier.wait();
        first_thread.join().unwrap().unwrap();
        second_thread.join().unwrap().unwrap();

        let stored = wiki::open(&home, "test").unwrap();
        assert_eq!(stored.config.project_roots.len(), 2);
        assert!(stored
            .config
            .project_roots
            .iter()
            .any(|root| root.ends_with("project-a")));
        assert!(stored
            .config
            .project_roots
            .iter()
            .any(|root| root.ends_with("project-b")));
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn concurrent_init_allows_only_one_wiki_for_a_project_root() {
        let sequence = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "wookie-commands-concurrent-init-{}-{sequence}",
            std::process::id()
        ));
        let home = base.join("home");
        let project = base.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));

        let first_home = home.clone();
        let first_project = project.clone();
        let first_barrier = std::sync::Arc::clone(&barrier);
        let first = std::thread::spawn(move || {
            first_barrier.wait();
            init(
                &first_home,
                &first_project,
                Some("first".into()),
                Some(first_project.clone()),
                None,
                false,
            )
        });
        let second_home = home.clone();
        let second_project = project.clone();
        let second_barrier = std::sync::Arc::clone(&barrier);
        let second = std::thread::spawn(move || {
            second_barrier.wait();
            init(
                &second_home,
                &second_project,
                Some("second".into()),
                Some(second_project.clone()),
                None,
                false,
            )
        });
        barrier.wait();
        let results = [first.join().unwrap(), second.join().unwrap()];
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        let error = results
            .iter()
            .find_map(|result| result.as_ref().err())
            .unwrap()
            .to_string();
        assert!(error.contains("already registered"), "{error}");
        assert_eq!(wiki::all_wikis(&home).len(), 1);
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn init_rejects_unsafe_metadata_before_creating_a_wiki() {
        let sequence = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "wookie-commands-invalid-init-{}-{sequence}",
            std::process::id()
        ));
        let home = base.join("home");
        let project = base.join("project");
        std::fs::create_dir_all(&project).unwrap();

        let error = init(
            &home,
            &project,
            Some("unsafe-metadata".into()),
            Some(project.clone()),
            Some("terminal\u{1b}[31m".into()),
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("control"), "{error}");
        assert!(!home.join("unsafe-metadata").exists());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn project_freshness_distinguishes_current_stale_and_unknown() {
        let (base, mut wiki) = temp_wiki("freshness-states");
        let project = base.join("project");
        let revision = init_test_project(&project);
        wiki.config.project_roots = vec![project.to_string_lossy().to_string()];
        wiki.config.last_ingest_commit = Some(revision);

        let mut page = Page::parse("code/lib", "**Lib.** Tracks the library.");
        page.fm.sources = vec!["src/lib.rs".into()];
        let pages = vec![page];

        let current = project_freshness(&wiki, &pages, Some(&project));
        assert_eq!(current.state, retrieval::FreshnessState::Current);
        assert_eq!(current.changed_count, Some(0));

        std::fs::write(
            project.join("uncovered.rs"),
            "pub const NEW: bool = true;\n",
        )
        .unwrap();
        let stale = project_freshness(&wiki, &pages, Some(&project));
        assert_eq!(stale.state, retrieval::FreshnessState::Stale);
        assert_eq!(stale.changed_count, Some(1));
        assert_eq!(stale.uncovered_count, Some(1));
        assert!(stale.stale_page_ids.is_empty());

        wiki.config.last_ingest_commit = Some("not-a-revision".into());
        let unknown = project_freshness(&wiki, &pages, Some(&project));
        assert_eq!(unknown.state, retrieval::FreshnessState::Unknown);
        assert_eq!(unknown.changed_count, None);
        assert!(unknown
            .error
            .as_deref()
            .is_some_and(|error| error.contains("last ingest revision")));
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn changed_since_preserves_unusual_utf8_paths_and_rename_endpoints() {
        let (base, _wiki) = temp_wiki("nul-git-paths");
        let project = base.join("project");
        init_test_project(&project);

        let old = "old name.rs";
        let renamed = "renamed-é.rs";
        let with_newline = "line\nbreak.rs";
        let untracked = "untracked space.rs";
        std::fs::write(project.join(old), "pub const OLD: bool = true;\n").unwrap();
        std::fs::write(project.join(with_newline), "pub const VERSION: u8 = 1;\n").unwrap();
        git_ok(&project, &["add", "--", old, with_newline]);
        git_ok(&project, &["commit", "-qm", "add unusual paths"]);
        let revision = git_ok(&project, &["rev-parse", "HEAD"]);

        git_ok(&project, &["mv", "--", old, renamed]);
        std::fs::write(project.join(with_newline), "pub const VERSION: u8 = 2;\n").unwrap();
        std::fs::write(project.join(untracked), "pub const NEW: bool = true;\n").unwrap();

        let changed = changed_since(&project, &revision).unwrap();
        for expected in [old, renamed, with_newline, untracked] {
            assert!(changed.iter().any(|path| path == expected), "{changed:?}");
        }
        assert!(changed.windows(2).all(|pair| pair[0] < pair[1]));

        let inventory = list_project_files(&project).unwrap();
        for expected in [renamed, with_newline, untracked] {
            assert!(
                inventory.iter().any(|path| path == expected),
                "{inventory:?}"
            );
        }
        std::fs::remove_dir_all(base).unwrap();
    }

    // APFS rejects invalid UTF-8 names before Git can observe them. The
    // parser-level rejection test remains cross-platform above.
    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn changed_since_rejects_invalid_utf8_git_paths() {
        use std::os::unix::ffi::OsStringExt;

        let (base, _wiki) = temp_wiki("invalid-utf8-git-path");
        let project = base.join("project");
        let revision = init_test_project(&project);
        let invalid = std::ffi::OsString::from_vec(b"invalid-\xff.rs".to_vec());
        std::fs::write(project.join(invalid), "pub const BAD: bool = true;\n").unwrap();

        let error = changed_since(&project, &revision).unwrap_err().to_string();
        assert!(error.contains("not valid UTF-8"), "{error}");
        let error = list_project_files(&project).unwrap_err().to_string();
        assert!(error.contains("not valid UTF-8"), "{error}");
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn project_freshness_maps_file_line_only_provenance() {
        let (base, mut wiki) = temp_wiki("freshness-file-line");
        let project = base.join("project");
        let revision = init_test_project(&project);
        wiki.config.project_roots = vec![project.to_string_lossy().to_string()];
        wiki.config.last_ingest_commit = Some(revision);
        std::fs::write(project.join("src/lib.rs"), "pub fn live() { todo!() }\n").unwrap();

        let page = Page::parse(
            "code/lib",
            "**Lib.** Tracks the library.\n\nFile: `src/lib.rs`",
        );
        assert!(page.fm.sources.is_empty());
        let freshness = project_freshness(&wiki, &[page], Some(&project));
        assert_eq!(freshness.state, retrieval::FreshnessState::Stale);
        assert_eq!(freshness.changed_count, Some(1));
        assert_eq!(freshness.uncovered_count, Some(0));
        assert_eq!(freshness.stale_page_ids, vec!["code/lib"]);
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn project_freshness_selects_the_active_registered_root() {
        let (base, mut wiki) = temp_wiki("freshness-active-root");
        let first = base.join("project-one");
        let second = base.join("project-two");
        init_test_project(&first);
        let revision = init_test_project(&second);
        wiki.config.project_roots = vec![
            first.to_string_lossy().to_string(),
            second.to_string_lossy().to_string(),
        ];
        wiki.config.last_ingest_commit = Some(revision);
        std::fs::write(second.join("src/lib.rs"), "pub fn changed() {}\n").unwrap();

        let mut page = Page::parse("code/lib", "**Lib.** Tracks the library.");
        page.fm.sources = vec!["src/lib.rs".into()];
        let freshness = project_freshness(&wiki, &[page.clone()], Some(&second));
        assert_eq!(freshness.state, retrieval::FreshnessState::Stale);
        assert_eq!(freshness.stale_page_ids, vec!["code/lib"]);

        let ambiguous = project_freshness(&wiki, &[page], Some(&base));
        assert_eq!(ambiguous.state, retrieval::FreshnessState::Unknown);
        assert!(ambiguous
            .error
            .as_deref()
            .is_some_and(|error| error.contains("ambiguous")));
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn project_freshness_uses_the_active_linked_worktree() {
        let (base, mut wiki) = temp_wiki("freshness-linked-worktree");
        let main = base.join("main");
        let linked = base.join("linked");
        let revision = init_test_project(&main);
        let linked_arg = linked.to_string_lossy().to_string();
        git_ok(
            &main,
            &["worktree", "add", "-q", "-b", "freshness-test", &linked_arg],
        );
        wiki.config.project_roots = vec![main.to_string_lossy().to_string()];
        wiki.config.last_ingest_commit = Some(revision);
        std::fs::write(linked.join("src/lib.rs"), "pub fn linked_change() {}\n").unwrap();

        let mut page = Page::parse("code/lib", "**Lib.** Tracks the library.");
        page.fm.sources = vec!["src/lib.rs".into()];
        let freshness = project_freshness(&wiki, &[page], Some(&linked));
        assert_eq!(freshness.state, retrieval::FreshnessState::Stale);
        assert_eq!(freshness.stale_page_ids, vec!["code/lib"]);
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn search_rejects_materialization_limits_before_loading_pages() {
        let (base, wiki) = temp_wiki("search-hard-limits");
        let options = SearchOptions {
            cwd: None,
            query: "cache".into(),
            tag: None,
            limit: Some(MAX_SEARCH_LIMIT + 1),
            tokens: None,
            excerpt_lines: Some(1),
            cursor: 0,
            context_hash: None,
            regex: false,
            all: false,
        };
        let error = search_with_options(&wiki, &options, true)
            .unwrap_err()
            .to_string();
        assert!(error.contains("search limit"), "{error}");

        let options = SearchOptions {
            limit: Some(1),
            excerpt_lines: Some(MAX_EXCERPT_LINES + 1),
            ..options
        };
        let error = search_with_options(&wiki, &options, true)
            .unwrap_err()
            .to_string();
        assert!(error.contains("excerpt-lines"), "{error}");

        let options = SearchOptions {
            tokens: Some(usize::MAX),
            excerpt_lines: Some(1),
            ..options
        };
        let error = search_with_options(&wiki, &options, true)
            .unwrap_err()
            .to_string();
        assert!(error.contains("token budget"), "{error}");
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn prime_rejects_materialization_limits_before_loading_pages() {
        let (base, wiki) = temp_wiki("prime-hard-limits");
        let mut options = PrimeOptions {
            cwd: None,
            query: "limits".into(),
            tokens: Some(usize::MAX),
            instruction_tokens: Some(1),
            limit: Some(1),
            max_per_section: Some(1),
            since: None,
            cursor: 0,
            context_hash: None,
        };
        let error = prime(&wiki, &options, true).unwrap_err().to_string();
        assert!(error.contains("token budgets"), "{error}");

        options.tokens = Some(MAX_RETRIEVAL_TOKENS);
        options.instruction_tokens = Some(MAX_RETRIEVAL_TOKENS);
        options.max_per_section = Some(usize::MAX);
        let error = prime(&wiki, &options, true).unwrap_err().to_string();
        assert!(error.contains("max-per-section"), "{error}");

        options.max_per_section = Some(MAX_SEARCH_LIMIT);
        assert!(prime(&wiki, &options, true).is_ok());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn prime_and_doctor_reject_invalid_standing_pins() {
        let (base, wiki) = temp_wiki("invalid-standing-pins");
        let mut page = Page::parse("standing", "**Standing.** Real text.");
        page.fm.pin = true;
        page.fm.pin_level = Some(PinLevel::Instruction);
        page.fm.status = Some("stub".into());
        let path = wiki.page_path("standing").unwrap();
        std::fs::write(&path, page.render()).unwrap();
        let options = PrimeOptions {
            cwd: None,
            query: "standing".into(),
            tokens: Some(1_500),
            instruction_tokens: Some(500),
            limit: Some(5),
            max_per_section: Some(5),
            since: None,
            cursor: 0,
            context_hash: None,
        };
        let error = prime(&wiki, &options, true).unwrap_err().to_string();
        assert!(error.contains("is a stub"), "{error}");
        let report = audit::audit(&wiki, &audit::AuditOptions::default()).unwrap();
        assert!(report
            .diagnostics
            .iter()
            .any(|item| item.code == report::code::PINNED_STANDING_TEXT_INVALID));

        page.fm.status = None;
        page.body = "TODO: add the actual standing rule.".into();
        std::fs::write(&path, page.render()).unwrap();
        let error = prime(&wiki, &options, true).unwrap_err().to_string();
        assert!(error.contains("placeholder standing text"), "{error}");
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn new_rejects_standing_pin_stubs_but_allows_discoverable_stubs() {
        let (base, wiki) = temp_wiki("new-pin-stubs");
        for level in [PinLevel::Instruction, PinLevel::Summary] {
            let error = new_page(
                &wiki,
                &format!("invalid-{level:?}").to_ascii_lowercase(),
                None,
                None,
                Vec::new(),
                Vec::new(),
                Some(level),
                None,
                None,
                true,
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains("require a non-empty page body"), "{error}");
        }
        assert!(!wiki.exists("invalid-instruction"));
        assert!(!wiki.exists("invalid-summary"));

        new_page(
            &wiki,
            "discoverable-stub",
            None,
            None,
            Vec::new(),
            Vec::new(),
            Some(PinLevel::Discoverable),
            None,
            None,
            true,
        )
        .unwrap();
        let page = wiki.load_page("discoverable-stub").unwrap();
        assert!(page.is_stub());
        assert_eq!(page.pin_level(), Some(PinLevel::Discoverable));
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn prime_reports_structured_unknown_freshness() {
        let (base, mut wiki) = temp_wiki("prime-unknown-freshness");
        let project = base.join("project");
        let revision = init_test_project(&project);
        wiki.config.project_roots = vec![project.to_string_lossy().to_string()];
        wiki.config.last_ingest_commit = Some("missing-revision".into());

        let options = PrimeOptions {
            cwd: Some(project.clone()),
            query: "library".into(),
            tokens: Some(1_500),
            instruction_tokens: Some(200),
            limit: Some(5),
            max_per_section: Some(5),
            since: None,
            cursor: 0,
            context_hash: None,
        };
        let raw = prime(&wiki, &options, true).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["wiki"]["freshness"], "unknown");
        let context_hash = value["context_hash"].as_str().unwrap();
        assert!(context_hash.starts_with("sha256:"), "{context_hash}");
        assert_eq!(context_hash.len(), "sha256:".len() + 64);
        assert_eq!(value["telemetry"]["freshness"]["state"], "unknown");
        assert!(value["telemetry"]["freshness"]["changed_count"].is_null());
        assert!(value["telemetry"]["freshness"]["uncovered_count"].is_null());
        assert!(value["telemetry"]["freshness"]["error"].is_string());

        let human = prime(&wiki, &options, false).unwrap();
        assert!(human.contains("Freshness: unknown"), "{human}");

        wiki.config.last_ingest_commit = Some(revision);
        std::fs::write(
            project.join("uncovered.rs"),
            "pub const NEW: bool = true;\n",
        )
        .unwrap();
        let stale_raw = prime(&wiki, &options, true).unwrap();
        let stale: serde_json::Value = serde_json::from_str(&stale_raw).unwrap();
        assert_eq!(stale["wiki"]["freshness"], "stale");
        assert_eq!(stale["telemetry"]["freshness"]["changed_count"], 1);
        assert_eq!(stale["telemetry"]["freshness"]["uncovered_count"], 1);
        assert_eq!(stale["telemetry"]["freshness"]["stale_page_count"], 0);
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn prime_clamps_implicit_instruction_budget_but_rejects_explicit_overage() {
        let (base, wiki) = temp_wiki("prime-instruction-clamp");
        let mut options = PrimeOptions {
            cwd: None,
            query: "small task".into(),
            tokens: Some(500),
            instruction_tokens: None,
            limit: Some(5),
            max_per_section: Some(5),
            since: None,
            cursor: 0,
            context_hash: None,
        };
        let raw = prime(&wiki, &options, true).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["telemetry"]["budget_tokens"], 500);

        options.instruction_tokens = Some(501);
        let error = prime(&wiki, &options, true).unwrap_err().to_string();
        assert!(error.contains("instruction token budget"), "{error}");
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn prime_state_delta_is_query_independent_and_keeps_standing_text() {
        let (base, wiki) = temp_wiki("prime-state-delta");
        std::fs::create_dir_all(wiki.dir.join("pages/guides")).unwrap();
        for (id, body) in [
            ("guides/alpha", "**Alpha topic.** Alpha-only guidance."),
            ("guides/beta", "**Beta topic.** Beta-only guidance."),
        ] {
            let page = Page::parse(id, body);
            std::fs::write(wiki.page_path(id).unwrap(), page.render()).unwrap();
        }
        let mut rule = Page::parse(
            "standing-rule",
            "**Standing rule.** Always preserve the state contract.",
        );
        rule.fm.pin = true;
        rule.fm.pin_level = Some(PinLevel::Instruction);
        std::fs::write(wiki.page_path("standing-rule").unwrap(), rule.render()).unwrap();

        let options = PrimeOptions {
            cwd: None,
            query: "alpha topic".into(),
            tokens: Some(2_000),
            instruction_tokens: Some(500),
            limit: Some(1),
            max_per_section: Some(5),
            since: None,
            cursor: 0,
            context_hash: None,
        };
        let first: serde_json::Value =
            serde_json::from_str(&prime(&wiki, &options, true).unwrap()).unwrap();
        let state_hash = first["state_hash"].as_str().unwrap().to_string();
        let first_context = first["context_hash"].as_str().unwrap().to_string();
        assert!(!first["sections"].as_array().unwrap().is_empty());
        let continuation_argv = first["continuation_argv"].as_array().unwrap();
        assert!(continuation_argv
            .windows(2)
            .any(|pair| pair[0] == "--since" && pair[1] == state_hash));
        assert!(continuation_argv
            .windows(2)
            .any(|pair| pair[0] == "--context-hash" && pair[1] == first_context));

        let second_options = PrimeOptions {
            query: "beta topic".into(),
            since: Some(state_hash.clone()),
            ..options.clone()
        };
        let second: serde_json::Value =
            serde_json::from_str(&prime(&wiki, &second_options, true).unwrap()).unwrap();
        assert_eq!(second["state_hash"], state_hash);
        assert_ne!(second["context_hash"], first_context);
        assert_eq!(second["unchanged_since"], true);
        assert!(second["sections"].as_array().unwrap().is_empty());
        assert_eq!(second["suggested_pages"][0]["id"], "guides/beta");
        assert!(second["suggested_pages"][0].get("excerpt").is_none());
        assert!(second["instructions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == "standing-rule"));

        let changed_options = PrimeOptions {
            tokens: Some(2_500),
            ..second_options
        };
        let changed: serde_json::Value =
            serde_json::from_str(&prime(&wiki, &changed_options, true).unwrap()).unwrap();
        assert_eq!(changed["state_hash"], state_hash);
        assert_ne!(changed["context_hash"], second["context_hash"]);
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn prime_state_hash_tracks_catalog_pin_section_config_and_freshness_drift() {
        let (base, mut wiki) = temp_wiki("prime-state-drift");
        let project = base.join("project");
        let revision = init_test_project(&project);
        wiki.config.project_roots = vec![project.to_string_lossy().to_string()];
        wiki.config.last_ingest_commit = Some(revision);
        let page_path = wiki.page_path("knowledge").unwrap();
        let baseline_page = Page::parse("knowledge", "**Knowledge.** Stable content.");
        let baseline_raw = baseline_page.render();
        std::fs::write(&page_path, &baseline_raw).unwrap();

        let options = PrimeOptions {
            cwd: Some(project.clone()),
            query: "knowledge".into(),
            tokens: Some(2_000),
            instruction_tokens: Some(500),
            limit: Some(5),
            max_per_section: Some(5),
            since: None,
            cursor: 0,
            context_hash: None,
        };
        let state = |wiki: &Wiki| -> String {
            let value: serde_json::Value =
                serde_json::from_str(&prime(wiki, &options, true).unwrap()).unwrap();
            value["state_hash"].as_str().unwrap().to_string()
        };
        let baseline = state(&wiki);

        let edited = Page::parse("knowledge", "**Knowledge.** Edited content.");
        std::fs::write(&page_path, edited.render()).unwrap();
        assert_ne!(state(&wiki), baseline);
        std::fs::write(&page_path, &baseline_raw).unwrap();
        assert_eq!(state(&wiki), baseline);

        let mut pinned = baseline_page.clone();
        pinned.fm.pin = true;
        pinned.fm.pin_level = Some(PinLevel::Summary);
        std::fs::write(&page_path, pinned.render()).unwrap();
        assert_ne!(state(&wiki), baseline);
        std::fs::write(&page_path, &baseline_raw).unwrap();

        wiki.config.sections.insert(
            "operations".into(),
            crate::wiki::SectionConfig {
                description: "Operational knowledge".into(),
                ..Default::default()
            },
        );
        let section_drift = state(&wiki);
        assert_ne!(section_drift, baseline);
        wiki.config.sections.remove("operations");

        wiki.config.description = "changed config".into();
        assert_ne!(state(&wiki), baseline);
        wiki.config.description.clear();
        assert_eq!(state(&wiki), baseline);

        std::fs::write(project.join("src/lib.rs"), "pub fn changed() {}\n").unwrap();
        assert_ne!(state(&wiki), baseline);
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn discoverable_pins_are_metadata_only_and_budget_adaptive() {
        let (base, wiki) = temp_wiki("discoverable-budget");
        for index in 0..30 {
            let id = format!("reference-{index:02}");
            let mut page = Page::parse(
                &id,
                &format!(
                    "**Reference {index}.** SECRET-DISCOVERABLE-BODY-{index} must stay on demand."
                ),
            );
            page.fm.pin = true;
            page.fm.pin_level = Some(PinLevel::Discoverable);
            page.fm.description =
                "A deliberately long discoverable reference description ".repeat(12);
            std::fs::write(wiki.page_path(&id).unwrap(), page.render()).unwrap();
        }
        let options = PrimeOptions {
            cwd: None,
            query: "no matching task terms".into(),
            tokens: Some(700),
            instruction_tokens: Some(100),
            limit: Some(5),
            max_per_section: Some(5),
            since: None,
            cursor: 0,
            context_hash: None,
        };
        let raw = prime(&wiki, &options, true).unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(value["instructions"].as_array().unwrap().is_empty());
        assert_eq!(value["telemetry"]["instruction_tokens"], 0);
        assert!(value["omissions"]["discoverable_pages"].as_u64().unwrap() > 0);
        assert_eq!(value["discoverable_next_command"], "wookie context");
        assert!(!raw.contains("SECRET-DISCOVERABLE-BODY"));
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn human_prime_titles_rows_and_never_repeats_an_oversized_suggestion_cursor() {
        let (base, wiki) = temp_wiki("human-prime-budget-fallback");
        std::fs::create_dir_all(wiki.dir.join("pages/guides")).unwrap();
        let mut target = Page::parse(
            "guides/oversized-target",
            "**Quasarneedle target.** Detailed task guidance.",
        );
        target.fm.title = "Oversized Target".into();
        target.fm.description = "Long target description ".repeat(20);
        std::fs::write(
            wiki.page_path("guides/oversized-target").unwrap(),
            target.render(),
        )
        .unwrap();
        let mut reference = Page::parse(
            "operator-reference",
            "**Operator reference.** Body stays on demand.",
        );
        reference.fm.title = "Operator Reference".into();
        reference.fm.description = "The compact operator catalog".into();
        reference.fm.pin = true;
        reference.fm.pin_level = Some(PinLevel::Discoverable);
        std::fs::write(
            wiki.page_path("operator-reference").unwrap(),
            reference.render(),
        )
        .unwrap();

        let mut options = PrimeOptions {
            cwd: None,
            query: "quasarneedle".into(),
            tokens: Some(2_000),
            instruction_tokens: Some(100),
            limit: Some(1),
            max_per_section: Some(1),
            since: None,
            cursor: 0,
            context_hash: None,
        };
        let roomy = prime(&wiki, &options, false).unwrap();
        assert!(
            roomy.contains("guides/oversized-target — Oversized Target: Long target description"),
            "{roomy}"
        );
        assert!(
            roomy.contains("operator-reference — Operator Reference: The compact operator catalog"),
            "{roomy}"
        );

        options.tokens = Some(300);
        let tight = prime(&wiki, &options, false).unwrap();
        assert!(
            tight.contains("wookie read guides/oversized-target"),
            "{tight}"
        );
        assert!(!tight.contains("cursor 0"), "{tight}");
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn retrieval_cursors_are_bound_to_query_options_and_catalog() {
        let (base, wiki) = temp_wiki("cursor-binding");
        for index in 0..4 {
            let id = format!("guides/cache-{index}");
            std::fs::create_dir_all(wiki.dir.join("pages/guides")).unwrap();
            let page = Page::parse(&id, &format!("**Cache {index}.** Cache behavior {index}."));
            std::fs::write(wiki.page_path(&id).unwrap(), page.render()).unwrap();
        }

        let mut search_options = SearchOptions {
            cwd: None,
            query: "cache".into(),
            tag: None,
            limit: Some(1),
            tokens: Some(1_500),
            excerpt_lines: Some(1),
            cursor: 0,
            context_hash: None,
            regex: false,
            all: false,
        };
        let first: serde_json::Value =
            serde_json::from_str(&search_with_options(&wiki, &search_options, true).unwrap())
                .unwrap();
        let search_hash = first["context_hash"].as_str().unwrap().to_string();
        let search_cursor = first["continuation"].as_u64().unwrap() as usize;
        search_options.cursor = search_cursor;
        let missing = search_with_options(&wiki, &search_options, true)
            .unwrap_err()
            .to_string();
        assert!(missing.contains("not bound"), "{missing}");
        search_options.context_hash = Some(search_hash.clone());
        assert!(search_with_options(&wiki, &search_options, true).is_ok());
        search_options.query = "behavior".into();
        let changed_query = search_with_options(&wiki, &search_options, true)
            .unwrap_err()
            .to_string();
        assert!(changed_query.contains("not bound"), "{changed_query}");

        let mut prime_options = PrimeOptions {
            cwd: None,
            query: "cache".into(),
            tokens: Some(1_500),
            instruction_tokens: Some(200),
            limit: Some(1),
            max_per_section: Some(5),
            since: None,
            cursor: 0,
            context_hash: None,
        };
        let first: serde_json::Value =
            serde_json::from_str(&prime(&wiki, &prime_options, true).unwrap()).unwrap();
        let prime_hash = first["context_hash"].as_str().unwrap().to_string();
        let state_hash = first["state_hash"].as_str().unwrap().to_string();
        let prime_cursor = first["continuation"].as_u64().unwrap() as usize;
        prime_options.cursor = prime_cursor;
        let missing = prime(&wiki, &prime_options, true).unwrap_err().to_string();
        assert!(missing.contains("not bound"), "{missing}");

        prime_options.since = Some(state_hash);
        let state_only = prime(&wiki, &prime_options, true).unwrap_err().to_string();
        assert!(state_only.contains("not bound"), "{state_only}");
        prime_options.context_hash = Some(prime_hash.clone());
        assert!(prime(&wiki, &prime_options, true).is_ok());
        prime_options.context_hash = None;
        prime_options.since = Some(prime_hash.clone());
        assert!(prime(&wiki, &prime_options, true).is_ok());
        prime_options.query = "behavior".into();
        let changed_query = prime(&wiki, &prime_options, true).unwrap_err().to_string();
        assert!(changed_query.contains("not bound"), "{changed_query}");
        prime_options.query = "cache".into();
        prime_options.context_hash = Some(prime_hash.clone());

        let page = Page::parse("guides/cache-new", "**New cache.** Cache state changed.");
        std::fs::write(wiki.page_path("guides/cache-new").unwrap(), page.render()).unwrap();
        let changed_catalog = prime(&wiki, &prime_options, true).unwrap_err().to_string();
        assert!(changed_catalog.contains("not bound"), "{changed_catalog}");
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn read_expansion_has_depth_and_breadth_hard_limits() {
        let (base, wiki) = temp_wiki("read-expansion-bounds");
        let lone = Page::parse("lone", "**Lone.** No links live here.");
        std::fs::write(wiki.page_path("lone").unwrap(), lone.render()).unwrap();
        let no_links: serde_json::Value =
            serde_json::from_str(&read(&wiki, "lone", MAX_READ_EXPAND_DEPTH, true).unwrap())
                .unwrap();
        assert_eq!(no_links["linked"].as_array().unwrap().len(), 0);

        let error = read(&wiki, "lone", usize::MAX, true)
            .unwrap_err()
            .to_string();
        assert!(error.contains("must not exceed"), "{error}");

        let mut links = String::from("**Root.** Links to a broad catalog.\n\n");
        for index in 0..120 {
            let id = format!("linked-{index:03}");
            let page = Page::parse(&id, &format!("**Linked {index}.** Summary {index}."));
            std::fs::write(wiki.page_path(&id).unwrap(), page.render()).unwrap();
            let _ = write!(links, "[[{id}]] ");
        }
        let root = Page::parse("root", &links);
        std::fs::write(wiki.page_path("root").unwrap(), root.render()).unwrap();
        let expanded: serde_json::Value =
            serde_json::from_str(&read(&wiki, "root", 1, true).unwrap()).unwrap();
        assert_eq!(
            expanded["linked"].as_array().unwrap().len(),
            MAX_READ_EXPANDED_PAGES
        );
        assert_eq!(expanded["linked_omitted"], 20);
        assert!(expanded["continuation"].is_string());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn failed_move_restores_rewritten_backlinks() {
        use std::os::unix::fs::PermissionsExt;

        let (base, wiki) = temp_wiki("move-rollback");
        for (id, body) in [
            ("old", "Old page with a [[old]] self-link."),
            ("a-ref", "First backlink to [[old]]."),
            ("blocked/ref", "Second backlink to [[old]]."),
        ] {
            let mut page = Page::parse(id, body);
            wiki.save_page(&mut page, false).unwrap();
        }
        let blocked_dir = wiki.pages_dir().join("blocked");
        std::fs::set_permissions(&blocked_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

        let error = mv(&wiki, "old", "new", false).unwrap_err().to_string();

        std::fs::set_permissions(&blocked_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(error.contains("rolled back"), "{error}");
        assert!(wiki.exists("old"));
        assert!(!wiki.exists("new"));
        assert!(wiki.load_page("a-ref").unwrap().body.contains("[[old]]"));
        assert!(wiki
            .load_page("blocked/ref")
            .unwrap()
            .body
            .contains("[[old]]"));
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn critique_paths_reject_controls_and_hard_bounds() {
        let valid = vec![
            "src/a file.rs".to_string(),
            "-leading-dash.rs".to_string(),
            ":tracked.rs".to_string(),
        ];
        assert_eq!(validate_critique_paths(&valid).unwrap(), valid);

        let control = validate_critique_paths(&["src/unsafe\nname.rs".to_string()])
            .unwrap_err()
            .to_string();
        assert!(control.contains("control character"), "{control}");

        let oversized = "x".repeat(MAX_CRITIQUE_PATH_BYTES + 1);
        let error = validate_critique_paths(&[oversized])
            .unwrap_err()
            .to_string();
        assert!(error.contains("exceeds"), "{error}");

        for unsafe_path in [
            "/etc/passwd",
            "../../secret",
            "src/../secret",
            "src/./file.rs",
            "src//file.rs",
            "src/file.rs/",
            "src\\file.rs",
        ] {
            let error = validate_critique_paths(&[unsafe_path.to_string()])
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("project-relative")
                    || error.contains("component")
                    || error.contains("backslash"),
                "{unsafe_path:?} unexpectedly failed with: {error}"
            );
        }
    }

    #[test]
    fn critique_rejects_explicit_paths_outside_the_project() {
        let (base, wiki) = temp_wiki("critique-path-boundary");
        let project = base.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let paths = vec!["../outside.txt".to_string()];
        let options = CritiqueOptions {
            project_root: Some(&project),
            revision: None,
            section: None,
            since: None,
            staged: false,
            paths: &paths,
            tokens: None,
            all: true,
            json: true,
        };

        let error = critique(&wiki, &project, &options).unwrap_err().to_string();
        assert!(error.contains("component"), "{error}");
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn critique_git_invocation_is_structured_argv() {
        let argv = git_argv(
            Path::new("/tmp/project with spaces"),
            vec![
                "diff".to_string(),
                "HEAD".to_string(),
                "--".to_string(),
                "-leading-dash.rs".to_string(),
                "semi;colon.rs".to_string(),
            ],
        );
        assert_eq!(argv[0], "git");
        assert_eq!(argv[1], "-C");
        assert_eq!(argv[2], "/tmp/project with spaces");
        assert_eq!(argv[3], "--literal-pathspecs");
        assert_eq!(argv[6], "--");
        assert_eq!(argv[7], "-leading-dash.rs");

        let rendered = serde_json::to_string(&argv).unwrap();
        assert_eq!(
            serde_json::from_str::<Vec<String>>(&rendered).unwrap(),
            argv
        );
    }

    #[test]
    fn critique_keeps_machine_paths_lossless_and_escapes_human_controls() {
        let (base, wiki) = temp_wiki("critique-unusual-paths");
        let project = base.join("project");
        init_test_project(&project);
        let with_newline = "line\nbreak.rs";
        let unicode = "review-é space.rs";
        std::fs::write(project.join(with_newline), "pub const LINE: bool = true;\n").unwrap();
        std::fs::write(project.join(unicode), "pub const UTF8: bool = true;\n").unwrap();

        let paths: Vec<String> = Vec::new();
        let json_options = CritiqueOptions {
            project_root: Some(&project),
            revision: None,
            section: None,
            since: None,
            staged: false,
            paths: &paths,
            tokens: None,
            all: true,
            json: true,
        };
        let machine: serde_json::Value =
            serde_json::from_str(&critique(&wiki, &project, &json_options).unwrap()).unwrap();
        let files = machine["data"]["files"].as_array().unwrap();
        assert!(files.iter().any(|path| path.as_str() == Some(with_newline)));
        assert!(files.iter().any(|path| path.as_str() == Some(unicode)));

        let human_options = CritiqueOptions {
            json: false,
            ..json_options
        };
        let human = critique(&wiki, &project, &human_options).unwrap();
        assert!(human.contains("- line\\nbreak.rs"), "{human}");
        assert!(human.contains("- review-é space.rs"), "{human}");
        assert!(!human.contains("- line\nbreak.rs"), "{human}");
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn compact_critique_json_projects_files_and_all_is_exhaustive() {
        let (base, wiki) = temp_wiki("critique-file-projection");
        let project = base.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let paths = (0..75)
            .map(|index| format!("src/file-{index:03}.rs"))
            .collect::<Vec<_>>();

        let compact_options = CritiqueOptions {
            project_root: Some(&project),
            revision: None,
            section: None,
            since: None,
            staged: false,
            paths: &paths,
            tokens: Some(4_000),
            all: false,
            json: true,
        };
        let compact: serde_json::Value =
            serde_json::from_str(&critique(&wiki, &project, &compact_options).unwrap()).unwrap();
        assert_eq!(compact["data"]["files"].as_array().unwrap().len(), 50);
        assert_eq!(compact["data"]["file_projection"]["total"], 75);
        assert_eq!(compact["data"]["file_projection"]["returned"], 50);
        assert_eq!(compact["data"]["file_projection"]["omitted"], 25);
        assert_eq!(compact["data"]["omissions"]["files"], 25);
        assert!(compact["data"]["file_projection"]["continuation"].is_string());

        let exhaustive_options = CritiqueOptions {
            tokens: None,
            all: true,
            ..compact_options
        };
        let exhaustive: serde_json::Value =
            serde_json::from_str(&critique(&wiki, &project, &exhaustive_options).unwrap()).unwrap();
        assert_eq!(exhaustive["data"]["files"].as_array().unwrap().len(), 75);
        assert_eq!(exhaustive["data"]["file_projection"]["omitted"], 0);
        assert_eq!(exhaustive["data"]["file_projection"]["exhaustive"], true);
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn critique_revision_paths_are_literal_in_execution_and_reported_argv() {
        let (base, wiki) = temp_wiki("critique-literal-pathspec");
        let project = base.join("project");
        let revision = init_test_project(&project);
        let paths = vec![":(literal)src/lib.rs".to_string()];
        let options = CritiqueOptions {
            project_root: Some(&project),
            revision: Some(&revision),
            section: None,
            since: None,
            staged: false,
            paths: &paths,
            tokens: None,
            all: true,
            json: true,
        };

        let output: serde_json::Value =
            serde_json::from_str(&critique(&wiki, &project, &options).unwrap()).unwrap();
        assert!(output["data"]["files"].as_array().unwrap().is_empty());
        let argv = output["data"]["diff_argv"].as_array().unwrap();
        assert_eq!(argv[3], "--literal-pathspecs");
        assert!(argv
            .iter()
            .any(|argument| argument.as_str() == Some(":(literal)src/lib.rs")));
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn human_critique_exhaustive_continuation_pins_moving_revisions() {
        let (base, wiki) = temp_wiki("critique-pinned-continuation");
        let project = base.join("project");
        let base_revision = init_test_project(&project);
        std::fs::write(project.join("src/lib.rs"), "pub fn changed() {}\n").unwrap();
        git_ok(&project, &["add", "src/lib.rs"]);
        git_ok(&project, &["commit", "-qm", "change"]);
        let target_revision = git_ok(&project, &["rev-parse", "HEAD"]);
        let paths = Vec::new();
        let options = CritiqueOptions {
            project_root: Some(&project),
            revision: None,
            section: None,
            since: Some("HEAD~1"),
            staged: false,
            paths: &paths,
            tokens: Some(4_000),
            all: false,
            json: false,
        };

        let output = critique(&wiki, &project, &options).unwrap();
        let marker =
            "Exhaustive continuation argv (invoke directly; do not evaluate with a shell): ";
        let encoded = output
            .split_once(marker)
            .map(|(_, encoded)| encoded)
            .expect("missing exhaustive continuation argv");
        let argv: Vec<String> = serde_json::from_str(encoded).unwrap();
        let since = argv
            .iter()
            .position(|argument| argument == "--since")
            .unwrap();
        let revision = argv
            .iter()
            .position(|argument| argument == "--revision")
            .unwrap();
        assert_eq!(argv[since + 1], base_revision);
        assert_eq!(argv[revision + 1], target_revision);
        assert!(!argv.iter().any(|argument| argument == "HEAD~1"));
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn compact_critique_projects_many_rules_and_all_is_exhaustive() {
        let (base, wiki) = temp_wiki("critique-rule-projection");
        for index in 0..100 {
            let id = format!("style/rule-{index:03}");
            let body = format!(
                "**Rule {index}.** {}",
                "This deliberately substantial summary exercises compact critique projection. "
                    .repeat(8)
            );
            let page = Page::parse(&id, &body);
            let path = wiki.page_path(&id).unwrap();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, page.render()).unwrap();
        }
        let checks = Page::parse(
            "style/checks",
            "**Style checks.** Review each style rule against the selected files.",
        );
        std::fs::write(wiki.page_path("style/checks").unwrap(), checks.render()).unwrap();
        let project = base.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let paths = vec!["src/lib.rs".to_string()];
        let compact_options = CritiqueOptions {
            project_root: Some(&project),
            revision: None,
            section: None,
            since: None,
            staged: false,
            paths: &paths,
            tokens: Some(4_000),
            all: false,
            json: true,
        };

        let rendered = critique(&wiki, &project, &compact_options).unwrap();
        assert!(retrieval::estimate_tokens(&rendered) <= 4_000);
        let compact: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        let returned = compact["data"]["rules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|section| section["rules"].as_array().unwrap().len())
            .sum::<usize>();
        assert_eq!(compact["data"]["rule_projection"]["rules"]["total"], 100);
        assert_eq!(
            compact["data"]["rule_projection"]["rules"]["returned"],
            returned
        );
        assert!(returned <= MAX_COMPACT_CRITIQUE_RULES);
        assert_eq!(
            compact["data"]["rule_projection"]["rules"]["omitted"],
            100 - returned
        );
        assert!(
            compact["data"]["rule_projection"]["rules"]["omitted"]
                .as_u64()
                .unwrap()
                > 0
        );
        assert_eq!(
            compact["data"]["omissions"]["rule_entries"],
            compact["data"]["rule_projection"]["rules"]["omitted"]
        );
        assert_eq!(compact["data"]["omissions"]["rule_bodies"], 100);
        assert!(compact["data"]["rule_projection"]["continuation"].is_string());
        assert!(compact["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| {
                item["code"] == report::code::MISSING_CHECKS && item["page"] == "workflow/checks"
            }));

        let human_options = CritiqueOptions {
            project_root: Some(&project),
            revision: None,
            section: None,
            since: None,
            staged: false,
            paths: &paths,
            tokens: Some(4_000),
            all: false,
            json: false,
        };
        let human = critique(&wiki, &project, &human_options).unwrap();
        assert!(retrieval::estimate_tokens(&human) <= 4_000);
        let human_rules = human.matches("--- Rule (").count();
        assert!(human_rules > 0 && human_rules <= MAX_COMPACT_CRITIQUE_RULES);
        assert!(human_rules < 100);
        assert!(human.contains("Projection: sections"), "{human}");
        assert!(human.contains("Exhaustive continuation argv"), "{human}");
        assert!(human.contains("\"--all\""), "{human}");
        assert!(!human.contains("style/rule-099"), "{human}");

        let exhaustive_options = CritiqueOptions {
            tokens: None,
            all: true,
            ..compact_options
        };
        let exhaustive: serde_json::Value =
            serde_json::from_str(&critique(&wiki, &project, &exhaustive_options).unwrap()).unwrap();
        let exhaustive_rules = exhaustive["data"]["rules"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|section| section["rules"].as_array().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(exhaustive_rules.len(), 100);
        assert!(exhaustive_rules.iter().all(|rule| rule["body"].is_string()));
        assert_eq!(exhaustive["data"]["rule_projection"]["rules"]["omitted"], 0);
        assert_eq!(exhaustive["data"]["rule_projection"]["exhaustive"], true);
        assert!(exhaustive["data"]["rule_projection"]["continuation"].is_null());
        assert_eq!(exhaustive["data"]["omissions"]["rule_bodies"], 0);
        std::fs::remove_dir_all(base).unwrap();
    }
}
