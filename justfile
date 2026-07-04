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

# Build & run the click/flash measurement rig (tools/avrig, §5). Standalone crate
# (own [workspace], never linked into clipd — like /spikes). Pass a subcommand +
# args after the recipe name, e.g. `just rig flash --seconds 35` or
# `just rig measure clip.mp4`. §2 / 02-AV-SYNC-SPEC §5.
rig *ARGS:
    $env:RUST_LOG = if ($env:RUST_LOG) { $env:RUST_LOG } else { 'info' }; cargo run --manifest-path tools/avrig/Cargo.toml -- {{ARGS}}

# ffprobe assertion script against one or more saved clips (02-AV-SYNC-SPEC §4/§5,
# CLAUDE.md testing rules). Standalone tool crate (own [workspace], never linked
# into clipd — like tools/avrig). Asserts stream shape, monotonic PTS, video CFR,
# the §4 save-rebase origin, track end-alignment (≤ 1 AAC frame), and full-decode
# validity; exit 0 iff every clip passes. For the §5 "50 consecutive saves" gate:
# `just verify (Get-ChildItem clips\*.mp4)`. §2.
verify *ARGS:
    cargo run --quiet --manifest-path tools/verify/Cargo.toml -- {{ARGS}}

# Build & run a /spikes binary by NAME. Milestone 0 spikes. §2. Each spike is a
# standalone crate under spikes/<NAME>/ (its own [workspace], never linked into
# clipd — see DECISIONS.md). Pass extra args after `--`.
spike NAME *ARGS:
    $env:RUST_LOG = if ($env:RUST_LOG) { $env:RUST_LOG } else { 'info' }; cargo run --manifest-path spikes/{{NAME}}/Cargo.toml -- {{ARGS}}

# Launch with MFTrace attached (Media Foundation). Milestone 1+. §2/§5.
trace:
    Write-Host 'not yet implemented: MFTrace wiring lands with the Milestone-0/1 MF encoder spike'
