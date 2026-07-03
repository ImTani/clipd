# Command surface for clipd (07-DEVFLOW.md §2). Human and agent run identical
# commands so nothing is "forgotten". This file is a Milestone-1 deliverable and
# grows ONLY via a DECISIONS.md entry.
#
# clipd is Windows-only, so recipes run under PowerShell (always present; unlike
# `sh`, just's default, which needs Git Bash on PATH). Run `just --list`.
# CI does NOT use just — it calls cargo directly (see .github/workflows/ci.yml).

set windows-shell := ["powershell.exe", "-NoLogo", "-NoProfile", "-Command"]

# Show the recipe list by default.
default:
    @just --list

# Fast correctness gate: check + clippy (-D warnings) + fmt --check. §2.
# just runs each line separately and stops on the first non-zero exit.
check:
    cargo check --all-targets
    cargo clippy --all-targets -- -D warnings
    cargo fmt --check

# Run the test suite. Prefers nextest; falls back to `cargo test`. §1/§2.
# `exit $LASTEXITCODE` propagates the runner's exit code out of the if/else.
test:
    if (Get-Command cargo-nextest -ErrorAction SilentlyContinue) { cargo nextest run } else { Write-Host 'cargo-nextest not found; using cargo test'; cargo test }; exit $LASTEXITCODE

# Debug build + run with verbose tracing. §2. Pass args after `--`, e.g.
# `just run --check-config`.
run *ARGS:
    $env:RUST_LOG = 'debug'; cargo run -- {{ARGS}}

# Validate + print the effective config (01-PROJECT-PLAN.md §3 pitfall 30).
check-config *ARGS:
    cargo run --quiet -- --check-config {{ARGS}}

# Locked, stripped release build; print binary size against the 10 MB budget
# (01-PROJECT-PLAN.md §1). §2.
release:
    cargo build --release --locked
    $b = 'target/release/clipd.exe'; $s = (Get-Item $b).Length; $budget = 10*1024*1024; Write-Host "binary: $b"; Write-Host "size:   $s bytes ($([math]::Round($s/1MB,2)) MB)"; Write-Host "budget: $budget bytes (10.00 MB)"; if ($s -gt $budget) { Write-Error 'FAIL: over the 10 MB binary budget'; exit 1 } else { Write-Host 'OK: within the 10 MB binary budget' }

# --- Recipes for tools that arrive in later milestones. Stubbed so the command
# --- surface is stable; each prints where its deliverable will live.

# Build & run the click/flash measurement rig (tools/avrig). Milestone 1+. §2.
rig:
    Write-Host 'not yet implemented: tools/avrig lands with the Milestone-1 measurement rig'

# ffprobe assertion script against a saved clip. Milestone 3 deliverable. §2.
verify FILE:
    Write-Host 'not yet implemented: the ffprobe assertion script lands in Milestone 3 (target: {{FILE}})'

# Build & run a /spikes binary by NAME. Milestone 0 spikes. §2. Each spike is a
# standalone crate under spikes/<NAME>/ (its own [workspace], never linked into
# clipd — see DECISIONS.md). Pass extra args after `--`.
spike NAME *ARGS:
    $env:RUST_LOG = if ($env:RUST_LOG) { $env:RUST_LOG } else { 'info' }; cargo run --manifest-path spikes/{{NAME}}/Cargo.toml -- {{ARGS}}

# Launch with MFTrace attached (Media Foundation). Milestone 1+. §2/§5.
trace:
    Write-Host 'not yet implemented: MFTrace wiring lands with the Milestone-0/1 MF encoder spike'
