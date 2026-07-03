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

## 2026-07-03 — Milestone 0 spike #3: WASAPI loopback + mic capture

- **Standalone spike crate `spikes/wasapi_audio_spike/`**, using the whitelisted
  `wasapi` crate + `hound` (free dev-dep) for WAV. Proves §2's audio-clock story
  is viable: desktop loopback (default Render endpoint, opened loopback) + mic
  (default Capture endpoint) captured concurrently, each to a 48 kHz/f32 WAV.
- **Loopback = Render device initialized with `Direction::Capture`.** `wasapi`
  0.23 detects (Render device, Capture request, Shared) and sets
  `AUDCLNT_STREAMFLAGS_LOOPBACK` internally — no separate loopback API.
- **Per-packet QPC timestamp source = `BufferInfo.timestamp`** from
  `read_from_device_to_deque` (the `IAudioCaptureClient::GetBuffer` QPC-position
  out-param), in 100 ns ticks. This is the §2.2 stamp; confirmed monotonic
  (~100 000 ticks / 10 ms per 480-frame packet) with 0 timestamp errors on
  hardware. Validates the spec's "stamp from QPC position, never sample-count"
  rule is implementable with this crate.
- **Result (Nitro V15):** loopback (Realtek Headphones) 597 packets / 5.97 s;
  mic (FIFINE) 593 packets / 5.93 s; both WAVs `pcm_f32le`/48k/2ch. QPC span ==
  captured duration. **Silence-gap and mic-unplug runs are manual and still
  outstanding** (need a human to go silent / yank the mic).
- **Deprecation noted:** used `get_next_packet_size` (0.23 renamed
  `get_next_nbr_frames`). Trivia for the next audio task.
- **Bug found + fixed via the mic-unplug validation (pitfall 3):** the first cut
  panicked (`attempt to subtract with overflow`) when the mic was yanked — the
  invalidated device returned a packet with a non-monotonic / garbage QPC
  `timestamp` and the `i64` gap subtraction underflowed. Fix: device read errors
  now end the stream cleanly (`device_lost`, logged) keeping the partial WAV;
  gap math is `i128`+clamped; a backward timestamp is counted as a device event
  (`non_monotonic`), never a gap. **M2 input:** §7 device-change handling must
  tolerate garbage timestamps across the transition, and the §0 monotonicity
  guard is exactly the mechanism for it. This is why the spike gate is "the
  human runs it on hardware," not "the agent says it works."
- **Unplug confirmed on hardware:** `AUDCLNT_E_DEVICE_INVALIDATED` (0x88890004)
  → logged, `device_lost`, partial WAV kept, other stream unaffected, exit 0.
  Reconnect does NOT auto-recover — that is the §7 IMMNotificationClient
  teardown+rebuild, a Milestone-2 deliverable, not a spike defect.
- **Silence finding (this HW/OS):** desktop loopback does NOT gap during silence
  within a session — played→silent→played showed continuous full frames,
  `event_timeouts=0`, `silent_packets=0`, `max_gap≈0.7 ms`, aligned with the mic.
  The classic pitfall-2 "loopback delivers nothing when quiet" is a
  modern-Windows-mitigated / fully-idle-engine case that did not reproduce here.
  M2 keeps the defensive silence-synthesis path (§2.3) for hardware/OS where it
  does occur; the probe already detects it (timeouts / max_gap / silent flag).
