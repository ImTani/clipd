# Solo Dev Workflow (Orchestrator + Coding Agent)

Normative for process. The agent follows the command discipline; the human
follows the loop structure.

## 1. Inner loop (seconds)
- `cargo check` is the heartbeat; full builds only to run something.
- `cargo clippy -- -D warnings` before any task is declared done.
- Tests via `cargo nextest run` (faster, better failure output than
  `cargo test`; falls back to `cargo test` if nextest is absent).
- Link time is the Windows iteration tax. In `.cargo/config.toml`:
  use `lld-link` as the linker for dev profile, `debug = 1` (line tables only),
  and `opt-level = 1` for the dev profile's dependencies
  (`[profile.dev.package."*"] opt-level = 1`) so the `windows` crate isn't
  rebuilt in molasses mode.
- The `windows` crate compiles ONLY the feature gates listed in Cargo.toml.
  Blanket features are forbidden (briefing rule): each `Win32_*` feature added
  must correspond to an API actually called. This alone halves clean-build time.

## 2. Command surface: the justfile
All routine actions go through `just` recipes so human and agent run identical
commands and nothing is "forgotten":
```
just check        # cargo check + clippy -D warnings + fmt --check
just test         # cargo nextest run
just run          # debug build + run with dev config + verbose tracing
just rig          # build & run tools/avrig click/flash measurement tool
just verify FILE  # ffprobe assertion script against a saved clip
just spike NAME   # build & run a /spikes binary
just trace        # launch with MFTrace attached (prints reminder of setup)
just release      # locked, stripped release build; prints binary size vs 10MB budget
```
The justfile is a Milestone-1 deliverable and grows only via DECISIONS.md.

## 3. Task loop (the orchestration unit)
1. One milestone-tracker item ≈ one agent task ≈ one short-lived branch named
   after it (`m2-drift-controller`). Trunk-based; branches live hours-days.
2. Agent delivers: diff + DECISIONS.md delta + "run X, expect Y" block.
3. Human: review diff AND DECISIONS.md (scope creep enters there), run `just
   check && just test`, then the hardware procedure on the test machine.
4. Numbers go back into the tracker next to the item. Merge. Item closed by
   measurement, never by claim.
5. Tag the repo at each milestone gate (`m1`, `m2`, ...). Regressions bisect
   between tags.

## 4. CI (GitHub Actions, windows-latest)
- Every PR: fmt --check, clippy -D warnings, nextest, cargo-deny
  (licenses + advisories), release build with binary-size print.
- CI has no GPU: hardware items are manual by design; CI's job is keeping the
  pure-logic 60% of the codebase (clock, pacing, ring, drift, rebase, config)
  permanently green.
- Artifact upload of the release exe per commit to main (nightly-style).

## 5. Debugging kit bindings
- VS Code + rust-analyzer + `cppvsdbg` debugger config checked into
  `.vscode/launch.json` (MSVC PDBs make native Windows debugging first-class —
  essential when stepping through COM interop).
- MFTrace for anything Media Foundation; GPUView/WPR for GPU-engine questions;
  PresentMon for frametime impact; RenderDoc for the color-convert pass.
- Rule: every hardware tool prints OS build, GPU adapters, driver versions on
  startup so pasted-back results are self-documenting.
- Driver version is pinned per milestone (04-TEST-MACHINE.md); a driver update
  is an orchestrator decision, recorded in the tracker.

## 6. Hygiene
- `rust-toolchain.toml` pins the stable version; bumps are explicit tasks.
- Cargo.lock committed. Dependency bumps are their own PRs, never mixed with
  features.
- Logs from every hardware test session are saved under `/testlogs/<date>-<item>/`
  (gitignored except a SUMMARY.md per session).
- One Windows restore point before the first driver pin (convenience rollback,
  not fear — see 06-SAFETY-AND-VMS.md).
