//! Typed-but-flexible configuration editing. The CLI accepts dotted TOML
//! paths so new settings and custom sections do not require a new flag for
//! every field, then deserializes the result back into the real config type to
//! reject invalid shapes before anything reaches disk.

use crate::config::GlobalConfig;
use crate::wiki::{Wiki, WikiConfig};
use anyhow::{bail, Context, Result};
use serde::Serialize;

const PROTECTED_WIKI_KEYS: &[&str] = &["name", "project_roots", "last_ingest_commit"];

fn parse_value(raw: &str, force_string: bool) -> Result<toml::Value> {
    if force_string {
        return Ok(toml::Value::String(raw.to_string()));
    }
    if let Some(value) = toml::from_str::<toml::Value>(&format!("value = {raw}"))
        .ok()
        .and_then(|value| value.get("value").cloned())
    {
        return Ok(value);
    }

    let integer_candidate = raw
        .strip_prefix(['+', '-'])
        .unwrap_or(raw)
        .chars()
        .all(|character| character.is_ascii_digit() || character == '_')
        && raw.chars().any(|character| character.is_ascii_digit());
    if integer_candidate {
        bail!("integer configuration value is invalid or too large for TOML");
    }

    Ok(toml::Value::String(raw.to_string()))
}

fn segments(key: &str) -> Result<Vec<&str>> {
    let segments: Vec<&str> = key.split('.').collect();
    if segments.is_empty()
        || segments
            .iter()
            .any(|segment| segment.is_empty() || *segment == "..")
    {
        bail!("invalid configuration key '{key}'");
    }
    Ok(segments)
}

fn get_path<'a>(root: &'a toml::Value, key: &str) -> Result<&'a toml::Value> {
    let mut current = root;
    for segment in segments(key)? {
        current = current
            .get(segment)
            .with_context(|| format!("unknown configuration key '{key}'"))?;
    }
    Ok(current)
}

fn set_path(root: &mut toml::Value, key: &str, value: toml::Value) -> Result<()> {
    let segments = segments(key)?;
    let (leaf, parents) = segments.split_last().expect("segments is non-empty");
    let mut current = root;
    for segment in parents {
        let table = current
            .as_table_mut()
            .with_context(|| format!("configuration parent of '{key}' is not a table"))?;
        current = table
            .entry((*segment).to_string())
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
    }
    current
        .as_table_mut()
        .with_context(|| format!("configuration parent of '{key}' is not a table"))?
        .insert((*leaf).to_string(), value);
    Ok(())
}

fn unset_path(root: &mut toml::Value, key: &str) -> Result<()> {
    let segments = segments(key)?;
    let (leaf, parents) = segments.split_last().expect("segments is non-empty");
    let mut current = root;
    for segment in parents {
        current = current
            .get_mut(*segment)
            .with_context(|| format!("unknown configuration key '{key}'"))?;
    }
    let removed = current
        .as_table_mut()
        .with_context(|| format!("configuration parent of '{key}' is not a table"))?
        .remove(*leaf);
    if removed.is_none() {
        bail!("unknown configuration key '{key}'");
    }
    Ok(())
}

fn render_value(value: &toml::Value, json: bool) -> Result<String> {
    if json {
        return Ok(serde_json::to_string(value)?);
    }
    Ok(match value {
        toml::Value::String(value) => value.clone(),
        _ => value.to_string(),
    })
}

fn validate_global_config(config: &GlobalConfig) -> Result<()> {
    config.defaults.sessions.validate()?;
    config.defaults.history.validate()?;
    config.defaults.retrieval.validate()?;
    config.defaults.audit.validate()?;
    config.defaults.publish.validate()
}

fn assert_wiki_key_mutable(key: &str, rules_approved: bool) -> Result<()> {
    let first = key.split('.').next().unwrap_or_default();
    if PROTECTED_WIKI_KEYS.contains(&first) {
        bail!(
            "'{first}' is managed by a dedicated command and cannot be changed with `wookie config`"
        );
    }
    if first == "sections" && !rules_approved {
        bail!(
            "section configuration can change rules and locks; retry with --user-approved only after the user explicitly approves it"
        );
    }
    Ok(())
}

