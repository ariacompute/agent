//! CLI-side config for `aria-agent` (the `aria-agent-cloud` binary).
//!
//! Mirrors `aria-router-config`'s CLI config helpers: a small sidecar config
//! that only carries the Releases `upgrade_url`, plus the shared
//! `~/.ariacompute` home / `lib/` layout used by the FFI cdylib. All I/O is
//! plain `std::io` so the module stays dependency-light and testable.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Sidecar CLI config (separate from any server recipe).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentCliConfig {
    #[serde(default)]
    pub upgrade_url: String,
}

/// Default Releases org root for `aria-agent upgrade` (GitHub).
pub const DEFAULT_UPGRADE_URL: &str = "https://github.com/ariacompute";
/// Alternative Releases org root (Gitee mirror).
pub const GITEE_UPGRADE_URL: &str = "https://gitee.com/ariacompute";

/// Resolve the Releases org root from a short site name.
///
/// `gitee` → Gitee mirror; anything else (including `github`) → GitHub default.
pub fn upgrade_url_for_site(site: &str) -> String {
    match site.to_ascii_lowercase().as_str() {
        "gitee" => GITEE_UPGRADE_URL.to_string(),
        _ => DEFAULT_UPGRADE_URL.to_string(),
    }
}

/// `~/.ariacompute`, overridable via `ARIA_COMPUTE_HOME` (shared with router).
pub fn aria_home() -> std::io::Result<PathBuf> {
    if let Ok(override_home) = std::env::var("ARIA_COMPUTE_HOME") {
        if !override_home.is_empty() {
            return Ok(PathBuf::from(override_home));
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    Ok(PathBuf::from(home).join(".ariacompute"))
}

/// `~/.ariacompute/agent-cli.yml`.
pub fn cli_config_path() -> std::io::Result<PathBuf> {
    Ok(aria_home()?.join("agent-cli.yml"))
}

/// FFI install dir shared with native SDKs.
pub fn lib_dir() -> std::io::Result<PathBuf> {
    Ok(aria_home()?.join("lib"))
}

/// Ensure `~/.ariacompute`, `lib/`, and `tmp/` exist.
pub fn ensure_aria_home() -> std::io::Result<PathBuf> {
    let home = aria_home()?;
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(home.join("lib"))?;
    std::fs::create_dir_all(home.join("tmp"))?;
    Ok(home)
}

/// Load CLI config; missing file → empty default (caller decides what to do).
pub fn load_cli_config() -> std::io::Result<AgentCliConfig> {
    let path = cli_config_path()?;
    if !path.exists() {
        return Ok(AgentCliConfig::default());
    }
    let raw = std::fs::read_to_string(&path)?;
    serde_yaml::from_str(&raw)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))
}

/// Persist CLI config (creates `~/.ariacompute` if needed).
pub fn save_cli_config(cfg: &AgentCliConfig) -> std::io::Result<PathBuf> {
    ensure_aria_home()?;
    let path = cli_config_path()?;
    let raw = serde_yaml::to_string(cfg)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
    std::fs::write(&path, raw)?;
    Ok(path)
}

