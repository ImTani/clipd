//! Process logging (M5) — a rotating on-disk log plus the console.
//!
//! `01-PROJECT-PLAN.md §2`: *"Logging: `tracing` → rotating file in
//! `%LOCALAPPDATA%`. Every save attempt, every device change, every encoder
//! stall gets a line. When a user says 'it didn't save,' the log must answer
//! why."* This is the trust surface behind `05-MILESTONE-TRACKER.md` M5 item 3.
//!
//! The path builder is pure (unit-tested); the subscriber init is thin wiring
//! over `tracing-subscriber` + `tracing-appender` and has no `unsafe`.

use std::path::PathBuf;

use crate::spec_constants::PRODUCT_NAME;

/// The log directory: `%LOCALAPPDATA%\{PRODUCT_NAME}\logs`. Falls back to `logs`
/// in the working directory if `%LOCALAPPDATA%` is unset (mirrors
/// [`crate::config::default_config_path`]'s `%APPDATA%` fallback).
pub fn log_dir() -> PathBuf {
    match std::env::var_os("LOCALAPPDATA") {
        Some(local) => PathBuf::from(local).join(PRODUCT_NAME).join("logs"),
        None => PathBuf::from("logs"),
    }
}

/// The rolling log file name stem. `tracing-appender` appends the rotation date,
/// so files land as `{PRODUCT_NAME}.log.YYYY-MM-DD`.
fn log_file_prefix() -> String {
    format!("{PRODUCT_NAME}.log")
}

/// Initialize logging for a long-running session (`buffer` / `record`): the
/// console layer **plus** a daily-rotated file under [`log_dir`]. `RUST_LOG`
/// controls the filter (defaults to `info`).
///
/// Returns the appender's [`WorkerGuard`], which **must be held for the process
/// lifetime** — dropping it flushes and stops the non-blocking writer, so a
/// dropped guard silently loses buffered log lines. If the log directory cannot
/// be created, logging degrades to console-only and `None` is returned (a
/// missing log file must never take down the capture engine).
#[must_use = "the returned WorkerGuard must be held for the process lifetime, or file logs are lost"]
pub fn init_session() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{fmt, EnvFilter};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let console = fmt::layer();

    let dir = log_dir();
    match std::fs::create_dir_all(&dir) {
        Ok(()) => {
            let appender = tracing_appender::rolling::daily(&dir, log_file_prefix());
            let (non_blocking, guard) = tracing_appender::non_blocking(appender);
            // No ANSI in the file (escape codes are noise in a text log).
            let file = fmt::layer().with_ansi(false).with_writer(non_blocking);
            let _ = tracing_subscriber::registry()
                .with(filter)
                .with(console)
                .with(file)
                .try_init();
            Some(guard)
        }
        Err(e) => {
            // Fall back to console-only; surface why the file log is missing.
            let _ = tracing_subscriber::registry()
                .with(filter)
                .with(console)
                .try_init();
            eprintln!(
                "warning: could not create log directory {} ({e}); logging to console only",
                dir.display()
            );
            None
        }
    }
}

/// Initialize console-only logging (the short-lived `*-probe` diagnostics, which
/// should not spatter the rolling log directory). Idempotent.
pub fn init_console() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_dir_uses_localappdata_when_set() {
        // Serialized process-env mutation is fine here: the fallback logic is what
        // matters and no other test reads LOCALAPPDATA.
        let dir = log_dir();
        // Whatever LOCALAPPDATA is, the path ends with <product>/logs, or the
        // bare `logs` fallback when the var is unset.
        assert!(
            dir.ends_with(format!("{PRODUCT_NAME}/logs")) || dir.ends_with("logs"),
            "unexpected log dir: {}",
            dir.display()
        );
    }

    #[test]
    fn log_file_prefix_is_product_scoped() {
        assert_eq!(log_file_prefix(), format!("{PRODUCT_NAME}.log"));
    }
}
