//! Page format: the hardcoded conventions wookie enforces.
//!
//! A page is markdown with a fixed frontmatter block, a standalone first
//! paragraph summary, and `[[wikilinks]]` between pages. wookie itself owns
//! the frontmatter (timestamps, stub status); agents only write bodies.

use anyhow::{bail, Result};
use std::path::{Component, Path};

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
pub enum PinLevel {
    /// Standing normative text. Bounded priming extracts only the concise
    /// agent-instruction section (or the summary when no such section exists).
    Instruction,
    /// Discovery context only: expose the summary, never the detailed body.
    Summary,
    /// Always highlight the page as metadata with an explicit read command,
    /// but never inline any of its content as standing text.
    Discoverable,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Frontmatter {
    pub title: String,
    pub description: String,
    pub tags: Vec<String>,
    pub created: String,
    pub updated: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Project-relative paths (files or dir prefixes) this page documents.
    /// `wookie ingest` uses these to map code changes to stale pages.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<String>,
    /// Pinned pages are always-on instructions: `wookie context` inlines
    /// their full bodies. Reserve for rules every session must follow.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub pin: bool,
    /// Effective pin behavior. Legacy `pin: true` maps to `instruction`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin_level: Option<PinLevel>,
    /// Alternate names (usually the human title) so Obsidian hover,
    /// search and [[Title]]-style links resolve.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    /// Frontmatter lines wookie doesn't own (e.g. Obsidian properties),
    /// preserved verbatim so human edits survive agent writes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra: Vec<String>,
}

/// Double-quote a YAML scalar. Descriptions routinely contain [[wikilinks]]
/// and colons, which are invalid as bare YAML and break Obsidian Properties.
fn yaml_quote(s: &str) -> String {
    let mut escaped = String::new();
    for ch in clean(s).chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            ch if ch.is_control() => {
                use std::fmt::Write;
                let _ = write!(escaped, "\\u{:04X}", u32::from(ch));
            }
            ch => escaped.push(ch),
        }
    }
    format!("\"{escaped}\"")
}

fn unquote(v: &str) -> String {
    let v = v.trim();
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        v[1..v.len() - 1]
            .replace("\\\"", "\"")
            .replace("\\\\", "\\")
    } else {
        v.to_string()
    }
}

fn parse_inline_list(value: &str) -> Vec<String> {
    let inner = value.trim().trim_start_matches('[').trim_end_matches(']');
    inner
        .split(',')
        .map(unquote)
        .filter(|t| !t.is_empty())
        .collect()
}