fn assert_global_key_mutable(key: &str) -> Result<()> {
    if key == "defaults" || key.starts_with("defaults.") {
        Ok(())
    } else {
        bail!(
            "global `wookie config` may only change defaults.*; wiki registration is managed by init/roots/remove-wiki/rename-wiki"
        )
    }
}

#[derive(Serialize)]
struct EffectiveWiki<'a> {
    wiki: &'a str,
    auto_commit: bool,
    sessions: &'a crate::config::SessionSettings,
    history: &'a crate::config::HistorySettings,
    retrieval: &'a crate::config::RetrievalSettings,
    audit: &'a crate::config::AuditSettings,
    publish: &'a crate::config::PublishSettings,
    stored: &'a WikiConfig,
}

fn effective_wiki(w: &Wiki) -> EffectiveWiki<'_> {
    EffectiveWiki {
        wiki: &w.slug,
        auto_commit: w.auto_commit,
        sessions: &w.sessions,
        history: &w.history,
        retrieval: &w.retrieval,
        audit: &w.audit,
        publish: &w.publish,
        stored: &w.config,
    }
}

pub fn show_wiki(w: &Wiki, effective: bool, json: bool) -> Result<String> {
    if effective {
        return if json {
            Ok(serde_json::to_string(&effective_wiki(w))?)
        } else {
            Ok(toml::to_string_pretty(&effective_wiki(w))?)
        };
    }
    if json {
        Ok(serde_json::to_string(&w.config)?)
    } else {
        Ok(toml::to_string_pretty(&w.config)?)
    }
}

pub fn show_global(home: &std::path::Path, json: bool) -> Result<String> {
    let config = GlobalConfig::load(home)?;
    if json {
        Ok(serde_json::to_string(&config)?)
    } else {
        Ok(toml::to_string_pretty(&config)?)
    }
}

pub fn get_wiki(w: &Wiki, key: &str, effective: bool, json: bool) -> Result<String> {
    let root = if effective {
        toml::Value::try_from(effective_wiki(w))?
    } else {
        toml::Value::try_from(&w.config)?
    };
    render_value(get_path(&root, key)?, json)
}

pub fn get_global(home: &std::path::Path, key: &str, json: bool) -> Result<String> {
    let root = toml::Value::try_from(GlobalConfig::load(home)?)?;
    render_value(get_path(&root, key)?, json)
}

pub fn set_wiki(
    w: &mut Wiki,
    key: &str,
    raw: &str,
    force_string: bool,
    rules_approved: bool,
    json: bool,
) -> Result<String> {
    assert_wiki_key_mutable(key, rules_approved)?;
    let value = parse_value(raw, force_string)?;
    let output_value = serde_json::to_value(&value)?;
    w.update_config(&format!("wookie: config set {key}"), move |config| {
        let mut root = toml::Value::try_from(&*config)?;
        set_path(&mut root, key, value)?;
        *config = root
            .try_into()
            .context("configuration value has the wrong type")?;
        Ok(())
    })?;
    if json {
        Ok(serde_json::json!({"scope": "wiki", "key": key, "value": output_value}).to_string())
    } else {
        Ok(format!("Set wiki configuration '{key}'."))
    }
}

pub fn unset_wiki(w: &mut Wiki, key: &str, rules_approved: bool, json: bool) -> Result<String> {
    assert_wiki_key_mutable(key, rules_approved)?;
    w.update_config(&format!("wookie: config unset {key}"), |config| {
        let mut root = toml::Value::try_from(&*config)?;
        unset_path(&mut root, key)?;
        *config = root
            .try_into()
            .context("configuration value has the wrong type")?;
        Ok(())
    })?;
    if json {
        Ok(serde_json::json!({"scope": "wiki", "key": key, "unset": true}).to_string())
    } else {
        Ok(format!(
            "Unset wiki configuration '{key}'; it now inherits its default."
        ))
    }
}

