# DECISIONS

Append-only log of choices the coding agent made, per `CLAUDE.md` "How to handle
ambiguity". Newest last. Each entry: what, why, and (where relevant) the
reversible fallback. Scope creep is meant to be visible here.

---

## 2026-07-03 — Bootstrap / calibration task

Decisions carried over from the previous session's `HANDOVER.md` §2, now recorded
here so the handover file can be deleted:

- **License = `GPL-3.0-only`.** The source is FOSS but the compiled binary is
  sold (e.g. on Steam). As sole copyright holder you can still sell binaries;
  GPL copyleft stops a competitor shipping a closed-source paid fork (Krita is
  the precedent — GPL, sold on Steam). **Caveat:** if outside contributions are
  ever accepted, add a DCO or lightweight CLA to retain relicensing/selling
  rights. Solo development = no issue. `LICENSE` is the verbatim GPLv3 text from
  gnu.org.
- **Project relocated off OneDrive** to `X:\clipd` (disk pressure on C: +
  avoiding OneDrive sync locking the build directory).
- **`CARGO_HOME` moved to `X:\cargo`** (C: had ~4.6 GB free); persisted as a User
  env var with `X:\cargo\bin` on the User PATH.

### Bootstrap structural decisions (this task)

- **Crate is split library + binary** (`src/lib.rs` + `src/main.rs`, both named
  `clipd`). Rationale: the pure-logic modules (clock, config, spec_constants)
  must be unit-testable in isolation (`CLAUDE.md` testing rules); a lib target
  makes that clean and lets future integration tests under `tests/` import them.
  The binary is a thin shell. Reversible.

- **`clock.rs` reads QPC via the `windows` crate** with exactly the
  `Win32_System_Performance` feature gate (added in the same commit that calls
  `QueryPerformanceCounter`/`QueryPerformanceFrequency`, per the no-blanket-
  features rule). The conversion math and the monotonicity guard are pure/safe
  and exhaustively unit-tested; `unsafe` is confined to the two FFI reads, each
  with a `// SAFETY:` comment. `clock` is not on `CLAUDE.md`'s no-unsafe list
  (ring/pacing/drift/save/config), so the syscall boundary living here is
  consistent with the conventions.

- **Profiles live in `Cargo.toml`, linker in `.cargo/config.toml`.**
  07-DEVFLOW §1 phrases the fast-iteration setup as all in `.cargo/config.toml`,
  but cargo does not read `[profile.*]` from there. So `debug = 1` and
  `[profile.dev.package."*"] opt-level = 1` are in `Cargo.toml`; the dev linker
  (`rust-lld.exe`) is in `.cargo/config.toml`. Verified a debug build links with
  rust-lld. If rust-lld ever breaks on a machine, delete the `.cargo/config.toml`
  `linker` line to fall back to the default MSVC linker (correctness unaffected).

- **`release` profile does NOT set `panic = "abort"`.** `CLAUDE.md` requires
  worker-thread panics to be caught at the thread boundary (`catch_unwind`) and
  routed to the watchdog; that needs unwinding. Size budget is met via
  `lto`/`codegen-units = 1`/`strip` instead.

- **`rust-toolchain.toml` pins `1.95.0`** (07-DEVFLOW §6). Toolchain bumps are
  standalone PRs.

- **Config schema v1 tolerates unknown keys on read but does not yet preserve
  them on rewrite.** There is no config-rewrite path in v1 (nothing writes
  config to disk), so `--check-config` is read-validate-print only. Full
  unknown-key *preservation* on rewrite (01-PROJECT-PLAN §3 pitfall 30) is a
  Milestone-5 deliverable and will likely need `toml_edit` (not on the current
  dependency whitelist — a whitelist addition to raise then). Flagged, not
  silently adopted.

- **`justfile` stubs `rig`/`verify`/`spike`/`trace`.** Their deliverables
  (measurement rig, ffprobe assertion script, spikes, MFTrace wiring) arrive in
  Milestones 0–3. The recipes exist now so the command surface is stable; each
  stub prints where its deliverable will land.

## 2026-07-03 — Milestone 0 spike #1: MF async hardware H.264 encoder