/// Remove the CLI config file if present. Returns the path if it existed.
pub fn clear_cli_config() -> std::io::Result<Option<PathBuf>> {
    let path = cli_config_path()?;
    if path.exists() {
        std::fs::remove_file(&path)?;
        Ok(Some(path))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::Mutex;

    // `aria_home()` reads the process-global `ARIA_COMPUTE_HOME`, so env-mutating
    // tests must be serialized to avoid cross-thread races.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_aria_home(dir: &Path, f: impl Fn()) {
        with_aria_home_seeded(dir, None, f)
    }

    /// Run `f` with `ARIA_COMPUTE_HOME=dir`, restoring `seed` (or unsetting it)
    /// afterwards. `seed` lets the restore path be exercised for both the
    /// "previous value existed" and "previous value was absent" cases.
    fn with_aria_home_seeded(dir: &Path, seed: Option<&str>, f: impl Fn()) {
        let _g = ENV_LOCK.lock().unwrap();
        match seed {
            Some(v) => std::env::set_var("ARIA_COMPUTE_HOME", v),
            None => std::env::remove_var("ARIA_COMPUTE_HOME"),
        }
        let prev = std::env::var("ARIA_COMPUTE_HOME").ok();
        std::env::set_var("ARIA_COMPUTE_HOME", dir);
        let _ = std::fs::remove_dir_all(dir);
        f();
        let _ = std::fs::remove_dir_all(dir);
        restore_env("ARIA_COMPUTE_HOME", prev);
    }

    /// Restore one environment variable to its previous value (`None` = unset).
    fn restore_env(key: &str, prev: Option<String>) {
        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }

    #[test]
    fn upgrade_url_for_site_resolves_known_sources() {
        assert_eq!(upgrade_url_for_site("github"), DEFAULT_UPGRADE_URL);
        assert_eq!(upgrade_url_for_site("GITHUB"), DEFAULT_UPGRADE_URL);
        assert_eq!(upgrade_url_for_site("gitee"), GITEE_UPGRADE_URL);
        assert_eq!(upgrade_url_for_site(""), DEFAULT_UPGRADE_URL);
        assert_eq!(upgrade_url_for_site("anything"), DEFAULT_UPGRADE_URL);
    }

    #[test]
    fn defaults_empty_upgrade_url() {
        assert!(AgentCliConfig::default().upgrade_url.is_empty());
    }

    #[test]
    fn save_then_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("aria_agent_cli_{}", std::process::id()));
        with_aria_home(&dir, || {
            let cfg = AgentCliConfig {
                upgrade_url: "https://github.com/ariacompute".into(),
            };
            let path = save_cli_config(&cfg).unwrap();
            assert!(path.ends_with("agent-cli.yml"));
            let loaded = load_cli_config().unwrap();
            assert_eq!(loaded.upgrade_url, "https://github.com/ariacompute");
            assert!(lib_dir().unwrap().ends_with("lib"));
        });
    }

    #[test]
    fn missing_config_returns_default() {
        let dir =
            std::env::temp_dir().join(format!("aria_agent_cli_missing_{}", std::process::id()));
        with_aria_home(&dir, || {
            let loaded = load_cli_config().unwrap();
            assert!(loaded.upgrade_url.is_empty());
        });
    }

    #[test]
    fn aria_home_falls_back_to_the_user_home() {
        let _g = ENV_LOCK.lock().unwrap();
        let prev_override = std::env::var("ARIA_COMPUTE_HOME").ok();
        let prev_home = std::env::var("HOME").ok();
        std::env::remove_var("ARIA_COMPUTE_HOME");
        std::env::set_var("HOME", "/tmp/aria-home-fallback");
        let home = aria_home().unwrap();
        assert_eq!(home, PathBuf::from("/tmp/aria-home-fallback/.ariacompute"));
        // An empty override is ignored (treated as unset).
        std::env::set_var("ARIA_COMPUTE_HOME", "");
        assert_eq!(aria_home().unwrap(), home);
        // A non-empty override wins.
        std::env::set_var("ARIA_COMPUTE_HOME", "/tmp/aria-home-override");
        assert_eq!(
            aria_home().unwrap(),
            PathBuf::from("/tmp/aria-home-override")
        );

        restore_env("HOME", prev_home);
        restore_env("ARIA_COMPUTE_HOME", prev_override);
    }

    #[test]
    fn restore_env_covers_unset_and_set() {
        let _g = ENV_LOCK.lock().unwrap();
        restore_env("ARIA_TEST_RESTORE_ENV", Some("value".into()));
        assert_eq!(std::env::var("ARIA_TEST_RESTORE_ENV").unwrap(), "value");
        restore_env("ARIA_TEST_RESTORE_ENV", None);
        assert!(std::env::var("ARIA_TEST_RESTORE_ENV").is_err());
    }

    #[test]
    fn seeding_a_previous_override_is_restored() {
        let dir = std::env::temp_dir().join(format!("aria_agent_cli_seed_{}", std::process::id()));
        with_aria_home_seeded(&dir, Some("/tmp/aria-seeded-home"), || {
            // Inside the scope the test directory wins.
            assert_eq!(aria_home().unwrap(), dir);
            let _ = save_cli_config(&AgentCliConfig::default());
        });
        assert_eq!(
            std::env::var("ARIA_COMPUTE_HOME").unwrap(),
            "/tmp/aria-seeded-home"
        );
        std::env::remove_var("ARIA_COMPUTE_HOME");
    }

    #[test]
    fn clear_removes_only_when_present() {
        let dir = std::env::temp_dir().join(format!("aria_agent_cli_clear_{}", std::process::id()));
        with_aria_home(&dir, || {
            // Nothing to clear yet.
            assert!(clear_cli_config().unwrap().is_none());
            // Write then clear.
            save_cli_config(&AgentCliConfig {
                upgrade_url: "https://gitee.com/ariacompute".into(),
            })
            .unwrap();
            let cleared = clear_cli_config().unwrap();
            assert!(cleared
                .map(|p| p.ends_with("agent-cli.yml"))
                .unwrap_or(false));
            // And it's gone.
            assert!(clear_cli_config().unwrap().is_none());
        });
    }
}