pub fn set_global(
    home: &std::path::Path,
    key: &str,
    raw: &str,
    force_string: bool,
    json: bool,
) -> Result<String> {
    assert_global_key_mutable(key)?;
    let value = parse_value(raw, force_string)?;
    let output_value = serde_json::to_value(&value)?;
    GlobalConfig::update(home, move |config| {
        let mut root = toml::Value::try_from(&*config)?;
        set_path(&mut root, key, value)?;
        let next: GlobalConfig = root
            .try_into()
            .context("configuration value has the wrong type")?;
        validate_global_config(&next)?;
        *config = next;
        Ok(())
    })?;
    if json {
        Ok(serde_json::json!({"scope": "global", "key": key, "value": output_value}).to_string())
    } else {
        Ok(format!("Set global configuration '{key}'."))
    }
}

pub fn unset_global(home: &std::path::Path, key: &str, json: bool) -> Result<String> {
    assert_global_key_mutable(key)?;
    GlobalConfig::update(home, |config| {
        let mut root = toml::Value::try_from(&*config)?;
        unset_path(&mut root, key)?;
        let next: GlobalConfig = root
            .try_into()
            .context("configuration value has the wrong type")?;
        validate_global_config(&next)?;
        *config = next;
        Ok(())
    })?;
    if json {
        Ok(serde_json::json!({"scope": "global", "key": key, "unset": true}).to_string())
    } else {
        Ok(format!("Unset global configuration '{key}'."))
    }
}

fn key_list(global: bool) -> Vec<String> {
    let keys = [
        "description",
        "auto_commit",
        "sessions.enabled",
        "sessions.initial_lookback_hours",
        "sessions.stale_after_minutes",
        "sessions.activity_debounce_seconds",
        "sessions.retention_days",
        "sessions.auto_prune_on_start",
        "sessions.poll_limit",
        "sessions.max_summary_bytes",
        "sessions.max_agent_bytes",
        "sessions.max_label_bytes",
        "sessions.max_body_bytes",
        "sessions.max_paths",
        "sessions.max_path_bytes",
        "sessions.max_targets",
        "sessions.max_idempotency_key_bytes",
        "sessions.max_metadata_entries",
        "sessions.max_metadata_key_bytes",
        "sessions.max_metadata_value_bytes",
        "sessions.max_git_dirty_paths",
        "sessions.max_git_branch_bytes",
        "sessions.max_git_commit_bytes",
        "sessions.max_git_worktree_bytes",
        "sessions.include_git_context",
        "sessions.heartbeat_on_activity",
        "sessions.default_kind",
        "sessions.default_importance",
        "history.lock_timeout_ms",
        "history.lock_stale_seconds",
        "history.commit_sessions",
        "history.fail_on_commit_error",
        "retrieval.prime_tokens",
        "retrieval.instruction_tokens",
        "retrieval.search_limit",
        "retrieval.search_tokens",
        "retrieval.excerpt_lines",
        "retrieval.max_per_section",
        "audit.source_provenance",
        "audit.critique_tokens",
        "publish.require_base_revision",
        "publish.orphan_policy",
        "publish.output_tokens",
        "sections.<name>.description  (requires --user-approved)",
        "sections.<name>.kind  (requires --user-approved)",
        "sections.<name>.locked  (requires --user-approved)",
        "sections.<name>.required  (requires --user-approved)",
    ];
    if global {
        keys.iter()
            .filter(|key| !key.starts_with("sections.") && **key != "description")
            .map(|key| format!("defaults.{key}"))
            .collect()
    } else {
        keys.iter().map(|key| (*key).to_string()).collect()
    }
}

pub fn keys(global: bool) -> String {
    key_list(global).join("\n")
}

