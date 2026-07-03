## What this project is
A single-binary, native Windows replay-buffer clipper in Rust. Continuous
capture (monitor or focused window) → hardware encode → in-memory compressed
ring buffer → hotkey saves last N seconds as fMP4. Second mode: record next
N minutes to disk. Tray icon + TOML config. Nothing else.

## Hard constraints (violations are bugs regardless of whether code works)
1. **No scope additions.** The non-goals list in 01-PROJECT-PLAN.md §1 is
   normative. Do not add overlays, editors, uploaders, telemetry, game
   detection, or "small helpful extras" without an explicit orchestrator
   instruction in the task prompt.
2. **Dependency whitelist** (core binary): `windows`, `wasapi`, `rubato`,
   `global-hotkey`, `tray-icon`, `serde`, `toml`, `tracing`,
   `tracing-appender`, `tracing-subscriber`, `crossbeam-channel`, `thiserror`.
   Dev-deps are free (e.g. `hound`, `assert_cmd`). Adding anything else requires
   a line in DECISIONS.md and a callout in the task summary — never bury a new
   dep.
3. **No async runtime.** Threads + bounded channels only.
4. **No FFmpeg linkage, no vendor encoder SDKs** in v1. Media Foundation only.
5. **No process injection, no low-level keyboard hooks** (RegisterHotKey via
   `global-hotkey` only), no registry writes beyond the documented Run key.
6. **Pixels stay on the GPU.** Any code path that copies frame pixel data to
   system RAM is wrong unless the task explicitly says otherwise (e.g. a
   debug-dump utility behind a feature flag).
7. **Budgets are requirements** (01-PROJECT-PLAN.md §1). If an implementation
   choice risks a budget, say so before building and ask for guidance rather than silently choosing.
8. **Follow YAGNI principle** for features. Don't reinvent the wheel, use existing libraries where you can unless they add too much overhead.

## Architecture recap (details in 01-PROJECT-PLAN.md §2)
Threads: main/tray · capture (WGC → D3D11 VideoProcessor BGRA→NV12) ·
encode (MF async H.264 MFT) · audio (WASAPI loopback + mic → rubato → MF AAC) ·
buffer/mux (ring + fMP4 writer). Communication: `crossbeam_channel::bounded`.
All timestamps: i64 ticks (100 ns), single QPC master domain per
02-AV-SYNC-SPEC.md §0. That spec is frozen; implement it literally, including
its constants, thresholds, and adjustment rules.

## Repository layout (create in Milestone 1, keep flat)
```
/src
  main.rs            // tray, hotkeys, config load, watchdog, wiring
  config.rs          // schema v1, versioned, --check-config
  clock.rs           // QPC↔ticks, monotonicity guard (unit-test heavy)
  capture/           // wgc.rs, convert.rs (VideoProcessor), pacing.rs (CFR grid)
  encode/            // mft_h264.rs, mft_aac.rs (async MFT state machines)
  audio/             // wasapi_stream.rs, gaps.rs, drift.rs, devices.rs (IMMNotification)
  ring.rs            // packet ring, dual caps, GOP eviction
  mux/               // fmp4.rs or sinkwriter.rs (per Milestone-0 decision)
  save.rs            // rebasing per spec §4, atomic write
  watchdog.rs        // thresholds per spec §6.3
  ui.rs              // tray states, toasts
/spikes              // Milestone-0 throwaway code, kept for reference, never linked
/tools/avrig         // click/flash measurement rig + ffprobe assertion script
DECISIONS.md         // append-only log of choices the agent made
```

## Coding conventions
- Rust stable, `x86_64-pc-windows-msvc`. Edition 2021+. `cargo clippy -- -D
  warnings` and `cargo fmt --check` must pass on every task completion.
- Errors: `thiserror` enums per module; `Result` everywhere; `unwrap`/`expect`
  allowed only in tests and in `main()` before threads start. Panics in a
  worker thread must be caught at the thread boundary and routed to the
  watchdog (a dead thread with a live tray icon is the incumbent failure mode
  we exist to kill).
- `unsafe`: confined to the COM/D3D/MF wrapper modules. Every unsafe block
  gets a `// SAFETY:` comment stating the invariant. No unsafe in logic
  modules (ring, pacing, drift, save, config) — those must be 100% safe and
  unit-testable.
- COM threading: each worker thread that touches COM calls
  `CoInitializeEx(COINIT_MULTITHREADED)` on start and uninit on exit. MF:
  `MFStartup` once (main), `MFShutdown` on exit. Document apartment
  assumptions at the top of each module.
