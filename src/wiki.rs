//! Wiki storage and resolution. A wiki lives at `<WOOKIE_HOME>/<slug>/` with a
//! `wookie.toml` and a `pages/` tree. Resolution order: explicit slug, cwd
//! prefix match against registered project roots, then the git main-worktree
//! fallback so linked worktrees land on the same wiki.

use crate::config::GlobalConfig;
use crate::page::Page;
use anyhow::{bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct WikiConfig {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub project_roots: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_commit: Option<bool>,
    /// Project commit the wiki was last synced to (set by `wookie ingest --mark`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ingest_commit: Option<String>,
    /// Top-level namespaces pages are filed under. Empty means the built-in
    /// defaults apply (kept last: TOML wants tables after plain values).
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub sections: std::collections::BTreeMap<String, SectionConfig>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SectionKind {
    /// Descriptive knowledge: architecture, code reference, decisions.
    #[default]
    Info,
    /// Normative content: checkable via `wookie critique`, locked by default.
    Rules,
}

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SectionConfig {
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub kind: SectionKind,
    /// Override the default lock (rules sections are locked unless told otherwise).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locked: Option<bool>,
    /// Page names (relative to the section) doctor insists on, e.g. "overview".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required: Vec<String>,
}

impl SectionConfig {
    pub fn is_locked(&self) -> bool {
        self.locked.unwrap_or(self.kind == SectionKind::Rules)
    }
}

pub fn default_sections() -> std::collections::BTreeMap<String, SectionConfig> {
    let s = |description: &str, kind: SectionKind, required: &[&str]| SectionConfig {
        description: description.into(),
        kind,
        locked: None,
        required: required.iter().map(|r| r.to_string()).collect(),
    };
    use SectionKind::{Info, Rules};
    std::collections::BTreeMap::from([
        (
            "architecture".to_string(),
            s("System structure, boundaries, how subsystems interact", Info, &["overview"]),
        ),
        (
            "code".to_string(),
            s("Module-by-module reference (ingest seeds these)", Info, &[]),
        ),
        (
            "decisions".to_string(),
            s("Why things are the way they are, one page per decision", Info, &[]),
        ),
        (
            "guides".to_string(),
            s("How to do common tasks: build, test, release, debug", Info, &[]),
        ),
        (
            "style".to_string(),
            s("Code style, naming, idioms, review conventions", Rules, &[]),
        ),
        (
            "workflow".to_string(),
            s("How to commit, branch, PR, review and release; team process rules", Rules, &[]),
        ),
    ])
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
struct UnlockState {
    #[serde(default)]
    unlocks: std::collections::BTreeMap<String, String>,
}

pub struct Wiki {
    pub slug: String,
    pub dir: PathBuf,
    pub config: WikiConfig,
    pub auto_commit: bool,
}

/// If `cwd` is inside a git repo, return the main worktree's root, so any
/// linked worktree resolves to the same project. None outside git or for
/// bare repos.
pub fn git_main_worktree(cwd: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .args(["-C"])
        .arg(cwd)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let common = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    if common.file_name()?.to_str()? == ".git" {
        common.parent().map(Path::to_path_buf)
    } else {
        None
    }
}

fn canon(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

pub fn open(home: &Path, slug: &str) -> Result<Wiki> {
    let dir = home.join(slug);
    let cfg_path = dir.join("wookie.toml");
    if !cfg_path.exists() {
        bail!(
            "no wiki '{slug}' at {} (run `wookie list` to see known wikis)",
            dir.display()
        );
    }
    let raw = fs::read_to_string(&cfg_path)
        .with_context(|| format!("reading {}", cfg_path.display()))?;
    let config: WikiConfig =
        toml::from_str(&raw).with_context(|| format!("parsing {}", cfg_path.display()))?;
    let global = GlobalConfig::load(home)?;
    let auto_commit = config.auto_commit.unwrap_or(global.defaults.auto_commit);
    Ok(Wiki {
        slug: slug.to_string(),
        dir,
        config,
        auto_commit,
    })
}

/// Every wiki under home (a dir containing wookie.toml). This, not the
/// global config, is the source of truth for what exists.
pub fn all_wikis(home: &Path) -> Vec<String> {
    let mut slugs: Vec<String> = std::fs::read_dir(home)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .filter(|e| e.path().join("wookie.toml").exists())
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    slugs.sort();
    slugs
}

pub fn resolve(home: &Path, flag: Option<&str>, cwd: &Path) -> Result<Wiki> {
    if let Some(slug) = flag {
        return open(home, slug);
    }
    // Each wiki's own wookie.toml project_roots decide resolution, so
    // editing roots (or `wookie roots --add`) takes effect immediately.
    let mut wikis = vec![];
    for slug in all_wikis(home) {
        if let Ok(w) = open(home, &slug) {
            wikis.push(w);
        }
    }

    let match_path = |path: &Path| -> Option<usize> {
        let path = canon(path);
        let mut best: Option<(usize, usize)> = None; // (wiki idx, depth)
        for (i, w) in wikis.iter().enumerate() {
            for root in &w.config.project_roots {
                let root = canon(Path::new(root));
                if path.starts_with(&root) {
                    let depth = root.components().count();
                    if best.map_or(true, |(_, d)| depth > d) {
                        best = Some((i, depth));
                    }
                }
            }
        }
        best.map(|(i, _)| i)
    };

    let hit = match_path(cwd).or_else(|| git_main_worktree(cwd).and_then(|m| match_path(&m)));
    if let Some(i) = hit {
        return Ok(wikis.swap_remove(i));
    }

    let known: Vec<&str> = wikis.iter().map(|w| w.slug.as_str()).collect();
    if known.is_empty() {
        bail!("no wikis exist yet. Create one with `wookie init` from your project directory.");
    }
    bail!(
        "no wiki matches {} — pass --wiki <slug> or register this project with `wookie init`.\nKnown wikis: {}",
        cwd.display(),
        known.join(", ")
    );
}

/// Page ids are relative paths under pages/ without the .md extension.
pub fn validate_id(id: &str) -> Result<()> {
    if id.is_empty() {
        bail!("page id is empty");
    }
    if id.starts_with('/') || id.ends_with('/') {
        bail!("page id '{id}' must not start or end with '/'");
    }
    for seg in id.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." {
            bail!("page id '{id}' has an invalid path segment");
        }
        if seg.starts_with('.') {
            bail!("page id '{id}' must not contain hidden segments");
        }
        if !seg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            bail!("page id '{id}' may only contain letters, digits, '-', '_', '.' and '/'");
        }
        // Lowercase-only, hard rule: on case-insensitive filesystems (macOS)
        // 'STYLE/checks' aliases 'style/checks' and would bypass section locks.
        if seg.chars().any(|c| c.is_ascii_uppercase()) {
            bail!("page id '{id}' must be lowercase (did you mean '{}'?)", id.to_lowercase());
        }
    }
    Ok(())
}

impl Wiki {
    pub fn pages_dir(&self) -> PathBuf {
        self.dir.join("pages")
    }

    pub fn page_path(&self, id: &str) -> Result<PathBuf> {
        validate_id(id)?;
        Ok(self.pages_dir().join(format!("{id}.md")))
    }

    pub fn exists(&self, id: &str) -> bool {
        self.page_path(id).map(|p| p.exists()).unwrap_or(false)
    }

    pub fn page_ids(&self) -> Vec<String> {
        let root = self.pages_dir();
        let mut ids: Vec<String> = walkdir::WalkDir::new(&root)
            .into_iter()
            // Hidden dirs (e.g. pages/.obsidian) are never pages.
            .filter_entry(|e| {
                !e.file_name()
                    .to_str()
                    .map(|n| n.starts_with('.'))
                    .unwrap_or(false)
                    || e.depth() == 0
            })
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .filter_map(|e| {
                let rel = e.path().strip_prefix(&root).ok()?;
                let s = rel.to_str()?;
                s.strip_suffix(".md").map(|s| s.replace('\\', "/"))
            })
            .collect();
        ids.sort();
        ids
    }

    pub fn load_page(&self, id: &str) -> Result<Page> {
        let path = self.page_path(id)?;
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("no page '{id}' (looked at {})", path.display()))?;
        Ok(Page::parse(id, &raw))
    }

    pub fn all_pages(&self) -> Vec<Page> {
        self.page_ids()
            .iter()
            .filter_map(|id| self.load_page(id).ok())
            .collect()
    }

    /// Checked save: refuses pages in locked sections. This is THE
    /// enforcement point; command-level checks only improve error timing.
    pub fn save_page(&self, page: &mut Page, bump_updated: bool) -> Result<()> {
        self.assert_writable(&page.id)?;
        self.save_page_raw(page, bump_updated)
    }

    /// Unchecked save, for tool-internal mechanical operations only
    /// (doctor frontmatter repair, mv link rewrites). Never route agent
    /// content through this.
    pub fn save_page_raw(&self, page: &mut Page, bump_updated: bool) -> Result<()> {
        if bump_updated {
            page.fm.updated = crate::page::today();
        }
        if page.fm.created.is_empty() {
            page.fm.created = crate::page::today();
        }
        let path = self.page_path(&page.id)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&path, page.render()).with_context(|| format!("writing {}", path.display()))
    }

    pub fn delete_page(&self, id: &str) -> Result<()> {
        self.assert_writable(id)?;
        let path = self.page_path(id)?;
        fs::remove_file(&path).with_context(|| format!("no page '{id}'"))
    }

    /// Pages whose bodies link to `id`.
    pub fn backlinks(&self, id: &str) -> Vec<String> {
        self.all_pages()
            .iter()
            .filter(|p| p.id != id && p.links().iter().any(|l| l == id))
            .map(|p| p.id.clone())
            .collect()
    }

    /// Effective sections: the wiki's own, or the built-in defaults so
    /// pre-sections wikis get the feature without migration.
    pub fn sections(&self) -> std::collections::BTreeMap<String, SectionConfig> {
        if self.config.sections.is_empty() {
            default_sections()
        } else {
            self.config.sections.clone()
        }
    }

    pub fn save_config(&self) -> Result<()> {
        let path = self.dir.join("wookie.toml");
        fs::write(&path, toml::to_string_pretty(&self.config)?)
            .with_context(|| format!("writing {}", path.display()))
    }

    fn unlocks_path(&self) -> PathBuf {
        self.dir.join(".unlocks.toml")
    }

    fn load_unlocks(&self) -> std::collections::BTreeMap<String, String> {
        fs::read_to_string(self.unlocks_path())
            .ok()
            .and_then(|raw| toml::from_str::<UnlockState>(&raw).ok())
            .map(|st| st.unlocks)
            .unwrap_or_default()
    }

    fn save_unlocks(&self, unlocks: std::collections::BTreeMap<String, String>) -> Result<()> {
        let raw = toml::to_string_pretty(&UnlockState { unlocks })?;
        fs::write(self.unlocks_path(), raw)?;
        Ok(())
    }

    /// Make sure transient/local files stay out of the wiki's git history.
    pub fn ensure_gitignore(&self) -> Result<()> {
        let path = self.dir.join(".gitignore");
        let mut cur = fs::read_to_string(&path).unwrap_or_default();
        let mut changed = false;
        for entry in [".unlocks.toml", "pages/.obsidian/"] {
            if !cur.lines().any(|l| l.trim() == entry) {
                if !cur.is_empty() && !cur.ends_with('\n') {
                    cur.push('\n');
                }
                cur.push_str(entry);
                cur.push('\n');
                changed = true;
            }
        }
        if changed {
            fs::write(&path, cur)?;
        }
        Ok(())
    }

    /// A locked section is temporarily writable while an unlock is active.
    pub fn is_unlocked(&self, section: &str) -> bool {
        self.load_unlocks()
            .get(section)
            .and_then(|ts| chrono::DateTime::parse_from_rfc3339(ts).ok())
            .map(|exp| chrono::Utc::now() < exp)
            .unwrap_or(false)
    }

    /// Error unless the page's section is writable. The message doubles as
    /// agent instructions: get user permission before unlocking.
    pub fn assert_writable(&self, id: &str) -> Result<()> {
        let Some((section, _)) = id.split_once('/') else {
            return Ok(());
        };
        let sections = self.sections();
        let Some(cfg) = sections.get(section) else {
            return Ok(());
        };
        if cfg.is_locked() && !self.is_unlocked(section) {
            bail!(
                "section '{section}' is locked (it holds this project's rules). \
                 Do NOT unlock it on your own: ask the user for explicit permission first, \
                 then run `wookie unlock {section}` (auto-relocks in 15 min) and retry."
            );
        }
        Ok(())
    }

    pub fn unlock(&self, section: &str, minutes: u64) -> Result<String> {
        let minutes = minutes.clamp(1, 24 * 60);
        let sections = self.sections();
        let Some(cfg) = sections.get(section) else {
            bail!(
                "unknown section '{section}'. Sections: {}",
                sections.keys().cloned().collect::<Vec<_>>().join(", ")
            );
        };
        if !cfg.is_locked() {
            return Ok(format!("Section '{section}' is not locked."));
        }
        let mut unlocks = self.load_unlocks();
        let now = chrono::Utc::now();
        let expiry = now + chrono::Duration::minutes(minutes as i64);
        unlocks.insert(section.to_string(), expiry.to_rfc3339());
        unlocks.retain(|_, ts| {
            chrono::DateTime::parse_from_rfc3339(ts).map(|e| now < e).unwrap_or(false)
        });
        self.ensure_gitignore()?;
        self.save_unlocks(unlocks)?;
        Ok(format!(
            "Unlocked section '{section}' for {minutes} min (relock early with `wookie lock {section}`)."
        ))
    }

    pub fn relock(&self, section: &str) -> Result<String> {
        let mut unlocks = self.load_unlocks();
        unlocks.remove(section);
        self.save_unlocks(unlocks)?;
        Ok(format!("Locked section '{section}'."))
    }

    fn git(&self, args: &[&str]) {
        let _ = Command::new("git")
            .arg("-C")
            .arg(&self.dir)
            .args(args)
            .output();
    }

    pub fn init_git(&self) {
        self.git(&["init", "-q"]);
    }

    /// Best-effort history. Failures (no git, nothing staged) are silent:
    /// history is a nicety, never a blocker for an agent mid-task.
    pub fn commit(&self, msg: &str) {
        if !self.auto_commit {
            return;
        }
        self.git(&["add", "-A"]);
        self.git(&[
            "-c",
            "user.name=wookie",
            "-c",
            "user.email=wookie@localhost",
            "commit",
            "-q",
            "-m",
            msg,
        ]);
    }
}