pub fn keys_output(global: bool, json: bool) -> String {
    let keys = key_list(global);
    if json {
        serde_json::json!({
            "scope": if global { "global" } else { "wiki" },
            "keys": keys,
        })
        .to_string()
    } else {
        keys.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    fn temp_wiki(label: &str) -> (std::path::PathBuf, Wiki) {
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "wookie-settings-{label}-{}-{sequence}",
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
        let wiki = crate::wiki::open(&home, "test").unwrap();
        (base, wiki)
    }

    #[test]
    fn dotted_paths_can_create_nested_tables() {
        let mut value = toml::Value::Table(toml::map::Map::new());
        set_path(&mut value, "sessions.poll_limit", toml::Value::Integer(25)).unwrap();
        assert_eq!(
            get_path(&value, "sessions.poll_limit")
                .unwrap()
                .as_integer(),
            Some(25)
        );
        unset_path(&mut value, "sessions.poll_limit").unwrap();
        assert!(get_path(&value, "sessions.poll_limit").is_err());
    }

    #[test]
    fn parses_toml_or_falls_back_to_string() {
        assert_eq!(parse_value("true", false).unwrap().as_bool(), Some(true));
        assert_eq!(parse_value("42", false).unwrap().as_integer(), Some(42));
        assert_eq!(
            parse_value("[1, 2]", false).unwrap().as_array().unwrap(),
            &[toml::Value::Integer(1), toml::Value::Integer(2)]
        );
        assert_eq!(
            parse_value("normal", false).unwrap().as_str(),
            Some("normal")
        );
        assert_eq!(parse_value("true", true).unwrap().as_str(), Some("true"));
    }

    #[test]
    fn keys_include_all_bounded_retrieval_and_change_control_settings() {
        let wiki_keys = keys(false);
        let expected = [
            "retrieval.prime_tokens",
            "retrieval.instruction_tokens",
            "retrieval.search_limit",
            "retrieval.search_tokens",
            "retrieval.excerpt_lines",
            "retrieval.max_per_section",
            "audit.source_provenance",
            "audit.critique_tokens",
            "publish.require_base_revision",
            "publish.orphan_policy",
            "publish.output_tokens",
        ];
        for expected in expected {
            assert!(wiki_keys.lines().any(|key| key == expected), "{expected}");
        }

        let global_keys = keys(true);
        for expected in expected {
            let expected = format!("defaults.{expected}");
            assert!(global_keys.lines().any(|key| key == expected), "{expected}");
        }

        let machine: serde_json::Value = serde_json::from_str(&keys_output(true, true)).unwrap();
        assert_eq!(machine["scope"], "global");
        assert!(machine["keys"].is_array());
        assert!(machine["keys"]
            .as_array()
            .unwrap()
            .iter()
            .all(serde_json::Value::is_string));
    }

    #[test]
    fn concurrent_wiki_settings_merge_from_latest_config() {
        let (base, first) = temp_wiki("concurrent-wiki");
        let home = base.join("home");
        let second = crate::wiki::open(&home, "test").unwrap();
        let barrier = Arc::new(Barrier::new(3));

        let first_barrier = Arc::clone(&barrier);
        let first_thread = std::thread::spawn(move || {
            let mut wiki = first;
            first_barrier.wait();
            set_wiki(
                &mut wiki,
                "retrieval.search_limit",
                "17",
                false,
                false,
                false,
            )
        });
        let second_barrier = Arc::clone(&barrier);
        let second_thread = std::thread::spawn(move || {
            let mut wiki = second;
            second_barrier.wait();
            set_wiki(
                &mut wiki,
                "retrieval.search_tokens",
                "3100",
                false,
                false,
                false,
            )
        });
        barrier.wait();
        first_thread.join().unwrap().unwrap();
        second_thread.join().unwrap().unwrap();

        let stored = crate::wiki::open(&home, "test").unwrap();
        assert_eq!(stored.config.retrieval.search_limit, Some(17));
        assert_eq!(stored.config.retrieval.search_tokens, Some(3_100));
        std::fs::remove_dir_all(base).unwrap();
    }
}
