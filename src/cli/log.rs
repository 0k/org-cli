//! Logging: thin adapter over `kal-log` (named outputs, per-output filters).

pub use kal_log::LogHandle;

/// Install the global logger from CLI flags (no config file).
pub fn setup(args: &kal_log::LogArgs) -> Result<LogHandle, String> {
    kal_log::setup(&kal_log::LogConfig::default(), args).map_err(|e| e.to_string())
}
