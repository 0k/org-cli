//! User configuration loaded from
//! `~/.config/org-cli/config.yml`.
//!
//! YAML with dash-cased keys.
//!
//! Schema (every field optional):
//!
//! ```yaml
//! clock:
//!   accepted-zones:
//!     - Europe/Paris
//!     - Asia/Shanghai
//! todos:
//!   db-path: ~/.cache/org-cli/todos.sqlite
//!   serve-addr: 127.0.0.1:7480
//! ```
//!
//! When the file or any key is missing, the corresponding check
//! is disabled (silent). Invalid YAML is a hard error so the
//! user notices immediately.

use std::path::PathBuf;
use std::str::FromStr;

use serde::Deserialize;

/// The full deserialised config.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub clock: ClockConfig,
    #[serde(default)]
    pub todos: TodosConfig,
}

/// `clock:` sub-section of the config.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ClockConfig {
    /// IANA zone names (e.g. `"Europe/Paris"`). A CLOCK line's
    /// embedded numerical offset must match at least one of
    /// these zones at the line's instant.
    #[serde(default, rename = "accepted-zones")]
    pub accepted_zones: Vec<String>,
}

impl ClockConfig {
    /// Resolve `accepted_zones` into IANA `Tz` instances,
    /// returning an error on the first unrecognised zone name.
    pub fn resolve_zones(&self) -> Result<Vec<chrono_tz::Tz>, String> {
        self.accepted_zones
            .iter()
            .map(|name| {
                chrono_tz::Tz::from_str(name)
                    .map_err(|e| format!("unknown IANA zone {:?}: {}", name, e))
            })
            .collect()
    }
}

/// `todos:` sub-section of the config.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TodosConfig {
    /// Where the sqlite index lives. When unset, callers fall back
    /// to [`default_todos_db_path`].
    #[serde(default, rename = "db-path")]
    pub db_path: Option<PathBuf>,
    /// TCP address the `daemon` HTTP+SSE server binds to. When unset,
    /// callers fall back to [`DEFAULT_SERVE_ADDR`].
    #[serde(default, rename = "serve-addr")]
    pub serve_addr: Option<String>,
}

/// Built-in default bind address for `org-cli daemon`.
pub const DEFAULT_SERVE_ADDR: &str = "127.0.0.1:7480";

impl TodosConfig {
    /// Resolve the effective sqlite index path: the configured
    /// `db-path` when set, else the built-in default under the user
    /// cache directory.
    pub fn resolve_db_path(&self) -> Result<PathBuf, String> {
        match &self.db_path {
            Some(p) => Ok(p.clone()),
            None => default_todos_db_path(),
        }
    }

    /// Resolve the effective daemon bind address: the configured
    /// `serve-addr` when set, else [`DEFAULT_SERVE_ADDR`].
    pub fn resolve_serve_addr(&self) -> String {
        self.serve_addr
            .clone()
            .unwrap_or_else(|| DEFAULT_SERVE_ADDR.to_string())
    }
}

/// The built-in default index location:
/// `$XDG_CACHE_HOME/org-cli/todos.sqlite`
/// (or `~/.cache/org-cli/todos.sqlite` if XDG is unset).
pub fn default_todos_db_path() -> Result<PathBuf, String> {
    dirs::cache_dir()
        .map(|d| d.join("org-cli").join("todos.sqlite"))
        .ok_or_else(|| "cannot determine cache directory".to_string())
}

impl Config {
    /// Load the config from the default path
    /// (`~/.config/org-cli/config.yml`).
    ///
    /// Returns `Ok(Default::default())` if the file does not
    /// exist (config is optional). Returns `Err(_)` if the
    /// file exists but is unreadable or contains invalid YAML.
    pub fn load_default() -> Result<Self, String> {
        match default_path() {
            Some(p) => Self::load_from(&p),
            None => Ok(Self::default()),
        }
    }

    /// Load the config from an explicit path. Missing file is
    /// not an error (returns the default).
    pub fn load_from(path: &std::path::Path) -> Result<Self, String> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let body = std::fs::read_to_string(path)
            .map_err(|e| format!("reading {}: {}", path.display(), e))?;
        if body.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_yaml::from_str::<Config>(&body)
            .map_err(|e| format!("parsing {}: {}", path.display(), e))
    }
}