- **HDR verification (spike #2) is untestable on this hardware** — the Nitro V15
  panel is not HDR-capable. The WGC spike's HDR path is code-correct
  (auto-selects `R16G16B16A16Float` from the output colour space) but unverified;
  re-run on an HDR display when one is available. SDR path verified.

## 2026-07-03 — Milestone 0 spike #4: muxer decision (Sink Writer vs fMP4)

**Decision: hand-rolled fragmented MP4 (`mux/fmp4.rs`), NOT the MF Sink Writer.**

- **Spike evidence (`spikes/sinkwriter_mux_spike/`, Nitro V15 / RTX 4050):** the
  Sink Writer IS viable for correctness — fed spike #1's pre-encoded H.264
  samples in passthrough (sink input type == output type ⇒ no encoder inserted),
  it produced a valid `avc1` MP4, did NOT re-encode (bitrate preserved at ~116
  kbps, matching the raw stream vs the 8 Mbps target), and honored our QPC-grid
  timestamps to an exact `60/1` CFR / `2.000000` s / 120-frame file, ffmpeg
  decode clean. So MF will not fight us on timestamps — useful de-risking.
- **Why fMP4 wins anyway:** 02-AV-SYNC-SPEC §4 is FROZEN and overrides the plan's
  "if it works, use it." It mandates (a) crash-safety via one `moof`/`mdat`
  fragment per second (§4.6) — the Sink Writer writes `moov` only at
  `Finalize()`, so a crash mid-write yields an unplayable file, the exact
  "pressed the button, got nothing" failure the product exists to kill; (b)
  atomic `.part`→fsync→rename (§4.7); (c) explicit two-track rebasing against the
  cut keyframe origin (§4.2) on ring slices — control the Sink Writer's owned
  timing pipeline doesn't give.
- **Fallback:** the Sink Writer is retained as a documented, proven-working
  fallback if the hand-rolled fMP4 writer hits a wall. Reversible.
- This closes Milestone 0's decision item. No new dependencies; no whitelist
  change (both paths are Media Foundation via the `windows` crate).

### Resolved

- **`tracing-subscriber` added to the dependency whitelist.** It is required to
  install a subscriber and render `tracing` events to the rotating file
  (01-PROJECT-PLAN §2 logging row); `tracing` + `tracing-appender` alone cannot.
  Orchestrator-approved 2026-07-03; `CLAUDE.md` rule 2 whitelist updated
  accordingly. The crate is NOT yet a dependency in `Cargo.toml` (nothing wires
  logging yet — YAGNI per rule 8); it will be added in the same commit that
  first installs a subscriber (Milestone-0 spike or Milestone 5).

## 2026-07-03 — Milestone 1 Task A: shared D3D11 device + adapter topology (`src/gpu.rs`)

First real `src/` engine code for M1 (branch `m1-gpu-topology`). Closes the
`04-TEST-MACHINE.md` "adapter topology" pre-M1 task.

- **New module `src/gpu.rs`** — not in the CLAUDE.md flat-layout list, which does
  not enumerate a GPU/device module. Rationale: the D3D11 device is shared by the
  capture thread (WGC pool + `ID3D11VideoProcessor`) and the encode thread (async
  MFT). A single owner makes pitfall-14 co-location structural (the NV12 texture
  never crosses an adapter between convert and encode) instead of a per-frame
  concern. Alternative rejected: duplicating device creation in `capture/` and
  `encode/`, which would risk two devices on two adapters. Reversible: the module
  is small and only `main.rs` (probe path) references it so far.
- **Device flags = `BGRA_SUPPORT | VIDEO_SUPPORT`.** BGRA for WGC surfaces
  (spike #2); VIDEO for the video processor and the encoder's
  `IMFDXGIDeviceManager` (spike #1). Multithread protection enabled
  (`ID3D11Multithread::SetMultithreadProtected(true)`) so the async MFT worker
  can share the device with the capture thread.
- **Adapter selection `AdapterSelection::{Auto,PrimaryOutput,Index,Luid}`.**
  `Auto` (default) = `D3D_DRIVER_TYPE_HARDWARE` default pick — the M0-proven path.
  The pinned variants exist to measure the device-on-display (QSV, same-adapter
  WGC copy) vs device-on-dGPU (NVENC, cross-adapter copy) tradeoff. Correctness is
  identical; only copy/encoder cost differs, so `Auto` is the reversible default.
- **`windows` feature gates added (same commit):** `Win32_Foundation`,
  `Win32_Graphics_Direct3D`, `_Direct3D11`, `_Dxgi`, `_Dxgi_Common`, `_Gdi`. Gdi
  is required because `IDXGIOutput6::GetDesc1` is gated on it (its
  `DXGI_OUTPUT_DESC1` carries an `HMONITOR`), not because we call a Gdi function
  directly yet.
- **`probe-gpu` subcommand** added to `main.rs` to print the topology + the
  Auto-selected adapter and whether it co-locates with the primary output. This
  is the hardware deliverable for Task A.
- **Topology measured on the Nitro V15 this session** (refines the M0 finding):
  three adapters — `[0]` RTX 4050 Laptop (0x10DE, 5921 MiB) **drives the primary
  output `\.\DISPLAY5` 1920×1080 SDR**; `[1]` Intel UHD (0x8086, 128 MiB) drives
  `\.\DISPLAY1` 1536×864; `[2]` Microsoft Basic Render Driver (no outputs).
  `Auto` lands on the RTX 4050, which **currently drives the primary output**, so
  capture is a same-adapter copy and NVENC is co-located. NOTE: this is one MUX /
  Advanced-Optimus state (primary on the dGPU); the alternate state (primary on
  the iGPU, as M0 saw) remains a separate test configuration per 04-TEST-MACHINE.

## 2026-07-03 — Milestone 1 Task B: WGC monitor capture + all-MTA COM model

Branch `m1-wgc-capture` (stacked on `m1-gpu-topology`). Adds `src/com.rs` and
`src/capture/{mod,wgc}.rs`.

- **The engine is all-MTA, and COM crosses threads via per-type `unsafe impl
  Send` (TOP-OF-SUMMARY CALLOUT).** `windows` 0.62 interface types are
  `!Send + !Sync` (each wraps a bare `NonNull`; verified in the crate source —
  `IUnknown(NonNull<c_void>)` with no `Send`/`Sync` impl). But `TypedEventHandler::new`
  requires the callback be `Send`, and the pipeline moves D3D11 textures / MF
  samples between threads. Chosen model: every worker thread enters the
  multithreaded apartment (`com::ComMta` RAII guard, per CLAUDE.md's
  CoInitialize-per-thread rule), and each concrete type that crosses a thread
  boundary carries a local `unsafe impl Send` with a `SAFETY` note (e.g.
  `CapturedFrame`). Rationale: the wrapped objects are free-threaded,
  multithread-protected D3D11/DXGI resources or MTA-agile MF/WGC objects, sound
  to touch from any MTA thread; ownership is transferred (channel / Mutex),
  never mutably aliased. Alternatives rejected: `AgileReference<T>` everywhere
  (GIT-marshaling overhead + noise for objects that are already agile); a blanket
  `SendCom<T>` wrapper (hides which crossings are actually justified). Per-type
  `unsafe impl Send` keeps each crossing individually justified and confines the
  `unsafe` to the COM-wrapper modules where CLAUDE.md allows it. Reversible.
- **New module `src/com.rs`** — the shared `ComMta` apartment guard (mandated by
  CLAUDE.md; used by capture, and later encode/mux threads). Small; not in the
  flat-layout list, same latitude as `gpu.rs`.
- **Keep-latest cell:** `FrameArrived` stores the newest frame, dropping (and so
  `Close`-ing) any prior unconsumed one — the §1.4 "keep latest, release the rest
  before conversion" rule; no per-frame copy for dropped frames. Frame pool sized
  to **3 surfaces** (cell-held + consumer-in-flight + pool-composing) vs the
  spike's 2, to avoid dropped deliveries while the consumer holds a frame during
  conversion.
- **`SystemRelativeTime` used verbatim** as the 100 ns arrival tick (§1.1); if a
  frame lacks it (never observed) the frame is dropped rather than stamped with a
  fake time.
- **`IsCursorCaptureEnabled` (config) and `IsBorderRequired=false` (pitfall 9)**
  are best-effort — logged and skipped on builds that don't expose them.
- **`FrameArrived` token is a bare `i64` in `windows` 0.62** (not
  `EventRegistrationToken`, which is not exported).
- **`capture-probe [SECS]` subcommand** added for hardware validation.
- **windows features added same-commit:** `Win32_System_Com`, `Foundation`,
  `Graphics`, `Graphics_Capture`, `Graphics_DirectX`, `Graphics_DirectX_Direct3D11`,
  `Win32_System_WinRT_Direct3D11`, `Win32_System_WinRT_Graphics_Capture`.
- **Measured on the Nitro V15 this session:** `capture-probe 3` → primary monitor
  1920×1080, 54 frames / 3.00 s (~18 fps on a static screen, expected without
  on-screen motion), latest-frame `DXGI_FORMAT=87` (BGRA8) as predicted,
  monotonic `SystemRelativeTime`. Test-machine step: `clipd capture-probe 5` with
  a video playing, expect ~fps near the refresh rate and format 87.