- **Spikes are standalone crates under `spikes/<name>/`, detached with an empty
  `[workspace]` table.** Rationale: CLAUDE.md requires `/spikes` code be "never
  linked" into `clipd`. A standalone crate (its own `Cargo.lock` + `target/`)
  guarantees the core build, `just check`, and CI never compile it and never
  feature-unify against its heavy `windows` MF/D3D11 feature set. Alternatives
  rejected: a `[[bin]]` in the core crate (would drag MF feature gates into the
  core `windows` dep — a no-blanket-features violation) and a workspace member
  (shares the lockfile and risks accidental `--workspace` builds in CI).
  Reversible: delete the folder; nothing references it.
- **`just spike NAME` now runs `cargo run --manifest-path spikes/NAME/Cargo.toml`**
  (was a stub). The command surface promised in 07-DEVFLOW §2 is now real for
  spikes. `.gitignore` gained `/spikes/*/target/`.
- **The spike uses `tracing` + `tracing-subscriber` for its own output; the CORE
  `Cargo.toml` is untouched.** Consistent with the existing "Resolved" note
  below: `tracing-subscriber` is whitelisted but is added to the *core* crate
  only when the engine first installs a subscriber (M5). Dev/spike deps are free
  (CLAUDE.md rule 2), so pulling it into a throwaway crate costs the core
  nothing.
- **Spike rate-control = average bitrate (8 Mbps), not CQP.** The spec mandates
  CQP (§6.1) for the product, but the spike's job is to prove the async MFT +
  D3D-manager path, for which a plain bitrate target is the simplest reliable
  config. CQP/CODECAPI tuning is deferred to Milestone 1. Flagged, not silently
  adopted as a product choice.
- **Result (measured on the Nitro V15 / RTX 4050 this session):** `NVIDIA H.264
  Encoder MFT` activated, 120 frames in → 120 out, drain clean; output is valid
  `h264`/Main/1280×720/yuv420p, `nb_read_frames=120`, full `ffmpeg` decode with
  zero errors. Tracker M0 item 1 marked closed with this evidence.

## 2026-07-03 — Milestone 0 spike #2: WGC primary-monitor capture

- **Standalone spike crate `spikes/wgc_capture_spike/`** (same detached-crate
  pattern as spike #1). Proves the WGC path: monitor `GraphicsCaptureItem` →
  free-threaded frame pool → backing `ID3D11Texture2D`, reading only the texture
  descriptor (pixels stay on the GPU, CLAUDE.md rule 6).
- **Primary output / HDR detection enumerates the whole DXGI factory**, not the
  D3D device's own adapter: on this Optimus laptop the device's adapter can drive
  zero outputs. We pick the output whose desktop rect starts at (0,0) and read
  its `DXGI_OUTPUT_DESC1.ColorSpace` to choose the pool pixel format.
- **Local binding renamed `display` → `disp`**: the identifier `display` collides
  with the `tracing` macro's internal `display` field helper inside `info!(...)`.
  Trivia, logged so the next spike author doesn't retrip it.
- **Result (Nitro V15 / RTX 4050, SDR):** WGC supported; item 1920×1080;
  first-frame `DXGI_FORMAT` = 87 (BGRA8) == SDR expectation; ~28 fps on a static
  screen. **HDR run outstanding** (needs the panel toggled to HDR).
- **Hybrid-graphics data point (04-TEST-MACHINE.md topology task):** the default
  `D3D_DRIVER_TYPE_HARDWARE` device landed on the **RTX 4050 (dGPU)** and WGC
  still delivered BGRA8 textures for the 1080p panel via its cross-adapter copy
  (pitfall 14 works out of the box). M1 must still enumerate + co-locate the
  encoder deliberately rather than trusting the default adapter pick.

### Resolved

- **`tracing-subscriber` added to the dependency whitelist.** It is required to
  install a subscriber and render `tracing` events to the rotating file
  (01-PROJECT-PLAN §2 logging row); `tracing` + `tracing-appender` alone cannot.
  Orchestrator-approved 2026-07-03; `CLAUDE.md` rule 2 whitelist updated
  accordingly. The crate is NOT yet a dependency in `Cargo.toml` (nothing wires
  logging yet — YAGNI per rule 8); it will be added in the same commit that
  first installs a subscriber (Milestone-0 spike or Milestone 5).
