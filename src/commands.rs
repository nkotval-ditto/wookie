//! Every wookie verb, shared by the CLI and the MCP server. Each function
//! returns its output as a string; callers decide where it goes.

use crate::config::GlobalConfig;
use crate::page::{humanize, rewrite_links, today, Page};
use crate::wiki::{self, Wiki};
use anyhow::{bail, Result};
use std::collections::{BTreeMap, HashSet};
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

    let dir = home.join(&slug);
    std::fs::create_dir_all(dir.join("pages"))?;
    let description = description.unwrap_or_default();
    let wiki_config = wiki::WikiConfig {
        name: slug.clone(),
        description: description.clone(),
        project_roots: vec![project_root.clone()],
        auto_commit: None,
        last_ingest_commit: None,
        sections: wiki::default_sections(),
    };
    std::fs::write(dir.join("wookie.toml"), toml::to_string_pretty(&wiki_config)?)?;
    GlobalConfig::load(home)?.save(home)?;

    let w = wiki::open(home, &slug)?;
    w.ensure_gitignore()?;
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
            aliases: vec![humanize(&slug)],
            extra: vec![],
        },
        body: format!(
            "**The front door of this wiki.** It maps the project at `{project_root}`; \
             every page below is reachable by hovering or clicking a wikilink.\n\n\
             Add pages with `wookie new <id>` and connect them with wikilinks like `[[another-page]]`. \
             Run `wookie context` for an overview and `wookie doctor` to check health.\n\n\
             > [!tip] In Obsidian, hover any [[link]] to preview a page's summary paragraph."
        ),
    };
    w.save_page(&mut index, false)?;
    w.commit("wookie: init");

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
                .map(|a| a.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(", "))
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
        let flags = if flags.is_empty() { String::new() } else { format!(" [{}]", flags.join(", ")) };
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
    let pinned: Vec<&Page> = pages.iter().filter(|p| p.fm.pin && !p.is_stub()).collect();
    if json {
        let mut v = grouped_json(w);
        v["wiki"] = serde_json::json!(w.slug);
        v["description"] = serde_json::json!(w.config.description);
        v["project_roots"] = serde_json::json!(w.config.project_roots);
        v["pinned"] = pinned
            .iter()
            .map(|p| serde_json::json!({"id": p.id, "body": p.body}))
            .collect();
        return Ok(v.to_string());
    }
    let mut out = String::new();
    let _ = writeln!(out, "Wiki: {} — {}", w.slug, w.config.description);
    let _ = writeln!(out, "Project roots: {}", w.config.project_roots.join(", "));
    let _ = writeln!(out, "{} pages, {} stubs needing content", pages.len(), stubs);
    if !pinned.is_empty() {
        let _ = writeln!(out, "\n== Pinned instructions (always follow these) ==");
        for p in &pinned {
            let _ = writeln!(out, "\n### {} ({})\n{}", p.fm.title, p.id, p.body.trim_end());
        }
        let _ = writeln!(out, "\n== Reference pages (read on demand) ==");
    }
    render_grouped(w, &mut out);
    let _ = writeln!(
        out,
        "\nRead a page with linked context: wookie read <id> --expand\nSearch: wookie search <query> | Grow: wookie expand"
    );
    Ok(out.trim_end().to_string())
}

