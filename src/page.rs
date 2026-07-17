//! Page format: the hardcoded conventions wookie enforces.
//!
//! A page is markdown with a fixed frontmatter block, a standalone first
//! paragraph summary, and `[[wikilinks]]` between pages. wookie itself owns
//! the frontmatter (timestamps, stub status); agents only write bodies.

use regex::Regex;
use std::sync::OnceLock;

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Frontmatter {
    pub title: String,
    pub description: String,
    pub tags: Vec<String>,
    pub created: String,
    pub updated: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Project-relative paths (files or dir prefixes) this page documents.
    /// `wookie ingest` uses these to map code changes to stale pages.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<String>,
    /// Pinned pages are always-on instructions: `wookie context` inlines
    /// their full bodies. Reserve for rules every session must follow.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub pin: bool,
    /// Alternate names (usually the human title) so Obsidian hover,
    /// search and [[Title]]-style links resolve.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    /// Frontmatter lines wookie doesn't own (e.g. Obsidian properties),
    /// preserved verbatim so human edits survive agent writes.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub extra: Vec<String>,
}

/// Double-quote a YAML scalar. Descriptions routinely contain [[wikilinks]]
/// and colons, which are invalid as bare YAML and break Obsidian Properties.
fn yaml_quote(s: &str) -> String {
    format!("\"{}\"", clean(s).replace('\\', "\\\\").replace('"', "\\\""))
}

fn unquote(v: &str) -> String {
    let v = v.trim();
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        v[1..v.len() - 1].replace("\\\"", "\"").replace("\\\\", "\\")
    } else {
        v.to_string()
    }
}

fn parse_inline_list(value: &str) -> Vec<String> {
    let inner = value.trim().trim_start_matches('[').trim_end_matches(']');
    inner
        .split(',')
        .map(|t| unquote(t))
        .filter(|t| !t.is_empty())
        .collect()
}