/// Standard config location: `$XDG_CONFIG_HOME/org-cli/config.yml`
/// (or `~/.config/org-cli/config.yml` if XDG is unset).
pub fn default_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("org-cli").join("config.yml"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp(name: &str, body: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        p.push(format!("org-cli-config-test-{}-{}-{}", name, pid, nanos));
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        p
    }

    #[test]
    fn missing_file_returns_default() {
        let c = Config::load_from(std::path::Path::new("/no/such/path/config.yml")).unwrap();
        assert!(c.clock.accepted_zones.is_empty());
    }

    #[test]
    fn empty_file_returns_default() {
        let p = write_temp("empty", "");
        let c = Config::load_from(&p).unwrap();
        assert!(c.clock.accepted_zones.is_empty());
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn parses_dash_cased_accepted_zones() {
        let p = write_temp(
            "ok",
            "clock:\n  accepted-zones:\n    - Europe/Paris\n    - Asia/Shanghai\n",
        );
        let c = Config::load_from(&p).unwrap();
        assert_eq!(
            c.clock.accepted_zones,
            vec!["Europe/Paris".to_string(), "Asia/Shanghai".to_string()]
        );
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn rejects_underscore_keys() {
        // ``accepted_zones`` should NOT be picked up: the schema
        // explicitly demands ``accepted-zones`` (dash-cased).
        let p = write_temp(
            "underscore",
            "clock:\n  accepted_zones:\n    - Europe/Paris\n",
        );
        let c = Config::load_from(&p).unwrap();
        assert!(
            c.clock.accepted_zones.is_empty(),
            "underscored key should be ignored, got {:?}",
            c.clock.accepted_zones
        );
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn resolve_zones_ok() {
        let cfg = ClockConfig {
            accepted_zones: vec!["Europe/Paris".into(), "Asia/Shanghai".into()],
        };
        let zones = cfg.resolve_zones().unwrap();
        assert_eq!(zones.len(), 2);
    }

    #[test]
    fn resolve_zones_rejects_unknown() {
        let cfg = ClockConfig {
            accepted_zones: vec!["Europe/Paris".into(), "Mars/Olympus".into()],
        };
        let err = cfg.resolve_zones().unwrap_err();
        assert!(err.contains("Mars/Olympus"), "got: {}", err);
    }

    #[test]
    fn invalid_yaml_is_a_hard_error() {
        let p = write_temp("bad", "clock: {accepted-zones: [unclosed\n");
        assert!(Config::load_from(&p).is_err());
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn parses_dash_cased_todos_db_path() {
        let p = write_temp("todos-db", "todos:\n  db-path: /custom/todos.sqlite\n");
        let c = Config::load_from(&p).unwrap();
        assert_eq!(c.todos.db_path, Some(PathBuf::from("/custom/todos.sqlite")));
        assert_eq!(
            c.todos.resolve_db_path().unwrap(),
            PathBuf::from("/custom/todos.sqlite")
        );
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn todos_db_path_defaults_when_unset() {
        let c = TodosConfig::default();
        let resolved = c.resolve_db_path().unwrap();
        assert!(
            resolved.ends_with("org-cli/todos.sqlite"),
            "got {}",
            resolved.display()
        );
    }

    #[test]
    fn parses_dash_cased_serve_addr() {
        let p = write_temp("serve-addr", "todos:\n  serve-addr: 0.0.0.0:9999\n");
        let c = Config::load_from(&p).unwrap();
        assert_eq!(c.todos.serve_addr.as_deref(), Some("0.0.0.0:9999"));
        assert_eq!(c.todos.resolve_serve_addr(), "0.0.0.0:9999");
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn serve_addr_defaults_when_unset() {
        let c = TodosConfig::default();
        assert_eq!(c.resolve_serve_addr(), "127.0.0.1:7480");
    }

    #[test]
    fn rejects_underscore_todos_key() {
        // ``db_path`` (underscored) must be ignored; the schema
        // demands ``db-path`` (dash-cased).
        let p = write_temp("todos-underscore", "todos:\n  db_path: /x.sqlite\n");
        let c = Config::load_from(&p).unwrap();
        assert!(c.todos.db_path.is_none());
        std::fs::remove_file(p).ok();
    }
}