/// Frontmatter values are single-line by format; strip anything that would
/// break the block.
fn clean(s: &str) -> String {
    s.replace(['\n', '\r'], " ").trim().to_string()
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
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
    last.split(['-', '_'])
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

/// Derive a compact description from a standalone Markdown summary.
///
/// Closing emphasis markers belong to the sentence they wrap, so constructs
/// such as `**Lead.** More detail` stop after `Lead.` instead of swallowing
/// the entire paragraph.
pub fn first_sentence(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut sentence_end = None;
    for (index, character) in flat.char_indices() {
        if !matches!(character, '.' | '!' | '?') {
            continue;
        }
        let after = &flat[index + character.len_utf8()..];
        let after_emphasis = after.trim_start_matches(['*', '_', '`']);
        if after_emphasis.is_empty()
            || after_emphasis
                .chars()
                .next()
                .is_some_and(char::is_whitespace)
        {
            sentence_end = Some(index + character.len_utf8());
            break;
        }
    }
    let sentence = sentence_end.map_or(flat.as_str(), |end| &flat[..end]);
    let plain = sentence.trim().trim_matches(['*', '_', '`']).trim();
    if plain.ends_with(['.', '!', '?']) {
        plain.to_string()
    } else {
        format!("{}.", plain.trim_end_matches('.'))
    }
}

/// A wikilink found outside Markdown code.
///
/// `target_start..target_end` identifies only the meaningful target text. It
/// deliberately excludes surrounding whitespace and optional backtick markup
/// so a rewrite can change the destination without disturbing how the link is
/// displayed.
#[derive(Debug, PartialEq, Eq)]
struct Wikilink {
    end: usize,
    target_start: usize,
    target_end: usize,
    target: String,
}

fn byte_run(bytes: &[u8], start: usize, byte: u8) -> usize {
    bytes[start..].iter().take_while(|&&b| b == byte).count()
}

fn update_line_start(text: &str, from: usize, to: usize, current: usize) -> usize {
    text[from..to]
        .rfind('\n')
        .map_or(current, |newline| from + newline + 1)
}

/// Return a fenced-code opener at `pos`. CommonMark permits up to three spaces
/// of indentation and fences of three or more backticks or tildes.
fn fence_opener(bytes: &[u8], line_start: usize, pos: usize) -> Option<(u8, usize)> {
    let marker = *bytes.get(pos)?;
    if marker != b'`' && marker != b'~' {
        return None;
    }
    let indent = &bytes[line_start..pos];
    if indent.len() > 3 || indent.iter().any(|&b| b != b' ') {
        return None;
    }
    let len = byte_run(bytes, pos, marker);
    (len >= 3).then_some((marker, len))
}

/// Skip an entire fenced code block, returning the end of the closing line or
/// EOF when the fence is unclosed.
fn skip_fenced_code(text: &str, opener: usize, marker: u8, opener_len: usize) -> usize {
    let bytes = text.as_bytes();
    let Some(opening_newline) = bytes[opener..].iter().position(|&b| b == b'\n') else {
        return bytes.len();
    };
    let mut line_start = opener + opening_newline + 1;

    while line_start < bytes.len() {
        let line_end = bytes[line_start..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(bytes.len(), |offset| line_start + offset);
        let mut marker_start = line_start;
        while marker_start < line_end
            && marker_start - line_start < 3
            && bytes[marker_start] == b' '
        {
            marker_start += 1;
        }

        if marker_start < line_end && bytes[marker_start] == marker {
            let close_len = byte_run(bytes, marker_start, marker).min(line_end - marker_start);
            let rest = marker_start + close_len;
            if close_len >= opener_len
                && bytes[rest..line_end]
                    .iter()
                    .all(|&b| b == b' ' || b == b'\t')
            {
                return (line_end + usize::from(line_end < bytes.len())).min(bytes.len());
            }
        }

        line_start = line_end + usize::from(line_end < bytes.len());
    }

    bytes.len()
}

/// Find the end of a Markdown inline-code span. Backtick runs close only runs
/// of the same length; an unmatched opener is ordinary text.
fn inline_code_end(bytes: &[u8], opener: usize, opener_len: usize) -> Option<usize> {
    let mut pos = opener + opener_len;
    while pos < bytes.len() {
        if bytes[pos] == b'`' {
            let len = byte_run(bytes, pos, b'`');
            if len == opener_len {
                return Some(pos + len);
            }
            pos += len;
        } else {
            pos += 1;
        }
    }
    None
}

/// Locate the page-id portion of a raw wikilink target. A target wrapped
/// wholly in Markdown code formatting (`` [[`page`]] ``) resolves to `page`,
/// while the formatting itself remains outside the replacement range.
fn target_parts(raw: &str, absolute_start: usize) -> Option<(usize, usize, String)> {
    let leading = raw.len() - raw.trim_start().len();
    let trailing_end = raw.trim_end().len();
    if leading >= trailing_end {
        return None;
    }

    let trimmed = &raw[leading..trailing_end];
    let ticks = trimmed
        .as_bytes()
        .iter()
        .take_while(|&&b| b == b'`')
        .count();
    let trailing_ticks = trimmed
        .as_bytes()
        .iter()
        .rev()
        .take_while(|&&b| b == b'`')
        .count();
    let (content_start, content_end) =
        if ticks > 0 && trimmed.len() >= ticks * 2 && trailing_ticks == ticks {
            let inner = &trimmed[ticks..trimmed.len() - ticks];
            // A run equal to the delimiter would close the code span early, so it
            // cannot be part of an entirely code-formatted target.
            let delimiter = "`".repeat(ticks);
            if inner.contains(&delimiter) {
                (leading, trailing_end)
            } else {
                let inner_leading = inner.len() - inner.trim_start().len();
                let inner_end = inner.trim_end().len();
                (leading + ticks + inner_leading, leading + ticks + inner_end)
            }
        } else {
            (leading, trailing_end)
        };

    if content_start >= content_end {
        return None;
    }
    let target = raw[content_start..content_end].to_string();
    Some((
        absolute_start + content_start,
        absolute_start + content_end,
        target,
    ))
}

fn parse_wikilink(text: &str, start: usize) -> Option<Wikilink> {
    let bytes = text.as_bytes();
    let mut pos = start + 2;
    let mut separator = None;

    while pos + 1 < bytes.len() {
        if bytes[pos] == b'[' && bytes[pos + 1] == b'[' {
            return None;
        }
        if bytes[pos] == b'|' && separator.is_none() {
            separator = Some(pos);
        }
        if bytes[pos] == b']' && bytes[pos + 1] == b']' {
            let target_end = separator.unwrap_or(pos);
            let raw_target = &text[start + 2..target_end];
            let (target_start, target_end, target) = target_parts(raw_target, start + 2)?;
            return Some(Wikilink {
                end: pos + 2,
                target_start,
                target_end,
                target,
            });
        }
        pos += 1;
    }
    None
}

/// Parse wikilinks with just enough Markdown awareness to make graph reads and
/// link rewrites agree. Code examples are never graph edges. Backticks inside
/// a wikilink, however, are display markup and do not hide the link itself.
fn wikilinks(text: &str) -> Vec<Wikilink> {
    let bytes = text.as_bytes();
    let mut links = Vec::new();
    let mut pos = 0;
    let mut line_start = 0;

    while pos < bytes.len() {
        if bytes[pos] == b'\n' {
            pos += 1;
            line_start = pos;
            continue;
        }

        if let Some((marker, len)) = fence_opener(bytes, line_start, pos) {
            let end = skip_fenced_code(text, pos, marker, len);
            line_start = update_line_start(text, pos, end, line_start);
            pos = end;
            continue;
        }

        if bytes[pos] == b'`' {
            let len = byte_run(bytes, pos, b'`');
            if let Some(end) = inline_code_end(bytes, pos, len) {
                line_start = update_line_start(text, pos, end, line_start);
                pos = end;
                continue;
            }
            pos += len;
            continue;
        }

        if bytes[pos] == b'[' && bytes.get(pos + 1) == Some(&b'[') {
            if let Some(link) = parse_wikilink(text, pos) {
                let end = link.end;
                line_start = update_line_start(text, pos, end, line_start);
                links.push(link);
                pos = end;
                continue;
            }
        }

        pos += 1;
    }

    links
}

impl Page {
    /// Validate tool-owned metadata before it reaches YAML or an OS path API.
    /// Markdown bodies intentionally remain unrestricted apart from UTF-8.
    pub fn validate_frontmatter(&self) -> Result<()> {
        let scalar_fields = [
            ("title", self.fm.title.as_str()),
            ("description", self.fm.description.as_str()),
            ("created", self.fm.created.as_str()),
            ("updated", self.fm.updated.as_str()),
        ];
        for (field, value) in scalar_fields {
            if value.chars().any(char::is_control) {
                bail!("page '{}' has a control character in {field}", self.id);
            }
        }
        if self
            .fm
            .status
            .as_deref()
            .is_some_and(|value| value.chars().any(char::is_control))
        {
            bail!("page '{}' has a control character in status", self.id);
        }
        for (field, values) in [
            ("tag", &self.fm.tags),
            ("alias", &self.fm.aliases),
            ("source", &self.fm.sources),
        ] {
            for value in values {
                if value.chars().any(char::is_control) {
                    bail!("page '{}' has a control character in {field}", self.id);
                }
            }
        }
        for source in &self.fm.sources {
            let path = Path::new(source);
            if source.is_empty()
                || source.trim() != source
                || source.contains('\\')
                || path.is_absolute()
                || path
                    .components()
                    .any(|component| !matches!(component, Component::Normal(_)))
            {
                bail!(
                    "page '{}' has an invalid project-relative source path: '{source}'",
                    self.id
                );
            }
        }
        Ok(())
    }

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
                        if let (Some(key), Some(item)) =
                            (&open_list, line.trim().strip_prefix("- "))
                        {
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
                        "pin" => match unquote(value).as_str() {
                            "true" | "instruction" => {
                                fm.pin = true;
                                fm.pin_level = Some(PinLevel::Instruction);
                            }
                            "summary" => {
                                fm.pin = true;
                                fm.pin_level = Some(PinLevel::Summary);
                            }
                            "discoverable" => {
                                fm.pin = true;
                                fm.pin_level = Some(PinLevel::Discoverable);
                            }
                            _ => {
                                fm.pin = false;
                                fm.pin_level = None;
                            }
                        },
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
            items
                .iter()
                .map(|t| yaml_quote(t))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let mut s = String::from("---\n");
        s.push_str(&format!("title: {}\n", yaml_quote(&self.fm.title)));
        s.push_str(&format!(
            "description: {}\n",
            yaml_quote(&self.fm.description)
        ));
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
            match self.fm.pin_level.unwrap_or(PinLevel::Instruction) {
                PinLevel::Instruction => s.push_str("pin: true\n"),
                PinLevel::Summary => s.push_str("pin: summary\n"),
                PinLevel::Discoverable => s.push_str("pin: discoverable\n"),
            }
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
        let mut seen = std::collections::HashSet::new();
        wikilinks(&self.body)
            .into_iter()
            .map(|link| link.target)
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

    pub fn pin_level(&self) -> Option<PinLevel> {
        self.fm
            .pin
            .then_some(self.fm.pin_level.unwrap_or(PinLevel::Instruction))
    }

    /// Why a standing Instruction/Summary pin cannot safely be guaranteed at
    /// task start. Discoverable pins are metadata-only and intentionally have
    /// no standing text requirement.
    pub fn standing_text_issue(&self) -> Option<&'static str> {
        if !matches!(
            self.pin_level(),
            Some(PinLevel::Instruction | PinLevel::Summary)
        ) {
            return None;
        }
        if self.is_stub() {
            return Some("is a stub");
        }
        let text = self.pinned_text();
        let words = text
            .split(|character: char| !character.is_alphanumeric())
            .filter(|word| !word.is_empty())
            .map(str::to_ascii_lowercase)
            .collect::<Vec<_>>();
        let normalized = text
            .trim()
            .trim_matches(|character: char| !character.is_alphanumeric())
            .to_ascii_lowercase();
        let marker_only = words.len() == 1
            && matches!(
                words.first().map(String::as_str),
                Some("todo" | "tbd" | "placeholder")
            );
        let scaffold = [
            "todo: define",
            "todo: describe",
            "todo: add the actual",
            "tbd: define",
            "tbd: describe",
        ]
        .iter()
        .any(|prefix| normalized.starts_with(prefix));
        if words.is_empty() || marker_only || scaffold {
            return Some("has empty or placeholder standing text");
        }
        None
    }

    /// Compact standing text for task-start priming. Full detail remains
    /// available through `read` and the explicit exhaustive `context` command.
    pub fn pinned_text(&self) -> String {
        match self.pin_level() {
            Some(PinLevel::Summary) => self.summary(),
            Some(PinLevel::Instruction) => {
                let mut collecting = false;
                let mut lines = Vec::new();
                for line in self.body.lines() {
                    if line.trim().eq_ignore_ascii_case("## Agent instructions") {
                        collecting = true;
                        continue;
                    }
                    if collecting && line.starts_with("## ") {
                        break;
                    }
                    if collecting {
                        lines.push(line);
                    }
                }
                let extracted = lines.join("\n").trim().to_string();
                if extracted.is_empty() {
                    self.summary()
                } else {
                    extracted
                }
            }
            Some(PinLevel::Discoverable) => String::new(),
            None => String::new(),
        }
    }
}

/// Rewrite `[[old]]` and `[[old|...]]` links to point at `new`.
/// Returns the rewritten text and whether anything changed.
pub fn rewrite_links(text: &str, old: &str, new: &str) -> (String, bool) {
    let links = wikilinks(text);
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    let mut changed = false;
    for link in links.iter().filter(|link| link.target == old) {
        out.push_str(&text[cursor..link.target_start]);
        out.push_str(new);
        cursor = link.target_end;
        changed = true;
    }
    out.push_str(&text[cursor..]);
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
                pin_level: Some(PinLevel::Instruction),
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
    fn discoverable_pin_roundtrips_without_standing_text() {
        let page = Page::parse(
            "reference",
            "---\ntitle: Reference\npin: discoverable\n---\n\n**Reference.** Read on demand.",
        );
        assert_eq!(page.pin_level(), Some(PinLevel::Discoverable));
        assert!(page.pinned_text().is_empty());
        let parsed = Page::parse("reference", &page.render());
        assert_eq!(parsed.pin_level(), Some(PinLevel::Discoverable));
    }

    #[test]
    fn todo_policy_is_not_mistaken_for_placeholder_text() {
        let mut page = Page::parse(
            "todo-policy",
            "**TODO comments require issue links.** Enforce this during review.",
        );
        page.fm.pin = true;
        page.fm.pin_level = Some(PinLevel::Instruction);
        assert_eq!(page.standing_text_issue(), None);
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
    fn code_formatting_inside_wikilinks_does_not_hide_the_target() {
        let p = Page::parse(
            "x",
            "See [[plain]], [[with-alias|an alias]], [[code-alias|`run()`]], and [[`code-target`]]. Also [[`plain`|duplicate]].",
        );
        assert_eq!(
            p.links(),
            vec!["plain", "with-alias", "code-alias", "code-target"]
        );
    }

    #[test]
    fn target_formatting_and_whitespace_are_normalized() {
        let p = Page::parse(
            "x",
            "[[ spaced-target |label]] [[ `formatted-target` |label]] [[``double-tick``]]",
        );
        assert_eq!(
            p.links(),
            vec!["spaced-target", "formatted-target", "double-tick"]
        );
    }

    #[test]
    fn summary_skips_headings() {
        let p = Page::parse(
            "x",
            "---\ntitle: X\n---\n\n# Heading\n\nThe real summary.\n\nMore.",
        );
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
        assert!(
            rendered.contains(r#"description: "Keeps the [[hyperdrive]] running: fast""#),
            "got: {rendered}"
        );
        let parsed = Page::parse("x", &rendered);
        assert_eq!(parsed.fm.title, r#"The "Scheduler""#);
        assert_eq!(
            parsed.fm.description,
            "Keeps the [[hyperdrive]] running: fast"
        );
    }

    #[test]
    fn block_style_lists_parse_into_known_fields() {
        let content =
            "---\ntitle: X\naliases:\n  - Other Name\n  - X()\ntags:\n  - core\n---\n\nBody.";
        let p = Page::parse("x", content);
        assert_eq!(p.fm.aliases, vec!["Other Name", "X()"]);
        assert_eq!(p.fm.tags, vec!["core"]);
        assert!(p.fm.extra.is_empty(), "got extra: {:?}", p.fm.extra);
    }

    #[test]
    fn unknown_frontmatter_lines_survive_roundtrip() {
        let content = "---\ntitle: X\ndescription: d\ncustom-prop:\n  - other-name\ncssclasses: [wide]\n---\n\nBody.";
        let p = Page::parse("x", content);
        assert_eq!(
            p.fm.extra,
            vec!["custom-prop:", "  - other-name", "cssclasses: [wide]"]
        );
        let rendered = p.render();
        assert!(
            rendered.contains("custom-prop:\n  - other-name"),
            "got: {rendered}"
        );
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
    fn control_characters_in_owned_metadata_are_rejected_and_safely_escaped() {
        let mut page = Page::parse("unsafe", "Summary.");
        page.fm.title = "bad\0title".into();
        assert!(page.validate_frontmatter().is_err());
        let rendered = page.render();
        assert!(!rendered.contains('\0'));
        assert!(rendered.contains(r#"bad\u0000title"#), "{rendered}");

        page.fm.title = "safe".into();
        page.fm.tags = vec!["bad\ttag".into()];
        assert!(page.validate_frontmatter().is_err());
        page.fm.tags.clear();
        page.fm.sources = vec!["../outside".into()];
        assert!(page.validate_frontmatter().is_err());
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
    fn links_in_markdown_code_are_ignored_across_fence_styles() {
        let p = Page::parse(
            "x",
            concat!(
                "Real [[one]].\n",
                "Inline `[[inline-one]]` and ``[[inline-two|`alias`]]``.\n",
                "   ```markdown\n[[backtick-fence]]\n   ```\n",
                "~~~md\n[[tilde-fence|alias]]\n~~~~\n",
                "After [[two|`code alias`]]."
            ),
        );
        assert_eq!(p.links(), vec!["one", "two"]);
    }

    #[test]
    fn unmatched_backticks_do_not_hide_later_links() {
        let p = Page::parse("x", "An unmatched ` marker and a real [[target]].");
        assert_eq!(p.links(), vec!["target"]);
    }

    #[test]
    fn rewrites_links() {
        let (out, changed) =
            rewrite_links("a [[old]] b [[old|text]] c [[older]]", "old", "new/place");
        assert!(changed);
        assert_eq!(out, "a [[new/place]] b [[new/place|text]] c [[older]]");
    }

    #[test]
    fn rewrites_formatted_targets_and_preserves_alias_markup() {
        let text = concat!(
            "[[old|plain alias]] ",
            "[[old|`code alias`]] ",
            "[[`old`]] ",
            "[[ `old` | **rich alias** ]]"
        );
        let (out, changed) = rewrite_links(text, "old", "new/place");
        assert!(changed);
        assert_eq!(
            out,
            concat!(
                "[[new/place|plain alias]] ",
                "[[new/place|`code alias`]] ",
                "[[`new/place`]] ",
                "[[ `new/place` | **rich alias** ]]"
            )
        );
    }

    #[test]
    fn rewrites_links_without_touching_code_examples() {
        let text = concat!(
            "real [[old]]; inline `[[old]]`; double ``[[old|`alias`]]``; ",
            "fenced:\n```md\n[[old|example]]\n```\n",
            "~~~\n[[`old`]]\n~~~"
        );
        let (out, changed) = rewrite_links(text, "old", "new/place");
        assert!(changed);
        assert_eq!(
            out,
            concat!(
                "real [[new/place]]; inline `[[old]]`; double ``[[old|`alias`]]``; ",
                "fenced:\n```md\n[[old|example]]\n```\n",
                "~~~\n[[`old`]]\n~~~"
            )
        );
    }

    #[test]
    fn rewrite_reports_unchanged_when_only_code_examples_match() {
        let text = "`[[old]]`\n\n```\n[[old|example]]\n```";
        let (out, changed) = rewrite_links(text, "old", "new");
        assert!(!changed);
        assert_eq!(out, text);
    }

    #[test]
    fn humanizes_ids() {
        assert_eq!(humanize("internals/retry-policy"), "Retry Policy");
        assert_eq!(humanize("index"), "Index");
    }
}
