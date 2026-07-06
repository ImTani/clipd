//! Binary smoke tests — the built `clipd.exe` must **load and run** its
//! no-engine subcommands.
//!
//! Why this exists: the lib/bin *unit* tests link a test harness that
//! dead-strips code paths nothing references (e.g. the tray-building path in
//! `ui.rs`), so a load-time import failure in the real binary — a missing DLL
//! entrypoint pulled in by a UI dependency (`STATUS_ENTRYPOINT_NOT_FOUND`,
//! `0xc0000139`) — slips past `cargo test` entirely. These spawn the actual exe,
//! which resolves every import at load, so that class of regression fails CI.
//! (DECISIONS.md 2026-07-06 "M5 T2 fixup".)

use assert_cmd::Command;

/// `--version` loads the whole binary (all imports resolved before `main`) and
/// exits 0 — the direct guard against a load-time entrypoint failure.
#[test]
fn version_loads_and_runs() {
    Command::cargo_bin("clipd")
        .unwrap()
        .arg("--version")
        .assert()
        .success();
}

/// `--help` is the other no-engine path and exits 0.
#[test]
fn help_loads_and_runs() {
    Command::cargo_bin("clipd")
        .unwrap()
        .arg("--help")
        .assert()
        .success();
}

/// `--check-config` with no file prints the built-in defaults and exits 0 —
/// exercises the config path on the loaded binary too.
#[test]
fn check_config_defaults_runs() {
    Command::cargo_bin("clipd")
        .unwrap()
        .args(["--check-config", "this-path-does-not-exist.toml"])
        .assert()
        .success();
}
