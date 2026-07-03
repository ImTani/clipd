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
use clipd::gpu::{self, AdapterSelection, GpuContext};
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
             probe-gpu               Print the GPU/output topology and the adapter the\n                            \
                                     shared device lands on, then exit.\n    \
             -V, --version           Print version and exit.\n    \
             -h, --help              Print this help and exit.\n\
         \n\
         With no options the engine would start; it is not yet implemented\n\
         (Milestone 0 pending)."
    );
}

/// Run `probe-gpu`: print the full adapter/output topology and which adapter the
/// `Auto`-selected shared device lands on. This closes the `04-TEST-MACHINE.md`
/// "adapter topology" pre-Milestone-1 task on real hardware.
fn run_probe_gpu() -> ExitCode {
    let topology = match gpu::enumerate_topology() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: could not enumerate GPU topology: {e}");
            return ExitCode::from(2);
        }
    };
    print!("{topology}");

    match GpuContext::new(AdapterSelection::Auto) {
        Ok(ctx) => {
            let co_located = topology
                .primary_adapter_index()
                .and_then(|i| topology.adapters.get(i as usize))
                .map(|a| a.luid == ctx.adapter_luid)
                .unwrap_or(false);
            println!(
                "\nAuto-selected device adapter: {} (luid {:#018x})",
                ctx.adapter_description, ctx.adapter_luid
            );
            println!(
                "Co-located with the primary-output adapter: {}",
                if co_located {
                    "yes (same-adapter WGC copy)"
                } else {
                    "no (WGC does a cross-adapter copy into this device's pool)"
                }
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: could not create the shared D3D11 device: {e}");
            ExitCode::from(2)
        }
    }
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
        Some("probe-gpu") => run_probe_gpu(),
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