pub fn read(w: &Wiki, id: &str, expand: usize, json: bool) -> Result<String> {
    let page = w.load_page(id)?;

    let mut linked: Vec<Page> = vec![];
    let mut broken: Vec<String> = vec![];
    if expand > 0 {
        let mut visited: HashSet<String> = HashSet::from([id.to_string()]);
        let mut frontier = page.links();
        for _ in 0..expand {
            let mut next = vec![];
            for target in frontier {
                if !visited.insert(target.clone()) {
                    continue;
                }
                if !w.exists(&target) {
                    broken.push(target);
                    continue;
                }
                let p = w.load_page(&target)?;
                next.extend(p.links());
                linked.push(p);
            }
            frontier = next;
        }
    }

    if json {
        return Ok(serde_json::json!({
            "id": page.id, "frontmatter": page.fm, "body": page.body,
            "linked": linked.iter().map(|p| serde_json::json!({
                "id": p.id, "title": p.fm.title, "description": p.fm.description,
                "summary": p.summary(), "stub": p.is_stub(),
            })).collect::<Vec<_>>(),
            "broken_links": broken,
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
                p.fm.description,
                stub,
                indent(&p.summary())
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
    Ok(out.trim_end().to_string())
}

pub fn new_page(
    w: &Wiki,
    id: &str,
    title: Option<String>,
    description: Option<String>,
    tags: Vec<String>,
    sources: Vec<String>,
    pin: bool,
    body: Option<String>,
    json: bool,
) -> Result<String> {
    wiki::validate_id(id)?;
    w.assert_writable(id)?;
    if w.exists(id) {
        bail!("page '{id}' already exists — use `wookie write {id}` to replace its body");
    }
    let has_body = body.as_deref().map(|b| !b.trim().is_empty()).unwrap_or(false);
    let title_final = title.unwrap_or_else(|| humanize(id));
    let mut page = Page {
        id: id.to_string(),
        fm: crate::page::Frontmatter {
            title: title_final.clone(),
            description: if has_body { String::new() } else { format!("TODO: describe {id}") },
            tags,
            created: today(),
            updated: today(),
            status: if has_body { None } else { Some("stub".into()) },
            sources,
            pin,
            aliases: vec![title_final.clone()],
            extra: vec![],
        },
        body: body.filter(|b| !b.trim().is_empty()).unwrap_or_else(|| {
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
    w.save_page(&mut page, false)?;
    w.commit(&format!("wookie: new {id}"));

    let filing_note = match section_of(id) {
        Some(s) if w.sections().contains_key(s) => String::new(),
        _ if id == "index" => String::new(),
        _ => format!(
            "\nNote: '{id}' is unfiled. Known sections: {}. Consider `wookie mv` into one (locked sections need user approval + `wookie unlock` first).",
            w.sections().keys().cloned().collect::<Vec<_>>().join(", ")
        ),
    };
    if json {
        return Ok(serde_json::json!({"id": id, "stub": page.is_stub(), "unfiled": !filing_note.is_empty()}).to_string());
    }
    if page.is_stub() {
        Ok(format!(
            "Created stub '{id}'. Fill it by piping a body: wookie write {id} <<'EOF' ... EOF{filing_note}"
        ))
    } else {
        Ok(format!("Created page '{id}'.{filing_note}"))
    }
}

fn first_sentence(text: &str) -> String {
    let flat = text.replace('\n', " ");
    match flat.find(". ") {
        Some(i) => flat[..=i].trim().to_string(),
        None => flat.trim().trim_end_matches('.').to_string() + ".",
    }
}

pub fn write(
    w: &Wiki,
    id: &str,
    body: &str,
    append: bool,
    sources: Option<Vec<String>>,
    pin: Option<bool>,
    description: Option<String>,
    json: bool,
) -> Result<String> {
    if body.trim().is_empty() {
        bail!("empty body — pipe page content via stdin (e.g. wookie write {id} <<'EOF' ... EOF)");
    }
    wiki::validate_id(id)?;
    w.assert_writable(id)?;
    let mut page = match w.load_page(id) {
        Ok(p) => p,
        Err(_) => bail!("page '{id}' does not exist — create it with `wookie new {id}`"),
    };
    if append {
        page.body = format!("{}\n\n{}", page.body.trim_end(), body.trim());
    } else {
        page.body = body.trim().to_string();
    }
    // Real content clears stub status; description follows the new summary if
    // it was still a placeholder.
    page.fm.status = None;
    if let Some(sources) = sources {
        page.fm.sources = sources;
    }
    if let Some(pin) = pin {
        page.fm.pin = pin;
    }
    if page.fm.description.is_empty() || page.fm.description.starts_with("TODO") {
        page.fm.description = first_sentence(&page.summary());
    }
    if let Some(description) = description {
        page.fm.description = description;
    }
    w.save_page(&mut page, true)?;
    w.commit(&format!("wookie: write {id}"));

    let broken: Vec<String> = page
        .links()
        .into_iter()
        .filter(|l| !w.exists(l))
        .collect();
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

pub fn rm(w: &Wiki, id: &str, json: bool) -> Result<String> {
    w.assert_writable(id)?;
    let backlinks = w.backlinks(id);
    w.delete_page(id)?;
    w.commit(&format!("wookie: rm {id}"));
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
    w.assert_writable(old)?;
    w.assert_writable(new)?;
    if w.exists(new) {
        bail!("page '{new}' already exists");
    }
    let mut page = w.load_page(old)?;
    let mut rewritten = vec![];
    for other in w.all_pages() {
        if other.id == old {
            continue;
        }
        let (body, changed) = rewrite_links(&other.body, old, new);
        if changed {
            let mut other = other;
            other.body = body;
            // Mechanical link rewrite only; allowed even in locked sections
            // so renames don't strand broken links behind a lock.
            w.save_page_raw(&mut other, false)?;
            rewritten.push(other.id);
        }
    }
    page.id = new.to_string();
    w.save_page(&mut page, false)?;
    w.delete_page(old)?;
    w.commit(&format!("wookie: mv {old} -> {new}"));
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

pub fn expand(w: &Wiki, id: Option<&str>, json: bool) -> Result<String> {
    let pages: Vec<Page> = match id {
        Some(id) => vec![w.load_page(id)?],
        None => w.all_pages(),
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

    let mut created = vec![];
    let mut skipped_locked = vec![];
    for (target, sources) in &missing {
        if w.assert_writable(target).is_err() {
            skipped_locked.push(target.clone());
            continue;
        }
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
                aliases: vec![humanize(target)],
                extra: vec![],
            },
            body: format!(
                "**TODO: define {}.** Replace this with one bold-lead paragraph that \
                 stands alone as the hover summary; link related pages with [[wikilinks]].\n\n\
                 > [!note] Linked from: {}.",
                humanize(target),
                sources.iter().map(|s| format!("[[{s}]]")).collect::<Vec<_>>().join(", ")
            ),
        };
        w.save_page(&mut stub, false)?;
        created.push(target.clone());
    }
    if !created.is_empty() {
        w.commit(&format!("wookie: expand ({} stubs)", created.len()));
    }

    let stubs: Vec<String> = w
        .all_pages()
        .iter()
        .filter(|p| p.is_stub())
        .map(|p| p.id.clone())
        .collect();

    if json {
        return Ok(serde_json::json!({"created": created, "stubs": stubs, "skipped_locked": skipped_locked}).to_string());
    }

    let mut out = String::new();
    if !skipped_locked.is_empty() {
        let _ = writeln!(
            out,
            "Skipped {} broken link(s) into locked sections: {} (ask the user before unlocking).",
            skipped_locked.len(),
            skipped_locked.join(", ")
        );
    }
    if created.is_empty() {
        let _ = writeln!(out, "No broken links found — nothing to stub.");
    } else {
        let _ = writeln!(out, "Created {} stub page(s):", created.len());
        for c in &created {
            let _ = writeln!(out, "- {c}  (linked from {})", missing[c].join(", "));
        }
    }
    if stubs.is_empty() {
        let _ = writeln!(out, "No stubs waiting for content.");
    } else {
        let _ = writeln!(out, "\nStubs needing content ({}):", stubs.len());
        for s in &stubs {
            let _ = writeln!(out, "- {s}");
        }
        let _ = writeln!(
            out,
            "\nTo fill a stub:\n  1. wookie read <id> --expand   (see what links to it and expect from it)\n  2. Pipe the body: wookie write <id> <<'EOF' ... EOF\nFirst paragraph must be a standalone summary. Writing real content clears stub status."
        );
    }
    Ok(out.trim_end().to_string())
}

pub fn search(w: &Wiki, query: &str, tag: Option<&str>, json: bool) -> Result<String> {
    let re = regex::Regex::new(&format!("(?i){query}"))
        .or_else(|_| regex::Regex::new(&format!("(?i){}", regex::escape(query))))?;

    let mut hits = vec![];
    for p in w.all_pages() {
        if let Some(tag) = tag {
            if !p.fm.tags.iter().any(|t| t == tag) {
                continue;
            }
        }
        let meta_hit = re.is_match(&p.id) || re.is_match(&p.fm.title) || re.is_match(&p.fm.description);
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
        return Ok(serde_json::json!({"query": query, "hits": items}).to_string());
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
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["diff", "--name-only", since, "HEAD"])
        .output()?;
    if !out.status.success() {
        bail!(
            "cannot diff against '{since}' in {} — rerun with --full for a fresh ingest",
            root.display()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .filter(|l| !l.is_empty())
        .collect())
}

/// Project files, relative paths. git ls-files when available (respects
/// .gitignore), else a walk that skips hidden and well-known junk dirs.
fn list_project_files(root: &Path) -> Vec<String> {
    if let Ok(out) = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files"])
        .output()
    {
        if out.status.success() {
            let files: Vec<String> = String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(str::to_string)
                .filter(|l| !l.is_empty())
                .collect();
            if !files.is_empty() {
                return files;
            }
        }
    }
    const JUNK: &[&str] = &[
        "node_modules", "target", "dist", "build", "__pycache__", "venv", ".venv", "vendor",
    ];
    walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            e.depth() == 0 || (!name.starts_with('.') && !JUNK.contains(&name.as_ref()))
        })
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| {
            e.path()
                .strip_prefix(root)
                .ok()
                .map(|p| p.to_string_lossy().replace('\\', "/"))
        })
        .collect()
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
    let segs: Vec<String> = dir.split('/').map(slugify).filter(|s| !s.is_empty()).collect();
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
    exts.sort_by(|a, b| b.1.cmp(&a.1));
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
         (link the other [[wikilinks|code pages]] it touches).\n\n\
         ## Key files\n\n\
         {}\n\n\
         > [!note] {} files{}.",
        notable.iter().map(|f| format!("- `{f}`")).collect::<Vec<_>>().join("\n"),
        files.len(),
        if main_exts.is_empty() {
            String::new()
        } else {
            format!(" (mostly {})", main_exts.join(", "))
        },
    );
    (description, body)
}

fn seed_code_stub(w: &Wiki, dir: &str, files: &[&String]) -> Result<Option<String>> {
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
            aliases: vec![humanize(&id)],
            extra: vec![],
        },
        body,
    };
    w.save_page(&mut stub, false)?;
    Ok(Some(id))
}

pub fn ingest(
    w: &mut Wiki,
    cwd: &Path,
    level: IngestLevel,
    mark: bool,
    full: bool,
    since: Option<&str>,
    json: bool,
) -> Result<String> {
    let root = ingest_root(w, cwd)?;

    if mark {
        let head = head_commit(&root).ok_or_else(|| {
            anyhow::anyhow!("{} is not a git repo — --mark needs git history to diff against later", root.display())
        })?;
        w.config.last_ingest_commit = Some(head.clone());
        w.save_config()?;
        w.commit("wookie: ingest --mark");
        if json {
            return Ok(serde_json::json!({"marked": head}).to_string());
        }
        return Ok(format!("Marked wiki '{}' as synced to commit {}.", w.slug, &head[..8.min(head.len())]));
    }

    let base = since
        .map(str::to_string)
        .or_else(|| if full { None } else { w.config.last_ingest_commit.clone() });

    match base {
        Some(base) => ingest_update(w, &root, &base, level, json),
        None => ingest_fresh(w, &root, level, json),
    }
}

fn ingest_fresh(w: &Wiki, root: &Path, level: IngestLevel, json: bool) -> Result<String> {
    let files = list_project_files(root);
    if files.is_empty() {
        bail!("no files found under {}", root.display());
    }

    // Entry points the agent should read first.
    const ENTRY: &[&str] = &[
        "README.md", "README.rst", "ARCHITECTURE.md", "CONTRIBUTING.md", "CLAUDE.md",
        "Cargo.toml", "package.json", "pyproject.toml", "go.mod", "Makefile", "docker-compose.yml",
    ];
    let entries: Vec<&str> = ENTRY.iter().copied().filter(|e| files.iter().any(|f| f == e)).collect();

    // Seed stubs: top-level dirs always; significant second-level dirs for
    // standard/deep. Capped so a monorepo doesn't explode into stubs.
    let top = dirs_at_depth(&files, 1);
    let mut targets: Vec<(String, Vec<&String>)> = {
        let mut t: Vec<_> = top.into_iter().collect();
        t.sort_by_key(|(_, fs)| std::cmp::Reverse(fs.len()));
        t.truncate(15);
        t
    };
    if level != IngestLevel::Quick {
        let mut second: Vec<_> = dirs_at_depth(&files, 2)
            .into_iter()
            .filter(|(_, fs)| fs.len() >= 3)
            .collect();
        second.sort_by_key(|(_, fs)| std::cmp::Reverse(fs.len()));
        second.truncate(25);
        targets.extend(second);
    }

    let mut created = vec![];
    for (dir, dir_files) in &targets {
        if let Some(id) = seed_code_stub(w, dir, dir_files)? {
            created.push(id);
        }
    }
    if !created.is_empty() {
        w.commit(&format!("wookie: ingest seed ({} stubs)", created.len()));
    }

    if json {
        return Ok(serde_json::json!({
            "mode": "fresh", "level": format!("{level:?}").to_lowercase(),
            "root": root, "files": files.len(),
            "entry_points": entries, "seeded": created,
        })
        .to_string());
    }

    let mut out = String::new();
    let _ = writeln!(
        out,
        "Ingest ({:?}, fresh) — {} files under {}\n",
        level,
        files.len(),
        root.display()
    );
    if created.is_empty() {
        let _ = writeln!(out, "No new stubs seeded (module pages already exist).\n");
    } else {
        let _ = writeln!(out, "Seeded {} module stub(s):", created.len());
        for c in &created {
            let _ = writeln!(out, "- {c}");
        }
        let _ = writeln!(out);
    }
    let _ = writeln!(out, "Worklist — do these now:");
    let _ = writeln!(
        out,
        "1. Read the entry points: {}.",
        if entries.is_empty() { "(none found — skim the file tree)".into() } else { entries.join(", ") }
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
    let last_step = match level { IngestLevel::Quick => 4, IngestLevel::Standard => 5, IngestLevel::Deep => 6 };
    if head_commit(root).is_some() {
        let _ = writeln!(
            out,
            "{last_step}. Run `wookie doctor`, fix what it reports, then record the sync point: `wookie ingest --mark`."
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
        "\nConventions: every page's first paragraph is a standalone summary; set `--sources` to the paths a page documents so future ingests can flag it when that code changes."
    );
    Ok(out.trim_end().to_string())
}

fn ingest_update(w: &Wiki, root: &Path, base: &str, level: IngestLevel, json: bool) -> Result<String> {
    let changed = changed_since(root, base)?;
    if changed.is_empty() {
        let msg = format!("No code changes since {} — wiki is in sync.", &base[..8.min(base.len())]);
        if json {
            return Ok(serde_json::json!({"mode": "update", "changed": [], "stale": []}).to_string());
        }
        return Ok(msg);
    }

    // Map changed files onto pages via their sources prefixes. Per file,
    // only the most specific (longest) matching prefix counts, so a change
    // in src/scheduler/ marks code/src/scheduler stale without also
    // dragging in the code/src parent page.
    let pages = w.all_pages();
    let mut matches: Vec<(&String, usize, &str)> = vec![]; // (file, prefix len, page id)
    for p in &pages {
        for src in &p.fm.sources {
            let prefix = src.trim_end_matches('/');
            for f in &changed {
                if f == prefix || f.starts_with(&format!("{prefix}/")) {
                    matches.push((f, prefix.len(), p.id.as_str()));
                }
            }
        }
    }
    let mut best: BTreeMap<&String, usize> = BTreeMap::new();
    for (f, len, _) in &matches {
        let e = best.entry(f).or_insert(0);
        *e = (*e).max(*len);
    }
    let mut stale: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut covered: HashSet<&String> = HashSet::new();
    for (f, len, id) in &matches {
        covered.insert(f);
        if len == &best[f] {
            stale.entry(id.to_string()).or_default().push(f.to_string());
        }
    }
    let uncovered: Vec<&String> = changed.iter().filter(|f| !covered.contains(f)).collect();

    // New modules that appeared since last ingest get stubs; standard/deep
    // also look one level down, mirroring fresh-ingest seeding.
    let all_files = list_project_files(root);
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
                if let Some(id) = seed_code_stub(w, &dir, &dir_files)? {
                    seeded.push(id);
                }
            }
        }
    }
    if !seeded.is_empty() {
        w.commit(&format!("wookie: ingest seed ({} stubs)", seeded.len()));
    }

    if json {
        return Ok(serde_json::json!({
            "mode": "update", "since": base, "changed": changed,
            "stale": stale.iter().map(|(id, fs)| serde_json::json!({"id": id, "files": fs})).collect::<Vec<_>>(),
            "uncovered": uncovered, "seeded": seeded,
        })
        .to_string());
    }

    let mut out = String::new();
    let _ = writeln!(
        out,
        "Ingest ({:?}, update) — {} file(s) changed since {}\n",
        level,
        changed.len(),
        &base[..8.min(base.len())]
    );
    if stale.is_empty() {
        let _ = writeln!(out, "No existing pages claim the changed files via sources.");
    } else {
        let _ = writeln!(out, "Stale pages (their sources changed):");
        for (id, fs) in &stale {
            let mut fs = fs.clone();
            fs.sort();
            fs.dedup();
            let shown = fs.iter().take(6).cloned().collect::<Vec<_>>().join(", ");
            let more = if fs.len() > 6 { format!(" (+{} more)", fs.len() - 6) } else { String::new() };
            let _ = writeln!(out, "- {id}  <- {shown}{more}");
        }
    }
    if !seeded.is_empty() {
        let _ = writeln!(out, "\nNew module stub(s) seeded:");
        for s in &seeded {
            let _ = writeln!(out, "- {s}");
        }
    }
    if !uncovered.is_empty() {
        let shown = uncovered.iter().take(10).map(|s| s.as_str()).collect::<Vec<_>>().join(", ");
        let more = if uncovered.len() > 10 { format!(" (+{} more)", uncovered.len() - 10) } else { String::new() };
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
    let _ = write!(
        out,
        "\nWorklist — do these now:\n1. For each stale page: `wookie read <id>`, review the changed files (git diff {base} -- <files>), update the page with `wookie write <id>`.\n2. Fill any seeded stubs.{deep_step}\n{}. Run `wookie doctor`, then record the new sync point: `wookie ingest --mark`.",
        if level == IngestLevel::Deep { 4 } else { 3 }
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

/// Assemble the critique briefing: target files + every rules section's
/// checks page and rule pages + the output contract. The agent executes it;
/// wookie only gathers.
pub fn critique(
    w: &Wiki,
    cwd: &Path,
    section: Option<&str>,
    since: Option<&str>,
    staged: bool,
    paths: &[String],
    json: bool,
) -> Result<String> {
    let root = ingest_root(w, cwd)?;

    // Target: what is being critiqued, and how the agent views it.
    let (target_desc, files, diff_cmd) = if !paths.is_empty() {
        (
            format!("{} explicitly given path(s)", paths.len()),
            paths.to_vec(),
            format!("read the files directly under {}", root.display()),
        )
    } else {
        let (range, label) = match (since, staged) {
            (Some(r), _) => (vec!["diff", "--name-only", r, "HEAD"], format!("changes since {r}")),
            (None, true) => (vec!["diff", "--name-only", "--cached"], "staged changes".to_string()),
            (None, false) => (vec!["diff", "--name-only", "HEAD"], "uncommitted changes".to_string()),
        };
        let out = std::process::Command::new("git").arg("-C").arg(&root).args(&range).output()?;
        if !out.status.success() {
            bail!(
                "cannot compute target in {} — pass explicit paths: wookie critique --paths <files>",
                root.display()
            );
        }
        let mut files: Vec<String> = String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .filter(|l| !l.is_empty())
            .collect();
        // Untracked files are the most common critique target; a plain
        // `git diff` never shows them.
        if !staged {
            if let Ok(unt) = std::process::Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(["ls-files", "--others", "--exclude-standard"])
                .output()
            {
                files.extend(
                    String::from_utf8_lossy(&unt.stdout)
                        .lines()
                        .map(str::to_string)
                        .filter(|l| !l.is_empty()),
                );
            }
        }
        files.sort();
        files.dedup();
        let view = range.join(" ").replace("--name-only ", "");
        (label, files, format!("git -C {} {view} (plus untracked files: read them directly)", root.display()))
    };

    // Rules sections, optionally narrowed to one.
    let sections = w.sections();
    let rules: Vec<(String, wiki::SectionConfig)> = sections
        .into_iter()
        .filter(|(name, cfg)| {
            cfg.kind == wiki::SectionKind::Rules && section.map_or(true, |s| s == name)
        })
        .collect();
    if rules.is_empty() {
        bail!(
            "no rules sections{} — mark one in wookie.toml with kind = \"rules\"",
            section.map(|s| format!(" matching '{s}'")).unwrap_or_default()
        );
    }

    if json {
        return Ok(serde_json::json!({
            "target": target_desc, "files": files, "diff_cmd": diff_cmd,
            "sections": rules.iter().map(|(name, _)| name).collect::<Vec<_>>(),
        })
        .to_string());
    }

    let mut out = String::new();
    let _ = writeln!(out, "Critique briefing — wiki '{}', target: {target_desc}", w.slug);
    if files.is_empty() {
        let _ = writeln!(out, "No target files — nothing to critique.");
        return Ok(out.trim_end().to_string());
    }
    let _ = writeln!(out, "Files ({}):", files.len());
    for f in files.iter().take(50) {
        let _ = writeln!(out, "- {f}");
    }
    if files.len() > 50 {
        let _ = writeln!(out, "  (+{} more)", files.len() - 50);
    }
    let _ = writeln!(out, "View the changes: {diff_cmd}");

    for (name, cfg) in &rules {
        let _ = writeln!(out, "\n== Rules: {name}/ — {} ==", cfg.description);
        let checks_id = format!("{name}/checks");
        match w.load_page(&checks_id) {
            Ok(checks) => {
                let _ = writeln!(out, "\n--- How to verify ({checks_id}) ---\n{}", checks.body.trim_end());
            }
            Err(_) => {
                let _ = writeln!(
                    out,
                    "\n(no {checks_id} page — apply the rules below with judgment, and note that this section needs a checks page)"
                );
            }
        }
        let prefix = format!("{name}/");
        let mut any = false;
        for p in w.all_pages() {
            if p.id.starts_with(&prefix) && p.id != checks_id {
                any = true;
                let _ = writeln!(out, "\n--- Rule ({}) ---\n{}", p.id, p.body.trim_end());
            }
        }
        if !any {
            let _ = writeln!(out, "\n(no rule pages in this section yet)");
        }
    }

    let _ = write!(
        out,
        "\n== Output contract ==\n\
         Now EXECUTE this critique against the target:\n\
         1. Review the changes with the command above (and read files as needed).\n\
         2. Check every rule. Report each violation as: severity (error|warn) | rule page id | file:line | what is wrong | suggested fix.\n\
         3. End with a verdict per rules section: pass or fail.\n\
         4. If a rule was unclear or seems outdated, say so — but do NOT edit rules sections; they are locked and changing them needs explicit user permission."
    );
    Ok(out.trim_end().to_string())
}

/// List or edit the wiki's project roots (the resolution source of truth).
pub fn roots(
    w: &mut Wiki,
    add: Option<PathBuf>,
    remove: Option<PathBuf>,
    json: bool,
) -> Result<String> {
    if let Some(path) = add {
        let path = path.canonicalize().unwrap_or(path).to_string_lossy().to_string();
        if !w.config.project_roots.contains(&path) {
            w.config.project_roots.push(path);
            w.save_config()?;
        }
    }
    if let Some(path) = remove {
        let path = path.canonicalize().unwrap_or(path).to_string_lossy().to_string();
        let before = w.config.project_roots.len();
        w.config.project_roots.retain(|r| r != &path);
        if w.config.project_roots.len() == before {
            bail!(
                "{path} is not a project root of '{}' (current: {})",
                w.slug,
                w.config.project_roots.join(", ")
            );
        }
        w.save_config()?;
    }
    if json {
        return Ok(serde_json::json!({"wiki": w.slug, "project_roots": w.config.project_roots}).to_string());
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
    let w = wiki::open(home, slug)?;
    if !force {
        bail!(
            "this permanently deletes wiki '{slug}' and its {} page(s) at {} — rerun with --force to confirm",
            w.page_ids().len(),
            w.dir.display()
        );
    }
    std::fs::remove_dir_all(&w.dir)?;
    if json {
        return Ok(serde_json::json!({"removed": slug}).to_string());
    }
    Ok(format!("Removed wiki '{slug}' ({}).", w.dir.display()))
}

pub fn rename_wiki(home: &Path, old: &str, new: &str, json: bool) -> Result<String> {
    let new = slugify(new);
    if new.is_empty() {
        bail!("new slug is empty after slugification");
    }
    let mut w = wiki::open(home, old)?;
    if wiki::all_wikis(home).contains(&new) {
        bail!("wiki '{new}' already exists");
    }
    std::fs::rename(&w.dir, home.join(&new))?;
    w.dir = home.join(&new);
    w.config.name = new.clone();
    w.save_config()?;
    if json {
        return Ok(serde_json::json!({"from": old, "to": new}).to_string());
    }
    Ok(format!("Renamed wiki '{old}' -> '{new}'."))
}

fn obsidian_app_config() -> PathBuf {
    let home = crate::config::user_home();
    #[cfg(target_os = "macos")]
    return home.join("Library/Application Support/obsidian/obsidian.json");
    #[cfg(target_os = "linux")]
    return home.join(".config/obsidian/obsidian.json");
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    return std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or(home)
        .join("obsidian/obsidian.json");
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
    let cfg_path = obsidian_app_config();
    let raw = std::fs::read_to_string(&cfg_path).unwrap_or_else(|_| "{}".into());
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
    if vaults.values().any(|e| e.get("path").and_then(|p| p.as_str()) == Some(target.as_str())) {
        return Ok(false);
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    vaults.insert(fnv1a_hex(&target), serde_json::json!({ "path": target, "ts": ts }));
    if let Some(parent) = cfg_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&cfg_path, serde_json::to_string(&cfg)?)?;
    Ok(true)
}

/// Open the wiki's pages/ folder as an Obsidian vault.
pub fn obsidian(w: &Wiki, print_only: bool, json: bool) -> Result<String> {
    let vault = w.pages_dir().canonicalize().unwrap_or_else(|_| w.pages_dir());
    // A .obsidian dir holds the vault's local settings; keep it out of wiki
    // history via .gitignore.
    std::fs::create_dir_all(vault.join(".obsidian"))?;
    let gitignore = w.dir.join(".gitignore");
    if !gitignore.exists() {
        std::fs::write(&gitignore, "pages/.obsidian/\n")?;
    }

    let uri = format!(
        "obsidian://open?path={}",
        percent_encode(&vault.to_string_lossy())
    );
    if json {
        return Ok(serde_json::json!({"vault": vault, "uri": uri, "opened": !print_only}).to_string());
    }
    if print_only {
        return Ok(uri);
    }

    // Only when actually opening: --print stays side-effect free.
    let newly_registered = register_obsidian_vault(&vault).unwrap_or(false);

    #[cfg(target_os = "macos")]
    let status = std::process::Command::new("open").arg(&uri).status();
    #[cfg(target_os = "linux")]
    let status = std::process::Command::new("xdg-open").arg(&uri).status();
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let status = std::process::Command::new("cmd").args(["/C", "start", ""]).arg(&uri).status();
    match status {
        Ok(s) if s.success() => {
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

pub fn doctor(w: &Wiki, fix: bool, json: bool) -> Result<(String, usize)> {
    let mut issues: Vec<String> = vec![];
    let mut fixed: Vec<String> = vec![];
    let pages = w.all_pages();
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
                let mut p2 = p.clone();
                w.save_page_raw(&mut p2, true)?;
                fixed.push(format!("normalized frontmatter of '{}'", p.id));
            } else {
                issues.push(format!("missing/invalid frontmatter: '{}'", p.id));
            }
        } else if fix {
            let on_disk = std::fs::read_to_string(w.page_path(&p.id)?).unwrap_or_default();
            if on_disk != p.render() {
                let mut p2 = p.clone();
                w.save_page_raw(&mut p2, false)?;
                fixed.push(format!("reserialized '{}' to canonical format", p.id));
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
    if let (Some(last), Some(root)) = (&w.config.last_ingest_commit, w.config.project_roots.first()) {
        if let Some(head) = head_commit(Path::new(root)) {
            if &head != last {
                issues.push(
                    "code changed since last ingest — run `wookie ingest` for a stale-page worklist".into(),
                );
            }
        }
    }
    if !fixed.is_empty() {
        w.commit("wookie: doctor --fix");
    }

    if json {
        return Ok((serde_json::json!({"issues": issues, "fixed": fixed}).to_string(), issues.len()));
    }
    let mut out = String::new();
    if issues.is_empty() && fixed.is_empty() {
        return Ok((format!("Wiki '{}' is healthy: {} pages, no issues.", w.slug, pages.len()), 0));
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
