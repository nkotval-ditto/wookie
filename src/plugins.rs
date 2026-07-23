//! Plugin installers: teach Claude Code and Codex how to use wookie. Both are
//! generated from one embedded template so the guidance never drifts.

use crate::config::user_home;
use anyhow::{bail, Context, Result};
use std::fs;

const GUIDANCE: &str = include_str!("../templates/guidance.md");
const CODEX_START: &str = "<!-- wookie:start -->";
const CODEX_END: &str = "<!-- wookie:end -->";
const INTEGRATION_VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_INTEGRATION_FILE_BYTES: usize = 4 * 1024 * 1024;

fn read_integration_file(path: &std::path::Path) -> Result<Option<String>> {
    crate::config::read_optional_bounded_regular_utf8(
        path,
        MAX_INTEGRATION_FILE_BYTES,
        "agent integration file",
    )
}

#[derive(Clone, Copy, Debug, clap::ValueEnum, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Target {
    Claude,
    Codex,
}

pub fn install(target: Target, json: bool) -> Result<String> {
    match target {
        Target::Claude => install_claude(json),
        Target::Codex => install_codex(json),
    }
}

#[derive(serde::Serialize)]
struct IntegrationStatus {
    target: Target,
    state: &'static str,
    path: String,
    version: &'static str,
}

pub fn status(target: Option<Target>, strict: bool, json: bool) -> Result<String> {
    let targets = target.map_or_else(
        || vec![Target::Claude, Target::Codex],
        |target| vec![target],
    );
    let mut lines = vec![];
    for target in targets {
        let (path, current) = match target {
            Target::Claude => {
                let path = user_home()?.join(".claude/skills/wookie/SKILL.md");
                let current = read_integration_file(&path)?
                    .is_some_and(|contents| contents == claude_content());
                (path, current)
            }
            Target::Codex => {
                let path = user_home()?.join(".codex/AGENTS.md");
                let current = read_integration_file(&path)?
                    .and_then(|contents| managed_codex_block(&contents))
                    .is_some_and(|block| block == codex_block());
                (path, current)
            }
        };
        let state = if current {
            "current"
        } else if path.exists() {
            "stale"
        } else {
            "missing"
        };
        lines.push(IntegrationStatus {
            target,
            state,
            path: path.display().to_string(),
            version: INTEGRATION_VERSION,
        });
    }
    if strict && lines.iter().any(|status| status.state != "current") {
        bail!(
            "one or more integrations are stale or missing; refresh with `wookie plugin install <target>`"
        );
    }
    if json {
        Ok(serde_json::to_string(&lines)?)
    } else {
        Ok(lines
            .iter()
            .map(|status| {
                let name = match status.target {
                    Target::Claude => "claude",
                    Target::Codex => "codex",
                };
                format!("{name}: {} ({})", status.state, status.path)
            })
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

fn claude_content() -> String {
    format!(
        "---\nname: wookie\ndescription: Read and grow the project's wookie wiki and coordinate concurrent agent sessions. Use when starting work on a project, when asked to look up or document project knowledge, after learning something durable, or when polling/publishing cross-session notifications. Triggers on \"wiki\", \"wookie\", \"document this\", \"what do we know about\", \"notify other agents\".\nmetadata:\n  version: \"{INTEGRATION_VERSION}\"\n---\n\n{GUIDANCE}"
    )
}

fn codex_block() -> String {
    format!("{CODEX_START}\n<!-- wookie:version={INTEGRATION_VERSION} -->\n{GUIDANCE}\n{CODEX_END}")
}

fn managed_codex_block(contents: &str) -> Option<String> {
    let start = contents.find(CODEX_START)?;
    let relative_end = contents[start..].find(CODEX_END)?;
    let end = start + relative_end + CODEX_END.len();
    Some(contents[start..end].to_string())
}

/// Claude Code: a skill at ~/.claude/skills/wookie/SKILL.md.
fn install_claude(json: bool) -> Result<String> {
    let dir = user_home()?.join(".claude/skills/wookie");
    fs::create_dir_all(&dir)?;
    let path = dir.join("SKILL.md");
    let content = claude_content();
    crate::wiki::atomic_write(&path, content)
        .with_context(|| format!("writing {}", path.display()))?;
    if json {
        Ok(
            serde_json::json!({"target": "claude", "path": path, "version": INTEGRATION_VERSION})
                .to_string(),
        )
    } else {
        Ok(format!("Installed Claude Code skill: {}", path.display()))
    }
}

/// Codex: a managed block in ~/.codex/AGENTS.md, idempotent on reinstall.
fn install_codex(json: bool) -> Result<String> {
    let dir = user_home()?.join(".codex");
    fs::create_dir_all(&dir)?;
    let path = dir.join("AGENTS.md");
    let block = codex_block();
    let existing = read_integration_file(&path)?.unwrap_or_default();

    let updated = replace_codex_block(&existing, &block)?;
    crate::wiki::atomic_write(&path, updated)
        .with_context(|| format!("writing {}", path.display()))?;
    if json {
        Ok(
            serde_json::json!({"target": "codex", "path": path, "version": INTEGRATION_VERSION})
                .to_string(),
        )
    } else {
        Ok(format!(
            "Installed Codex guidance block: {}",
            path.display()
        ))
    }
}

fn replace_codex_block(existing: &str, block: &str) -> Result<String> {
    match (existing.find(CODEX_START), existing.find(CODEX_END)) {
        (Some(start), Some(end)) if end > start => {
            let after = &existing[end + CODEX_END.len()..];
            Ok(format!("{}{}{}", &existing[..start], block, after))
        }
        (None, None) if existing.trim().is_empty() => Ok(block.to_string()),
        (None, None) => Ok(format!("{}\n\n{}\n", existing.trim_end(), block)),
        _ => bail!(
            "Codex guidance has an unmatched wookie marker; repair {} / {} before reinstalling",
            CODEX_START,
            CODEX_END
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_block_replacement_preserves_surrounding_content() {
        let existing = format!("before\n{CODEX_START}\nold\n{CODEX_END}\nafter");
        let updated = replace_codex_block(&existing, "NEW").unwrap();
        assert_eq!(updated, "before\nNEW\nafter");
    }

    #[test]
    fn unmatched_codex_markers_are_rejected() {
        let error = replace_codex_block(CODEX_START, "NEW").unwrap_err();
        assert!(error.to_string().contains("unmatched"));
    }
}
