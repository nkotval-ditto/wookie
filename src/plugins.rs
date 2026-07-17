//! Plugin installers: teach Claude Code and Codex how to use wookie. Both are
//! generated from one embedded template so the guidance never drifts.

use crate::config::user_home;
use anyhow::{Context, Result};
use std::fs;

const GUIDANCE: &str = include_str!("../templates/guidance.md");
const CODEX_START: &str = "<!-- wookie:start -->";
const CODEX_END: &str = "<!-- wookie:end -->";

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum Target {
    Claude,
    Codex,
}

pub fn install(target: Target) -> Result<String> {
    match target {
        Target::Claude => install_claude(),
        Target::Codex => install_codex(),
    }
}

/// Claude Code: a skill at ~/.claude/skills/wookie/SKILL.md.
fn install_claude() -> Result<String> {
    let dir = user_home().join(".claude/skills/wookie");
    fs::create_dir_all(&dir)?;
    let path = dir.join("SKILL.md");
    let content = format!(
        "---\nname: wookie\ndescription: Read and grow the project's wookie wiki (local LLM-first knowledge base). Use when starting work on a project, when asked to look up or document project knowledge, or after learning something durable about a codebase. Triggers on \"wiki\", \"wookie\", \"document this\", \"what do we know about\".\n---\n\n{GUIDANCE}"
    );
    fs::write(&path, content).with_context(|| format!("writing {}", path.display()))?;
    Ok(format!("Installed Claude Code skill: {}", path.display()))
}

/// Codex: a managed block in ~/.codex/AGENTS.md, idempotent on reinstall.
fn install_codex() -> Result<String> {
    let dir = user_home().join(".codex");
    fs::create_dir_all(&dir)?;
    let path = dir.join("AGENTS.md");
    let block = format!("{CODEX_START}\n{GUIDANCE}\n{CODEX_END}");
    let existing = fs::read_to_string(&path).unwrap_or_default();

    let updated = match (existing.find(CODEX_START), existing.find(CODEX_END)) {
        (Some(start), Some(end)) if end > start => {
            let after = existing[end + CODEX_END.len()..].to_string();
            format!("{}{}{}", &existing[..start], block, after)
        }
        _ => {
            if existing.trim().is_empty() {
                block
            } else {
                format!("{}\n\n{}\n", existing.trim_end(), block)
            }
        }
    };
    fs::write(&path, updated).with_context(|| format!("writing {}", path.display()))?;
    Ok(format!("Installed Codex guidance block: {}", path.display()))
}