- Logging: `tracing` with a target per module. Every save attempt, device
  change, epoch restart, watchdog trigger, and threshold crossing from spec
  §6.3 emits a structured event. When in doubt, log — the plan's trust model
  depends on the log answering "why didn't my clip save."
- Every constant from 02-AV-SYNC-SPEC.md lives in one `spec_constants.rs`
  with a doc comment citing the spec section. No magic numbers inline.

## Testing rules
- Pure-logic modules (clock, pacing grid, gap synthesis, drift controller,
  ring eviction, rebasing math, config) get exhaustive unit tests INCLUDING
  the spec's edge numbers (e.g. gap exactly 20_000 ticks; slot round-half;
  eviction with byte-cap pressure; rebase across a GOP boundary).
- Hardware paths (WGC, MFT, WASAPI) get thin integration binaries under
  `/tools` that the orchestrator runs manually on the test machine; the agent
  writes them plus a checklist of expected output, and cites
  04-TEST-MACHINE.md for machine-specific expectations.
- The ffprobe assertion script (track durations within 1 AAC frame, monotonic
  PTS, CFR deltas, fragment validity) is a Milestone-3 deliverable and runs on
  every saved test clip thereafter.

## How to handle ambiguity (the non-iterative contract)
1. If 02-AV-SYNC-SPEC.md answers it (including via its adjustment rules),
   apply the rule. No question needed.
2. Else if 01-PROJECT-PLAN.md answers it, apply.
3. Else choose the option that is (a) simpler, (b) more logged, (c) reversible,
   append the decision + rationale to DECISIONS.md, and flag it at the TOP of
   the task summary so the orchestrator sees it without reading code.
4. Only stop and ask when a decision is irreversible or crosses a hard
   constraint. Batch such questions; do not trickle them.

## Task hygiene for the orchestrator's benefit
- One milestone-item per task where possible; name the branch after it.
- Task summary format: what was built · decisions made (with DECISIONS.md
  refs) · what to run on the test machine and the expected numbers · known
  gaps. Keep it short; the orchestrator is a human with a laptop, not CI.
- Never claim a hardware path "works" — claim it "builds and is ready for
  the 04-TEST-MACHINE.md procedure X". Only the machine says it works.

## Devflow discipline (see 07-DEVFLOW.md; normative for the agent)
- All routine actions via `just` recipes; if a needed recipe doesn't exist,
  add it in the same PR and note it in DECISIONS.md.
- `cargo check` while iterating; task completion requires `just check` and
  `just test` green locally (fmt, clippy -D warnings, nextest).
- `windows` crate feature gates: add ONLY the `Win32_*` features for APIs
  actually called, in the same commit that calls them. Blanket features are a
  review-rejection offense.
- Dependency bumps and toolchain bumps are standalone PRs, never mixed with
  feature work. Cargo.lock is committed.
- Branch per tracker item, named after it. Every task summary ends with the
  "run X on the test machine, expect Y" block — no exceptions, even for
  pure-logic tasks (then it's "expect: no hardware step; CI green suffices").

## UI rules (Feature-Complete phase, M7 — see 08-FEATURE-COMPLETE.md)
- Framework: egui/eframe only. No WebView, no other UI crates. `eframe` and
  `egui` join the dependency whitelist for the UI module alone.
- The settings window is a SATELLITE: lazily created from the tray, talks to
  the engine over the existing channels, and the engine must be fully
  functional if the window never opens. No engine code may depend on, link
  against, or block on UI modules. Enforce with module visibility: `ui`
  depends on engine types, never the reverse.
- UI writes config exclusively through the same versioned-TOML path as
  --check-config; no second config representation.
- No UI work before M7 unless a task explicitly says otherwise.

## Naming placeholder
The product name is undecided. Use the working crate/binary name `clipd` and a
single `PRODUCT_NAME: &str` constant in `spec_constants.rs` referenced by tray
tooltip, logs, and config header. Renaming later must be a one-constant +
one-Cargo.toml change; anything that hardcodes the name elsewhere is a bug.

## Scope ratchet
01-PROJECT-PLAN.md non-goals + the REJECTED list in 08-FEATURE-COMPLETE.md are
a ratchet: they can only be reopened by an explicit orchestrator instruction
quoted in the task prompt. "It was easy to add" is not a justification that
appears in DECISIONS.md.
