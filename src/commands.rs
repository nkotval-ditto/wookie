//! Every wookie verb, shared by the CLI and the MCP server. Each function
//! returns its output as a string; callers decide where it goes.

use crate::config::{GlobalConfig, WikiEntry};
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

    let mut global = GlobalConfig::load(home)?;
    if global.wikis.contains_key(&slug) {
        bail!("wiki '{slug}' already exists (wookie list)");
    }
    for (other, entry) in &global.wikis {
        if entry.project_roots.iter().any(|r| r == &project_root) {
            bail!("{project_root} is already registered to wiki '{other}'");
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
    };
    std::fs::write(dir.join("wookie.toml"), toml::to_string_pretty(&wiki_config)?)?;

    global
        .wikis
        .insert(slug.clone(), WikiEntry { project_roots: vec![project_root.clone()] });
    global.save(home)?;

    let w = wiki::open(home, &slug)?;
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
        },
        body: format!(
            "Wiki for the project at {project_root}, managed by wookie.\n\n\
             Add pages with `wookie new <id>` and connect them with wikilinks like `[[another-page]]`. \
             Run `wookie context` for an overview and `wookie doctor` to check health."
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
    let global = GlobalConfig::load(home)?;
    if global.wikis.is_empty() {
        return Ok("No wikis yet. Run `wookie init` from a project directory.".into());
    }
    let mut rows = vec![];
    for slug in global.wikis.keys() {
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

pub fn toc(w: &Wiki, json: bool) -> Result<String> {
    let rows = toc_rows(w);
    if json {
        let items: Vec<_> = rows
            .iter()
            .map(|(id, d, stub)| serde_json::json!({"id": id, "description": d, "stub": stub}))
            .collect();
        return Ok(serde_json::json!({"wiki": w.slug, "pages": items}).to_string());
    }
    if rows.is_empty() {
        return Ok(format!("Wiki '{}' has no pages yet.", w.slug));
    }
    let mut out = String::new();
    for (id, desc, stub) in rows {
        let marker = if stub { "  [stub]" } else { "" };
        let _ = writeln!(out, "- {id} — {desc}{marker}");
    }
    Ok(out.trim_end().to_string())
}

pub fn context(w: &Wiki, json: bool) -> Result<String> {
    let rows = toc_rows(w);
    let stubs = rows.iter().filter(|(_, _, s)| *s).count();
    if json {
        let items: Vec<_> = rows
            .iter()
            .map(|(id, d, stub)| serde_json::json!({"id": id, "description": d, "stub": stub}))
            .collect();
        return Ok(serde_json::json!({
            "wiki": w.slug,
            "description": w.config.description,
            "project_roots": w.config.project_roots,
            "pages": items,
        })
        .to_string());
    }
    let mut out = String::new();
    let _ = writeln!(out, "Wiki: {} — {}", w.slug, w.config.description);
    let _ = writeln!(out, "Project roots: {}", w.config.project_roots.join(", "));
    let _ = writeln!(out, "{} pages, {} stubs needing content", rows.len(), stubs);
    let _ = writeln!(out, "\nPages:");
    for (id, desc, stub) in &rows {
        let marker = if *stub { "  [stub]" } else { "" };
        let _ = writeln!(out, "- {id} — {desc}{marker}");
    }
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
    tags: Vec<String>,
    body: Option<String>,
    json: bool,
) -> Result<String> {
    wiki::validate_id(id)?;
    if w.exists(id) {
        bail!("page '{id}' already exists — use `wookie write {id}` to replace its body");
    }
    let has_body = body.as_deref().map(|b| !b.trim().is_empty()).unwrap_or(false);
    let mut page = Page {
        id: id.to_string(),
        fm: crate::page::Frontmatter {
            title: title.unwrap_or_else(|| humanize(id)),
            description: if has_body { String::new() } else { format!("TODO: describe {id}") },
            tags,
            created: today(),
            updated: today(),
            status: if has_body { None } else { Some("stub".into()) },
        },
        body: body
            .filter(|b| !b.trim().is_empty())
            .unwrap_or_else(|| "TODO: fill in this page.".to_string()),
    };
    if has_body {
        page.fm.description = first_sentence(&page.summary());
    }
    w.save_page(&mut page, false)?;
    w.commit(&format!("wookie: new {id}"));

    if json {
        return Ok(serde_json::json!({"id": id, "stub": page.is_stub()}).to_string());
    }
    if page.is_stub() {
        Ok(format!(
            "Created stub '{id}'. Fill it by piping a body: wookie write {id} <<'EOF' ... EOF"
        ))
    } else {
        Ok(format!("Created page '{id}'."))
    }
}

fn first_sentence(text: &str) -> String {
    let flat = text.replace('\n', " ");
    match flat.find(". ") {
        Some(i) => flat[..=i].trim().to_string(),
        None => flat.trim().trim_end_matches('.').to_string() + ".",
    }
}

pub fn write(w: &Wiki, id: &str, body: &str, append: bool, json: bool) -> Result<String> {
    if body.trim().is_empty() {
        bail!("empty body — pipe page content via stdin (e.g. wookie write {id} <<'EOF' ... EOF)");
    }
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
    if page.fm.description.is_empty() || page.fm.description.starts_with("TODO") {
        page.fm.description = first_sentence(&page.summary());
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
            w.save_page(&mut other, false)?;
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
    for (target, sources) in &missing {
        let mut stub = Page {
            id: target.clone(),
            fm: crate::page::Frontmatter {
                title: humanize(target),
                description: format!("TODO: describe {target}"),
                tags: vec![],
                created: today(),
                updated: today(),
                status: Some("stub".into()),
            },
            body: format!(
                "TODO: fill in this page. It is linked from: {}.",
                sources.join(", ")
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
        return Ok(serde_json::json!({"created": created, "stubs": stubs}).to_string());
    }

    let mut out = String::new();
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

/// Open the wiki's pages/ folder as an Obsidian vault.
pub fn obsidian(w: &Wiki, print_only: bool, json: bool) -> Result<String> {
    let vault = w.pages_dir();
    // A .obsidian dir marks the folder as a vault so Obsidian opens it
    // without an "open folder as vault" detour. Its contents stay out of
    // wiki history via .gitignore.
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

    #[cfg(target_os = "macos")]
    let opener = "open";
    #[cfg(target_os = "linux")]
    let opener = "xdg-open";
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let opener = "open";

    let status = std::process::Command::new(opener).arg(&uri).status();
    match status {
        Ok(s) if s.success() => Ok(format!("Opened wiki '{}' in Obsidian: {}", w.slug, vault.display())),
        _ => bail!("could not launch Obsidian — open this URI manually: {uri}"),
    }
}

pub fn doctor(w: &Wiki, fix: bool, json: bool) -> Result<String> {
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
                w.save_page(&mut p2, true)?;
                fixed.push(format!("normalized frontmatter of '{}'", p.id));
            } else {
                issues.push(format!("missing/invalid frontmatter: '{}'", p.id));
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
    }
    if !fixed.is_empty() {
        w.commit("wookie: doctor --fix");
    }

    if json {
        return Ok(serde_json::json!({"issues": issues, "fixed": fixed}).to_string());
    }
    let mut out = String::new();
    if issues.is_empty() && fixed.is_empty() {
        return Ok(format!("Wiki '{}' is healthy: {} pages, no issues.", w.slug, pages.len()));
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
    Ok(out.trim_end().to_string())
}
