//! `clipd` binary entry point.
//!
//! At this milestone the engine (capture/encode/audio/ring/mux threads) is not
//! yet wired — this shell exists to prove the pure-logic modules build into a
//! runnable binary and to provide the `--check-config` calibration surface
//! (`01-PROJECT-PLAN.md §3` pitfall 30). Per `CLAUDE.md`, `expect`/`unwrap` are
//! permitted here because this runs before any worker thread starts.

use std::path::PathBuf;
use std::process::ExitCode;

use clipd::config::{default_config_path, Config};
use clipd::spec_constants::PRODUCT_NAME;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn print_usage() {
    println!(
        "{PRODUCT_NAME} {VERSION}\n\
         \n\
         USAGE:\n    \
             {PRODUCT_NAME} [OPTIONS]\n\
         \n\
         OPTIONS:\n    \
             --check-config [PATH]   Validate config (default: %APPDATA%\\{PRODUCT_NAME}\\config.toml)\n                            \
                                     and print the effective settings, then exit.\n    \
             -V, --version           Print version and exit.\n    \
             -h, --help              Print this help and exit.\n\
         \n\
         With no options the engine would start; it is not yet implemented\n\
         (Milestone 0 pending)."
    );
}

/// Run `--check-config`: load (or default) the config at `path`, print the
/// effective TOML, and return the process exit code.
fn run_check_config(path: PathBuf) -> ExitCode {
    if path.exists() {
        match Config::load(&path) {
            Ok(cfg) => {
                println!("# effective config from {}", path.display());
                match cfg.to_toml() {
                    Ok(toml) => {
                        print!("{toml}");
                        ExitCode::SUCCESS
                    }
                    Err(e) => {
                        eprintln!("error: {e}");
                        ExitCode::from(2)
                    }
                }
            }
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::from(2)
            }
        }
    } else {
        // No file: show the defaults so the user sees what would take effect.
        let cfg = Config::default();
        println!(
            "# no config file at {}; showing built-in defaults",
            path.display()
        );
        match cfg.to_toml() {
            Ok(toml) => {
                print!("{toml}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::from(2)
            }
        }
    }
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("-h") | Some("--help") => {
            print_usage();
            ExitCode::SUCCESS
        }
        Some("-V") | Some("--version") => {
            println!("{PRODUCT_NAME} {VERSION}");
            ExitCode::SUCCESS
        }
        Some("--check-config") => {
            let path = args
                .next()
                .map(PathBuf::from)
                .unwrap_or_else(default_config_path);
            run_check_config(path)
        }
        Some(other) => {
            eprintln!("error: unrecognized option '{other}'\n");
            print_usage();
            ExitCode::from(2)
        }
        None => {
            println!(
                "{PRODUCT_NAME} {VERSION}: engine not yet implemented (Milestone 0 pending).\n\
                 Try `{PRODUCT_NAME} --check-config` or `{PRODUCT_NAME} --help`."
            );
            ExitCode::SUCCESS
        }
    }
}
