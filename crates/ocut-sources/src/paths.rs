//! Where OpenCUT and Claude Code keep their files.

use std::path::{Path, PathBuf};

use ocut_core::Config;

/// The environment variables path resolution depends on.
#[derive(Debug, Clone, Default)]
pub struct Env {
    pub home: Option<PathBuf>,
    pub claude_config_dir: Option<PathBuf>,
    pub xdg_config_home: Option<PathBuf>,
    pub xdg_cache_home: Option<PathBuf>,
    pub xdg_runtime_dir: Option<PathBuf>,
}

impl Env {
    pub fn from_process() -> Self {
        let var = |k: &str| {
            std::env::var_os(k)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
        };
        Self {
            home: var("HOME"),
            claude_config_dir: var("CLAUDE_CONFIG_DIR"),
            xdg_config_home: var("XDG_CONFIG_HOME"),
            xdg_cache_home: var("XDG_CACHE_HOME"),
            xdg_runtime_dir: var("XDG_RUNTIME_DIR"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub home: PathBuf,
    /// `~/.config/opencut/config.toml`.
    pub config_file: PathBuf,
    /// `~/.cache/opencut`.
    pub cache_dir: PathBuf,
    /// `$XDG_RUNTIME_DIR/opencut/usage.json` (falls back to the cache dir).
    pub snapshot: PathBuf,
    pub statusline_cache: PathBuf,
    pub notify_state: PathBuf,
    /// The first Claude config dir: `$CLAUDE_CONFIG_DIR` or `~/.claude`.
    pub claude_dir: PathBuf,
    /// Every `projects` dir to scan.
    pub project_dirs: Vec<PathBuf>,
    pub credentials: PathBuf,
    pub sessions_dir: PathBuf,
    /// `~/.claude/settings.json` (statusline install).
    pub claude_settings: PathBuf,
}

fn abs(p: Option<&PathBuf>) -> Option<&Path> {
    p.map(PathBuf::as_path).filter(|p| p.is_absolute())
}

impl Paths {
    /// `None` without `$HOME`.
    pub fn resolve(env: &Env) -> Option<Self> {
        let home = env.home.clone().filter(|h| h.is_absolute())?;
        let config_file = Config::default_path(env.xdg_config_home.clone(), Some(home.clone()))?;
        let cache_dir = abs(env.xdg_cache_home.as_ref())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| home.join(".cache"))
            .join("opencut");
        let snapshot = abs(env.xdg_runtime_dir.as_ref())
            .map(|r| r.join("opencut"))
            .unwrap_or_else(|| cache_dir.clone())
            .join("usage.json");
        let first_claude_dir = env
            .claude_config_dir
            .as_ref()
            .and_then(|p| p.to_str())
            .and_then(|s| s.split(',').map(str::trim).find(|d| !d.is_empty()))
            .map(PathBuf::from)
            .filter(|p| p.is_absolute());
        let claude_dir = first_claude_dir.unwrap_or_else(|| home.join(".claude"));
        let project_dirs = crate::jsonl::default_project_dirs(
            &home,
            env.claude_config_dir.as_deref(),
            env.xdg_config_home.as_deref(),
        );
        Some(Self {
            config_file,
            statusline_cache: cache_dir.join("statusline.json"),
            notify_state: cache_dir.join("notify-state.json"),
            cache_dir,
            snapshot,
            credentials: claude_dir.join(".credentials.json"),
            sessions_dir: claude_dir.join("sessions"),
            claude_settings: claude_dir.join("settings.json"),
            claude_dir,
            project_dirs,
            home,
        })
    }

    pub fn from_process() -> Option<Self> {
        Self::resolve(&Env::from_process())
    }

    /// Adds `config.sources.claude_dirs` to the scanned project dirs.
    pub fn with_config(mut self, cfg: &Config) -> Self {
        for d in &cfg.sources.claude_dirs {
            let p = d.join("projects");
            if !self.project_dirs.contains(&p) {
                self.project_dirs.push(p);
            }
        }
        self
    }
}

/// Loads the config file; a missing file is the default config.
pub fn load_config(path: &Path) -> Result<Config, String> {
    match std::fs::read_to_string(path) {
        Ok(s) => Config::from_toml_str(&s).map_err(|e| e.to_string()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(format!("cannot read config: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_under_home() {
        let p = Paths::resolve(&Env {
            home: Some("/home/u".into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            p.config_file,
            PathBuf::from("/home/u/.config/opencut/config.toml")
        );
        assert_eq!(
            p.snapshot,
            PathBuf::from("/home/u/.cache/opencut/usage.json")
        );
        assert_eq!(
            p.credentials,
            PathBuf::from("/home/u/.claude/.credentials.json")
        );
        assert!(p.project_dirs.contains(&"/home/u/.claude/projects".into()));
    }

    #[test]
    fn xdg_and_claude_dir() {
        let p = Paths::resolve(&Env {
            home: Some("/home/u".into()),
            claude_config_dir: Some("/c1, /c2".into()),
            xdg_runtime_dir: Some("/run/user/1000".into()),
            xdg_cache_home: Some("relative".into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            p.snapshot,
            PathBuf::from("/run/user/1000/opencut/usage.json")
        );
        assert_eq!(p.cache_dir, PathBuf::from("/home/u/.cache/opencut"));
        assert_eq!(p.sessions_dir, PathBuf::from("/c1/sessions"));
        assert!(p.project_dirs.contains(&"/c2/projects".into()));
        assert!(Paths::resolve(&Env::default()).is_none());
    }

    #[test]
    fn missing_config_is_default() {
        assert_eq!(
            load_config(Path::new("/nonexistent/opencut.toml")).unwrap(),
            Config::default()
        );
    }
}
