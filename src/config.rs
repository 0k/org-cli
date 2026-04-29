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
}