/// Frontmatter values are single-line by format; strip anything that would
/// break the block.
fn clean(s: &str) -> String {
    s.replace(['\n', '\r'], " ").trim().to_string()
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Page {
    pub id: String,
    pub fm: Frontmatter,
    pub body: String,
}

pub fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// Turn a page id's last segment into a human title: `retry-policy` -> `Retry Policy`.
pub fn humanize(id: &str) -> String {
    let last = id.rsplit('/').next().unwrap_or(id);
    last.split(|c| c == '-' || c == '_')
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn link_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\[\[([^\[\]|]+)(?:\|([^\[\]]+))?\]\]").unwrap())
}

fn code_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?s)```.*?```|`[^`\n]*`").unwrap())
}

impl Page {
    /// Parse page content. Lenient: a file without valid frontmatter still
    /// loads (doctor flags it via the empty `created` field).
    pub fn parse(id: &str, content: &str) -> Page {
        let mut fm = Frontmatter {
            title: humanize(id),
            ..Default::default()
        };
        let mut body = content.to_string();

        if let Some(rest) = content.strip_prefix("---\n") {
            if let Some(end) = rest.find("\n---") {
                let block = &rest[..end];
                let after = &rest[end + 4..];
                // Which known list key a block-style `- item` line belongs to
                // (Obsidian writes lists in block form).
                #[derive(PartialEq)]
                enum ListKey {
                    Tags,
                    Sources,
                    Aliases,
                }
                let mut open_list: Option<ListKey> = None;
                for line in block.lines() {
                    if line.starts_with([' ', '\t']) {
                        if let (Some(key), Some(item)) = (&open_list, line.trim().strip_prefix("- ")) {
                            let item = unquote(item);
                            match key {
                                ListKey::Tags => fm.tags.push(item),
                                ListKey::Sources => fm.sources.push(item),
                                ListKey::Aliases => fm.aliases.push(item),
                            }
                            continue;
                        }
                        if !line.trim().is_empty() {
                            fm.extra.push(line.to_string());
                        }
                        continue;
                    }
                    open_list = None;
                    let Some((key, value)) = line.split_once(':') else {
                        if !line.trim().is_empty() {
                            fm.extra.push(line.to_string());
                        }
                        continue;
                    };
                    let value = value.trim();
                    match key.trim() {
                        "title" => fm.title = unquote(value),
                        "description" => fm.description = unquote(value),
                        "created" => fm.created = unquote(value),
                        "updated" => fm.updated = unquote(value),
                        "status" => {
                            if !value.is_empty() {
                                fm.status = Some(unquote(value));
                            }
                        }
                        "pin" => fm.pin = value == "true",
                        "tags" | "sources" | "aliases" => {
                            let list_key = match key.trim() {
                                "tags" => ListKey::Tags,
                                "sources" => ListKey::Sources,
                                _ => ListKey::Aliases,
                            };
                            if value.is_empty() {
                                open_list = Some(list_key);
                            } else {
                                let items = parse_inline_list(value);
                                match list_key {
                                    ListKey::Tags => fm.tags = items,
                                    ListKey::Sources => fm.sources = items,
                                    ListKey::Aliases => fm.aliases = items,
                                }
                            }
                        }
                        _ => fm.extra.push(line.to_string()),
                    }
                }
                body = after.trim_start_matches('\n').to_string();
            }
        }

        Page {
            id: id.to_string(),
            fm,
            body,
        }
    }

    /// Serialize back to the canonical on-disk format: valid YAML, so
    /// Obsidian renders the Properties panel instead of raw text.
    pub fn render(&self) -> String {
        let quoted_list = |items: &[String]| {
            items.iter().map(|t| yaml_quote(t)).collect::<Vec<_>>().join(", ")
        };
        let mut s = String::from("---\n");
        s.push_str(&format!("title: {}\n", yaml_quote(&self.fm.title)));
        s.push_str(&format!("description: {}\n", yaml_quote(&self.fm.description)));
        if !self.fm.aliases.is_empty() {
            s.push_str(&format!("aliases: [{}]\n", quoted_list(&self.fm.aliases)));
        }
        s.push_str(&format!("tags: [{}]\n", quoted_list(&self.fm.tags)));
        s.push_str(&format!("created: {}\n", clean(&self.fm.created)));
        s.push_str(&format!("updated: {}\n", clean(&self.fm.updated)));
        if let Some(status) = &self.fm.status {
            s.push_str(&format!("status: {}\n", yaml_quote(status)));
        }
        if !self.fm.sources.is_empty() {
            s.push_str(&format!("sources: [{}]\n", quoted_list(&self.fm.sources)));
        }
        if self.fm.pin {
            s.push_str("pin: true\n");
        }
        for line in &self.fm.extra {
            s.push_str(line);
            s.push('\n');
        }
        s.push_str("---\n\n");
        s.push_str(self.body.trim_end());
        s.push('\n');
        s
    }

    /// Outgoing wikilink targets, deduped, order preserved. Links inside
    /// code fences and inline code spans don't count: that is how pages
    /// document link syntax without creating phantom links.
    pub fn links(&self) -> Vec<String> {
        let body = code_re().replace_all(&self.body, "");
        let mut seen = std::collections::HashSet::new();
        link_re()
            .captures_iter(&body)
            .map(|c| c[1].trim().to_string())
            .filter(|t| seen.insert(t.clone()))
            .collect()
    }

    /// First body paragraph that is not a heading: the standalone summary.
    pub fn summary(&self) -> String {
        self.body
            .split("\n\n")
            .map(str::trim)
            .find(|p| !p.is_empty() && !p.starts_with('#'))
            .unwrap_or("")
            .to_string()
    }

    pub fn is_stub(&self) -> bool {
        self.fm.status.as_deref() == Some("stub")
    }
}

/// Rewrite `[[old]]` and `[[old|...]]` links to point at `new`.
/// Returns the rewritten text and whether anything changed.
pub fn rewrite_links(text: &str, old: &str, new: &str) -> (String, bool) {
    let plain_old = format!("[[{old}]]");
    let plain_new = format!("[[{new}]]");
    let pipe_old = format!("[[{old}|");
    let pipe_new = format!("[[{new}|");
    let changed = text.contains(&plain_old) || text.contains(&pipe_old);
    let out = text.replace(&plain_old, &plain_new).replace(&pipe_old, &pipe_new);
    (out, changed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_roundtrip() {
        let p = Page {
            id: "internals/retry-policy".into(),
            fm: Frontmatter {
                title: "Retry Policy".into(),
                description: "How retries work".into(),
                tags: vec!["core".into(), "scheduler".into()],
                created: "2026-07-17".into(),
                updated: "2026-07-17".into(),
                status: Some("stub".into()),
                sources: vec!["src/retry.rs".into(), "src/backoff/".into()],
                pin: true,
                aliases: vec!["Retry Policy".into()],
                extra: vec![],
            },
            body: "Summary paragraph.\n\nMore detail with a [[scheduler]] link.".into(),
        };
        let rendered = p.render();
        let parsed = Page::parse(&p.id, &rendered);
        assert_eq!(parsed.fm.title, "Retry Policy");
        assert_eq!(parsed.fm.description, "How retries work");
        assert_eq!(parsed.fm.tags, vec!["core", "scheduler"]);
        assert_eq!(parsed.fm.status.as_deref(), Some("stub"));
        assert_eq!(parsed.fm.sources, vec!["src/retry.rs", "src/backoff/"]);
        assert!(parsed.fm.pin);
        assert_eq!(parsed.body.trim(), p.body.trim());
    }

    #[test]
    fn parse_without_frontmatter_is_lenient() {
        let p = Page::parse("some-page", "just a body");
        assert_eq!(p.fm.title, "Some Page");
        assert!(p.fm.created.is_empty());
        assert_eq!(p.body, "just a body");
    }

    #[test]
    fn extracts_links_with_and_without_display_text() {
        let p = Page::parse(
            "x",
            "---\ntitle: X\n---\n\nSee [[scheduler]] and [[internals/retry-policy|retries]]. Also [[scheduler]] again.",
        );
        assert_eq!(p.links(), vec!["scheduler", "internals/retry-policy"]);
    }

    #[test]
    fn summary_skips_headings() {
        let p = Page::parse("x", "---\ntitle: X\n---\n\n# Heading\n\nThe real summary.\n\nMore.");
        assert_eq!(p.summary(), "The real summary.");
    }

    #[test]
    fn frontmatter_is_valid_yaml_with_links_and_colons() {
        let p = Page {
            id: "x".into(),
            fm: Frontmatter {
                title: "The \"Scheduler\"".into(),
                description: "Keeps the [[hyperdrive]] running: fast".into(),
                ..Default::default()
            },
            body: "Body.".into(),
        };
        let rendered = p.render();
        assert!(rendered.contains(r#"description: "Keeps the [[hyperdrive]] running: fast""#), "got: {rendered}");
        let parsed = Page::parse("x", &rendered);
        assert_eq!(parsed.fm.title, r#"The "Scheduler""#);
        assert_eq!(parsed.fm.description, "Keeps the [[hyperdrive]] running: fast");
    }

    #[test]
    fn block_style_lists_parse_into_known_fields() {
        let content = "---\ntitle: X\naliases:\n  - Other Name\n  - X()\ntags:\n  - core\n---\n\nBody.";
        let p = Page::parse("x", content);
        assert_eq!(p.fm.aliases, vec!["Other Name", "X()"]);
        assert_eq!(p.fm.tags, vec!["core"]);
        assert!(p.fm.extra.is_empty(), "got extra: {:?}", p.fm.extra);
    }

    #[test]
    fn unknown_frontmatter_lines_survive_roundtrip() {
        let content = "---\ntitle: X\ndescription: d\ncustom-prop:\n  - other-name\ncssclasses: [wide]\n---\n\nBody.";
        let p = Page::parse("x", content);
        assert_eq!(p.fm.extra, vec!["custom-prop:", "  - other-name", "cssclasses: [wide]"]);
        let rendered = p.render();
        assert!(rendered.contains("custom-prop:\n  - other-name"), "got: {rendered}");
        assert!(rendered.contains("cssclasses: [wide]"), "got: {rendered}");
        let p2 = Page::parse("x", &rendered);
        assert_eq!(p2.fm.extra, p.fm.extra);
    }

    #[test]
    fn newlines_in_values_cannot_corrupt_frontmatter() {
        let p = Page {
            id: "x".into(),
            fm: Frontmatter {
                title: "evil\ntitle: injected".into(),
                ..Default::default()
            },
            body: "Body.".into(),
        };
        let parsed = Page::parse("x", &p.render());
        assert_eq!(parsed.fm.title, "evil title: injected");
    }

    #[test]
    fn links_in_code_spans_are_ignored() {
        let p = Page::parse(
            "x",
            "---\ntitle: X\n---\n\nReal [[target]]. Syntax demo: `[[not-a-link]]`.\n\n```\n[[also-not]]\n```",
        );
        assert_eq!(p.links(), vec!["target"]);
    }

    #[test]
    fn rewrites_links() {
        let (out, changed) = rewrite_links("a [[old]] b [[old|text]] c [[older]]", "old", "new/place");
        assert!(changed);
        assert_eq!(out, "a [[new/place]] b [[new/place|text]] c [[older]]");
    }

    #[test]
    fn humanizes_ids() {
        assert_eq!(humanize("internals/retry-policy"), "Retry Policy");
        assert_eq!(humanize("index"), "Index");
    }
}
