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

pub fn resolve(home: &Path, flag: Option<&str>, cwd: &Path) -> Result<Wiki> {
    if let Some(slug) = flag {
        return open(home, slug);
    }
    let global = GlobalConfig::load(home)?;

    let match_path = |path: &Path| -> Option<(String, usize)> {
        let path = canon(path);
        let mut best: Option<(String, usize)> = None;
        for (slug, entry) in &global.wikis {
            for root in &entry.project_roots {
                let root = canon(Path::new(root));
                if path.starts_with(&root) {
                    let depth = root.components().count();
                    if best.as_ref().map_or(true, |(_, d)| depth > *d) {
                        best = Some((slug.clone(), depth));
                    }
                }
            }
        }
        best
    };

    if let Some((slug, _)) = match_path(cwd) {
        return open(home, &slug);
    }
    // Worktree fallback: match the main checkout's path instead.
    if let Some(main) = git_main_worktree(cwd) {
        if let Some((slug, _)) = match_path(&main) {
            return open(home, &slug);
        }
    }

    let known: Vec<&str> = global.wikis.keys().map(String::as_str).collect();
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

    pub fn save_page(&self, page: &mut Page, bump_updated: bool) -> Result<()> {
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

    pub fn save_config(&self) -> Result<()> {
        let path = self.dir.join("wookie.toml");
        fs::write(&path, toml::to_string_pretty(&self.config)?)
            .with_context(|| format!("writing {}", path.display()))
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
