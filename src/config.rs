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
//!   overlap:
//!     exclude-globs:
//!       - ~/notes/activity/**
//!       - ~/notes/presence/**
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
    /// Settings for the cross-entry `clock overlap` check.
    #[serde(default)]
    pub overlap: OverlapConfig,
}

/// `clock.overlap:` sub-section of the config.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OverlapConfig {
    /// Glob patterns matched against candidate file paths; matching
    /// files contribute no intervals to the overlap check.
    ///
    /// This is where the *work log* versus *evidence* distinction
    /// gets declared. Not every CLOCK line records time spent
    /// working:
    ///
    /// - `activity/**` holds generated activity segments — the
    ///   raw material a work log is written *from*, not a work log.
    /// - `presence/**` holds presence windows: one CLOCK per
    ///   stay in a place, routinely months long. Such a line
    ///   overlaps essentially every work clock by construction.
    ///
    /// Left empty, no file is excluded and the check still runs —
    /// it just reports the evidence-versus-work collisions too.
    ///
    /// A leading `~/` is expanded to the user's home directory.
    #[serde(default, rename = "exclude-globs")]
    pub exclude_globs: Vec<String>,
}

impl OverlapConfig {
    /// The configured globs with a leading `~/` expanded against the
    /// user's home directory.
    ///
    /// Expansion happens here rather than at glob-compile time so
    /// the raw config value stays exactly what the user typed, and
    /// so a missing home directory is an error the caller can
    /// report instead of a silently unexpanded pattern that would
    /// quietly match nothing.
    pub fn resolve_exclude_globs(&self) -> Result<Vec<String>, String> {
        self.exclude_globs
            .iter()
            .map(|g| match g.strip_prefix("~/") {
                Some(rest) => {
                    let home = dirs::home_dir()
                        .ok_or_else(|| format!("cannot expand {:?}: no home directory", g))?;
                    Ok(home.join(rest).to_string_lossy().into_owned())
                }
                None => Ok(g.clone()),
            })
            .collect()
    }
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
            ..Default::default()
        };
        let zones = cfg.resolve_zones().unwrap();
        assert_eq!(zones.len(), 2);
    }

    #[test]
    fn resolve_zones_rejects_unknown() {
        let cfg = ClockConfig {
            accepted_zones: vec!["Europe/Paris".into(), "Mars/Olympus".into()],
            ..Default::default()
        };
        let err = cfg.resolve_zones().unwrap_err();
        assert!(err.contains("Mars/Olympus"), "got: {}", err);
    }

    // ----- clock.overlap ------------------------------------------------

    #[test]
    fn overlap_exclude_globs_default_to_empty() {
        let c = Config::default();
        assert!(c.clock.overlap.exclude_globs.is_empty());
        assert!(c.clock.overlap.resolve_exclude_globs().unwrap().is_empty());
    }

    #[test]
    fn parses_dash_cased_overlap_exclude_globs() {
        let p = write_temp(
            "overlap",
            concat!(
                "clock:\n",
                "  overlap:\n",
                "    exclude-globs:\n",
                "      - /abs/path/**\n",
                "      - ~/notes/activity/**\n",
            ),
        );
        let c = Config::load_from(&p).unwrap();
        assert_eq!(
            c.clock.overlap.exclude_globs,
            vec![
                "/abs/path/**".to_string(),
                "~/notes/activity/**".to_string()
            ]
        );
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn overlap_coexists_with_accepted_zones() {
        let p = write_temp(
            "overlap-and-zones",
            concat!(
                "clock:\n",
                "  accepted-zones:\n",
                "    - Europe/Paris\n",
                "  overlap:\n",
                "    exclude-globs:\n",
                "      - /x/**\n",
            ),
        );
        let c = Config::load_from(&p).unwrap();
        assert_eq!(c.clock.accepted_zones, vec!["Europe/Paris".to_string()]);
        assert_eq!(c.clock.overlap.exclude_globs, vec!["/x/**".to_string()]);
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn rejects_underscore_overlap_key() {
        // ``exclude_globs`` (underscored) must be ignored; the
        // schema demands ``exclude-globs``.
        let p = write_temp(
            "overlap-underscore",
            "clock:\n  overlap:\n    exclude_globs:\n      - /x/**\n",
        );
        let c = Config::load_from(&p).unwrap();
        assert!(c.clock.overlap.exclude_globs.is_empty());
        std::fs::remove_file(p).ok();
    }

    #[test]
    fn resolve_exclude_globs_expands_leading_tilde() {
        let cfg = OverlapConfig {
            exclude_globs: vec!["~/notes/activity/**".into()],
        };
        let resolved = cfg.resolve_exclude_globs().unwrap();
        let home = dirs::home_dir().unwrap();
        assert_eq!(
            resolved,
            vec![
                home.join("notes/activity/**")
                    .to_string_lossy()
                    .into_owned()
            ]
        );
    }

    #[test]
    fn resolve_exclude_globs_leaves_absolute_paths_alone() {
        let cfg = OverlapConfig {
            exclude_globs: vec!["/abs/**".into(), "relative/**".into()],
        };
        let resolved = cfg.resolve_exclude_globs().unwrap();
        assert_eq!(
            resolved,
            vec!["/abs/**".to_string(), "relative/**".to_string()]
        );
    }

    #[test]
    fn resolve_exclude_globs_does_not_expand_bare_tilde_word() {
        // Only a ``~/`` prefix is a home reference; ``~foo`` is a
        // literal path component and must survive untouched.
        let cfg = OverlapConfig {
            exclude_globs: vec!["~backup/**".into()],
        };
        let resolved = cfg.resolve_exclude_globs().unwrap();
        assert_eq!(resolved, vec!["~backup/**".to_string()]);
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
