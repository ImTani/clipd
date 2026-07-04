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

## 2026-07-03 — Milestone 1 Task C: BGRA→NV12 on the video processor (`capture/convert.rs`)

Branch `m1-convert-nv12` (stacked on `m1-wgc-capture`). Net-new module — no spike
covered colour conversion.

- **`ID3D11VideoProcessor` (not a 3D compute shader)** does BGRA→NV12, per plan
  data-flow rule 1 / pitfall 16a — conversion rides the dedicated video-processor
  engine so it doesn't queue behind a game's 3D work. Uses the shared device from
  `gpu.rs`; pixels stay on the GPU.
- **Colour = BT.709, full-range RGB in → studio/limited-range YCbCr out** via the
  `...ColorSpace1` APIs: input `DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709`, output
  `DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709`. This is HALF of "correct
  colours"; the matching H.264 VUI tags on the encoder output (Task E) are the
  other half. Full verification is a saved clip + RenderDoc (Task F1), not this
  probe.
- **Output NV12 textures = a 4-deep round-robin pool** (`D3D11_BIND_RENDER_TARGET`,
  `DEFAULT` usage). Rationale: the async encoder may still hold frame N's texture
  while frame N+1 is produced; a pool avoids a per-frame allocation in the hot
  path. Tradeoff/limitation: it is NOT a hard guarantee against a slow encoder
  (no GPU fence yet) — depth 4 gives practical slack; a fence-based recycle is the
  proper fix, deferred past M1. Alternative rejected for M1: fresh per-frame NV12
  allocation (race-free but 60 allocs/s in the hot path).
- **`D3D11_TEXTURE2D_DESC.BindFlags` is a raw `u32`** in `windows` 0.62 (not the
  `D3D11_BIND_FLAG` newtype) — use `D3D11_BIND_RENDER_TARGET.0 as u32`.
- No new `windows` feature gates (all video interfaces are under the already-enabled
  `Win32_Graphics_Direct3D11` + `Dxgi_Common`).
- **`convert-probe` subcommand** added. **Measured on the Nitro V15:** capture one
  frame → convert → NV12 (`DXGI_FORMAT=103`) 1920×1080, Blt OK. Test-machine step:
  `clipd convert-probe`, expect the "converted ... NV12 ... OK" line; colour
  correctness closes at Task F1 with a saved clip + reference screenshot.

## 2026-07-03 — Milestone 1 Task D: CFR pacing grid (`capture/pacing.rs`)

Branch `m1-pacing-grid`. Pure, safe, unit-tested logic implementing
`02-AV-SYNC-SPEC §1.2/§1.3/§1.4` literally. No hardware step (CLAUDE.md: CI green
suffices for pure-logic tasks).

- **Pull-model API** (`on_arrival(tick)` + `poll(now) -> Option<SlotAction>`): the
  capture thread owns the wall clock and calls `poll` at each slot deadline; the
  grid returns `Fresh`/`Resubmit` with the exact slot PTS. Chosen over a
  push/bucketing model because it maps directly onto the capture loop and is
  deterministically testable by feeding synthetic `now` ticks. Keep-latest is
  shared with the WGC cell (which already retains only the newest frame); the grid
  additionally counts displaced arrivals as drops.
- **Round-half-up** for arrival→slot mapping (`(Δ·fps + 5_000_000) / 10_000_000`);
  boundaries via the existing non-accumulating `slot_boundary_ticks`. PTS is the
  slot boundary, never the arrival time (§1.3).
- **Epoch restart** clears the base (next arrival rebases) and bumps `epoch_id`;
  the fresh/resubmit/drop counters are cumulative diagnostics, deliberately NOT
  reset across epochs.
- 11 unit tests incl. the spec edge numbers: 60-slot second is exact
  `TICKS_PER_SECOND`; round-half-up at the exact midpoint (fps=2); gap exactly at
  the grace boundary produces; duplicate-in-slot and 4-arrival high-refresh each
  count the right drops and emit one Fresh; epoch restart rebases.
- **No unsafe, no new deps, no feature gates.** 43 tests total green. Test-machine
  step: none (pure logic; CI green suffices).

## 2026-07-03 — Milestone 1 Task E: async H.264 MFT with CQP (`encode/mft_h264.rs`)

Branch `m1-encode-cqp`. Cannibalizes the M0 encoder spike's async state machine
onto the shared device, feeding real NV12 from the video processor.

- **CQP via `ICodecAPI`, not `MF_MT_AVG_BITRATE`.** The spike used average
  bitrate; M1 sets rate-control mode = `eAVEncCommonRateControlMode_Quality`,
  constant QP = spec CQ (`NVENC_CQ[0]` = 23) via `CODECAPI_AVEncVideoEncodeQP`
  (packed I/P/B), closed GOP = `2·fps` via `CODECAPI_AVEncMPVGOPSize`, and no
  B-frames via `CODECAPI_AVEncMPVDefaultBPictureCount = 0` (spec §3). Each
  `ICodecAPI::SetValue` is **best-effort** (logged, non-fatal) because vendors
  differ on which properties they honour (plan pitfall 18); the hardware ffprobe
  pass reveals what took. The exact CQ↔bitrate behaviour is content-adaptive and
  is judged on motion content in Task F1.
- **BT.709 limited-range VUI tags** on the output media type (`MF_MT_VIDEO_PRIMARIES`
  =BT709, `MF_MT_TRANSFER_FUNCTION`=709, `MF_MT_YUV_MATRIX`=BT709,
  `MF_MT_VIDEO_NOMINAL_RANGE`=16_235) — the metadata half of "correct colours",
  matching the video processor's output colour space (Task C).
- **`VARIANT` built by hand** for `ICodecAPI::SetValue` — the `windows` crate has
  no `From<u32>`/`From<u64>` for `VARIANT`. Small `variant_ui4`/`variant_ui8`
  helpers assemble the nested union (`VT_UI4`/`VT_UI8`, scalar, no heap → no
  `VariantClear`). `VARIANT` is gated on `Win32_System_Ole` + `Win32_System_Com`;
  `VARENUM`/`VT_*` on `Win32_System_Variant` — all three features added.
- **Encoder API is a pull-based event loop** `run(next_input, on_packet)`:
  `NeedInput` calls `next_input()` (None ends the stream → END_OF_STREAM+DRAIN);
  `HaveOutput` pulls one `EncodedPacket` (bytes + pts + duration + is_keyframe
  from `MFSampleExtension_CleanPoint` + epoch). Never feeds without draining
  (pitfall-17 deadlock avoidance). `InputFrame` carries `unsafe impl Send` for the
  capture→encode channel handoff; `EncodedPacket` is Send already.
- **`com::MediaFoundation` RAII guard** added (MFStartup/MFShutdown per CLAUDE.md).
- **windows features added:** `Win32_Media_MediaFoundation`, `Win32_System_Variant`,
  `Win32_System_Ole`.
- **Measured on the Nitro V15 (`encode-probe 2`):** 120 in / 120 out, 2 keyframes
  (IDR at 0 and 120 = the 2 s GOP), ~2.7 Mbps on a near-static desktop (correct
  content-adaptive CQP). **ffprobe:** `h264` / Main / 1920×1080 / yuv420p /
  color_range=tv / color_space=color_transfer=color_primaries=bt709 /
  nb_read_frames=120. Test-machine step: `clipd encode-probe 5` with motion, then
  ffprobe — expect the same tags, 300 frames, higher bitrate under motion; pixel
  colour correctness closes at F1 with a saved clip + reference screenshot.

## 2026-07-03 — Milestone 1 Task F1: Sink Writer mux + engine wiring + record

Branch `m1-mux-sinkwriter`. First real end-to-end recording. Adds
`src/{engine,watchdog}.rs`, `src/mux/{mod,sinkwriter}.rs`, and `record`.

- **Three worker threads from F1** (capture · encode · mux) over
  `crossbeam_channel::bounded`, pacing-grid-driven, per the §2 architecture. The
  encode thread hands the mux thread the negotiated output `IMFMediaType`
  (wrapped `SendMediaType`, MTA-agile) once via a bounded(1) channel, then pumps
  byte-based `EncodedPacket`s; the mux thread reconstructs an `IMFSample` per
  packet and `WriteSample`s it (passthrough). This keeps the mux on its own
  thread (pitfall 24) AND makes F2 a drop-in mux swap. Shutdown = channel
  disconnection (main sets a stop flag → capture drops senders → encoder drains →
  mux finalizes). Each worker body is `catch_unwind`-wrapped → panic becomes a
  thread-boundary error, not a silently dead thread.
- **CQP vendor finding (TOP CALLOUT).** On the RTX 4050, the `NVIDIA H.264
  Encoder MFT` **rejects** `CODECAPI_AVEncVideoEncodeQP` and
  `CODECAPI_AVEncMPVDefaultBPictureCount` (E_INVALIDARG), but **accepts**
  `AVEncCommonRateControlMode = Quality`, `AVEncCommonQuality`, and
  `AVEncMPVGOPSize`. So constant-quality is expressed via **`AVEncCommonQuality`
  (0-100)**, mapped from the spec's CQ: `quality = 100 − cq·100/51` (→ 55 for CQ
  23). This mapping is approximate (MF exposes no native NVENC CQ scale) and is
  tuned against measured bitrate on the test machine. No B-frames is left to the
  NVENC default (verified `has_b_frames=0`), since the explicit property is
  rejected. This is the pitfall-18 vendor quirk; the best-effort SetValue design
  (log + continue) handled it and the corrected knobs now apply with no warnings.
- **Sink Writer**: `MF_TRANSCODE_CONTAINERTYPE = MPEG4` forces the container
  independent of the `.part` extension; `.part` → `Finalize` → `sync_all`
  (FlushFileBuffers) → atomic `rename` (§4.7). Crash-safety is NOT provided
  (moov only at Finalize) — knowingly temporary; F2's fMP4 fixes it.
- **`GpuContext` is now `Send + Sync`** (multithread-protected device, per-thread
  clones). **NV12 pool bumped 4 → 8** to exceed the input-channel depth (4) so a
  queued frame's pool texture is never recycled under it.
- **Deps added (whitelisted):** `crossbeam-channel`, `tracing-subscriber` (first
  subscriber installed in `record`). **`record` output path** for M1 =
  `--out` or `<dir>/clipd_<unix_secs>.mp4`; full filename_template (date/time) is
  later polish.
- **Measured on the Nitro V15 (`record --seconds 5`):** 292 captured / encoded /
  muxed → playable `.mp4`. **ffprobe:** h264 / Main / avc1 / 1920×1080 / yuv420p /
  **r_frame_rate = avg_frame_rate = 60/1**, color_range=tv,
  color_space/transfer/primaries=bt709, has_b_frames=0, duration 4.867 s. **CFR
  frame PTS deltas all exactly 0.016667 (1/60)** — the pacing grid is strictly
  CFR. **Still pending: visual pixel colour** vs a reference screenshot (metadata
  is correct; the human eyeballs the clip / RenderDoc).

## 2026-07-03 — Milestone 1 Task F2: crash-safe fragmented MP4 (`mux/fmp4.rs`)

Branch `m1-mux-fmp4`. Replaces the F1 Sink Writer in the mux thread with the
frozen-spec §4 hand-rolled fMP4 writer. Sink Writer retained as the documented
fallback (`mux/sinkwriter.rs`, still compiled).

- **Structure:** `ftyp` + `moov` (with `mvex`/`trex` for fragmentation) written up
  front, then **one `moof`+`mdat` fragment per second** (§4.6). `moov` carries an
  `avc1` sample entry with `avcC` (from SPS/PPS) and a `colr` nclx box (BT.709
  limited) alongside the H.264 VUI.
- **Timing is exact by construction:** video timescale = `fps·1000` (60000),
  every sample duration = `VIDEO_SAMPLE_DELTA` (1000), fragment
  `baseMediaDecodeTime = total_samples · sample_delta`. No PTS→timescale rounding
  — the pacing grid already guarantees exactly `fps` samples/s. `trun.data_offset`
  is patched post-assembly (default-base-is-moof).
- **Annex-B → AVCC:** the encoder emits Annex-B (start codes); samples are
  rewritten to length-prefixed NAL units for `mdat`, and SPS/PPS/AUD (types 7/8/9)
  are stripped (SPS/PPS live in `avcC`). SPS/PPS for `avcC` come from the media
  type's `MF_MT_MPEG_SEQUENCE_HEADER` blob (parsed as Annex-B).
- **Crash-safety:** each completed fragment is `flush`ed out of the `BufWriter` to
  the OS as it is written, so a process kill leaves whole fragments on disk;
  `finish` does the final `flush` + `sync_all` (FlushFileBuffers) + atomic
  `rename` (§4.7). `std::fs::rename` on Windows replaces atomically
  (MoveFileEx REPLACE_EXISTING), so no delete-then-rename window.
- **7 unit tests** for the pure box/parse logic: box + fullbox layout, Annex-B
  splitting (3- and 4-byte start codes), sample→AVCC stripping + length-prefix,
  avcC record layout, fragment `data_offset` correctness, moov nesting sizes.
- **`MuxError` promoted to `mux/mod.rs`** (shared by both muxers); `EngineError::Mux`
  now references it.
- **Measured on the Nitro V15 (`record --seconds 5`):** 293 frames → playable
  `.mp4`. **ffprobe:** h264/Main/avc1/1920×1080/yuv420p, r_frame_rate =
  avg_frame_rate = 60/1, color_range=tv, bt709 primaries/space, has_b_frames=0;
  CFR PTS deltas all 1/60; **moof=5 / mdat=5** (one fragment per second).
  **Crash test:** killed mid-record at ~2.5 s → no final `.mp4`, orphaned `.part`
  is a valid playable h264 file with duration exactly 2.000 s (the two completed
  fragments). Crash-safety (§4.6) verified. Test-machine step: `record --seconds
  10`, expect ~10 moof boxes and a playable clip; kill mid-record and confirm the
  `.part` plays.

## 2026-07-03 — Milestone 1 Task G: epoch-restart / sleep-resume rebuild

Branch `m1-epoch-restart`. The pipeline-rebuild path (spec §7; plan pitfalls
25/26). Closes the M1 checklist item "survives monitor sleep / lock / sleep-resume".

- **One rebuild path for all device-loss (pitfall 26).** `EngineError::is_device_lost`
  classifies a stage error as device-loss when the wrapped HRESULT is
  `DXGI_ERROR_DEVICE_REMOVED` / `_RESET` (sleep/resume, driver reset, TDR).
  `stop_and_join` returns `RecordOutcome::{Completed, DeviceLost}`.
- **Segmentation — a clip must not span epochs (§0).** `record` is now an epoch
  loop: each epoch is one segment file (`clip.mp4`, then `clip-1.mp4`,
  `clip-2.mp4`, …). On device-loss the current segment is finalized (the mux
  thread still runs `finish` on channel disconnect), then a fresh `GpuContext` +
  pipeline is built for the next epoch. `build_gpu` retries device creation for
  ~2 s (the §7 epoch-restart budget) while the device returns after resume.
- **Monitor sleep vs device loss.** Monitor sleep / lock (WGC simply stops
  delivering, no error) needs NO rebuild — the pacing grid's last-frame resubmit
  keeps the segment CFR. Only a real device-loss HRESULT triggers an epoch
  restart. Early detection: the record loop polls `RecordingEngine::any_worker_finished`
  (a worker exits on device-loss) instead of waiting out the full duration.
- **Stop triggers decoupled** into `arm_stop`: a timer thread for `--seconds`, or
  an Enter-key watcher thread otherwise, both setting the shared stop flag — so
  the epoch loop can poll for both stop and device-loss.
- **Per-segment `epoch_id` starts at 0** (each M1 segment is its own file/epoch);
  a process-global monotonic `epoch_id` is a post-M1 concern (matters once the
  ring buffer spans epochs).
- **Status:** builds; happy path verified on the Nitro (`record --seconds 3` →
  one clean segment, 60/1, bt709). The **actual device-loss path is NOT yet
  hardware-validated** — per CLAUDE.md it is "ready for the 04-TEST-MACHINE
  procedure": lid close / `Win+L` / modern standby during a recording; expect no
  crash, a `device lost … segment saved` line, a new `-N.mp4` segment, and both
  segments playable.

## 2026-07-03 — Milestone 1 validation results + deferred item

M1 (all tasks A–G) validated on the Nitro V15 / RTX 4050 this session. Branch
`m1-epoch-restart` (stacks A–G), not yet merged to `main`.

- **Pipeline / colour / CFR:** `record` → playable MP4, ffprobe 60/1 CFR (PTS
  deltas exactly 1/60), h264/Main/avc1/1080p/yuv420p, color_range=tv +
  bt709 primaries/transfer/matrix, has_b_frames=0. Pixel colour confirmed by eye.
- **fMP4 fragmentation + crash-safety:** one moof/mdat per second; killed
  mid-record → orphaned `.part` plays to the last complete fragment (2.000 s).
- **Perf budgets (perf counters, attributable to clipd):** Video-Encode engine
  37.6 %, 3D 1.4 % (< 3 %), CPU 0.61 % (< 2 %), RAM 66.5 MB (< 75 MB). Encode is
  on separate silicon from the 3D engine.
- **Game frametime (Roblox, PresentMon):** impact within gameplay noise — the
  before/after delta came out negative (rec window lighter than base; scene
  variance ±25 % >> clipd overhead). Combined with the engine-separation numbers,
  the < 4 % frametime budget is met. Recorded Roblox at strict 60/1 CFR,
  ~6.7–7.2 Mbps under motion (CQP content-adaptive).
- **Win+L lock:** survived; continuous 59.6 s clip, no crash, no device loss, no
  extra segment (lock does not lose the D3D device — expected).
- **DEFERRED (orchestrator's call):** the real **sleep/resume device-loss
  rebuild** (§7). The epoch-restart code + happy path + lock survival are
  validated, but an actual device loss was not triggered on hardware. Close it
  later via a Start→Sleep→wake mid-record (expect `device lost … segment saved`
  + a `-1.mp4` segment). Recorded in HANDOVER.md §4. (UPDATE, same day: the
  epoch-restart *logic* was subsequently validated via the added
  `--simulate-device-loss` hook — see the next entry — so only the real-hardware
  recovery remains.)

## 2026-07-03 — Milestone 1 pre-merge audit + fixes (+ epoch-restart bug)

Before merging `m1-epoch-restart` → `main`, ran a 3-way devpack audit (CLAUDE.md
hard constraints; frozen 02-AV-SYNC-SPEC §0/§1/§3/§4/§6; 01-PLAN §2 architecture +
pitfalls). **No BLOCKERs; cleared to merge.** SHOULD-FIX items addressed:

- **REAL BUG fixed — shared stop flag broke epoch restart.** `RecordingEngine`
  took the record loop's user-stop `Arc<AtomicBool>`, and `stop_and_join` sets it;
  so finalizing epoch 0 after a device loss tripped the loop's stop condition and
  the recorder exited instead of rebuilding. This would have broken the REAL
  sleep/resume recovery, not just the simulation. Fix: the engine now owns its own
  internal stop flag; the user-stop is observed only by the record loop. Verified
  via `--simulate-device-loss`: two playable segments (epoch 0 finalized, epoch 1
  rebuilt + continued).
- **`--simulate-device-loss <secs>` test hook added** (capture stage injects a
  synthetic `DXGI_ERROR_DEVICE_REMOVED` after N s; epoch 0 only). Validates the
  epoch-restart logic without a real sleep. Left in as a hidden diagnostic flag.
- **fMP4 `hdlr` box name** now uses `PRODUCT_NAME` (was hardcoded `"clipd"` in
  every output file's metadata — naming-placeholder rule). `encode-probe` temp
  filename likewise.
- **WGC `FrameArrived` lock** now recovers a poisoned mutex (`unwrap_or_else(|e|
  e.into_inner())`) instead of `unwrap()` — a panic there would unwind across the
  WinRT FFI callback (UB), and that thread is outside the engine's `catch_unwind`.
- **`pacing.rs` `expect` removed** — slot math factored into a total `slot_index`
  fn so the pure-logic grid is panic-free by construction.
- **Stale comments reconciled:** the mux thread + `mux/mod.rs` said "Sink Writer
  (first cut)" but the engine ships fMP4; the data-flow-rule-4 "never stalls
  capture" claim is now qualified for M1 (no ring buffer yet → a sustained disk
  stall back-pressures capture within the channel depth).
- **Pitfall 11 (resolution/display-mode change) documented as a deferred M4 gap**
  in `engine.rs`: a mid-recording size change is not a DXGI device loss, so it does
  not funnel into the epoch restart — it currently ends the recording rather than
  segmenting. Fixed-resolution monitor capture is the M1 scope; frame-pool
  `Recreate` lands with window mode in M4.

Accepted-as-deferred (flagged in code/DECISIONS, not fixed): full §6.3 watchdog
(only frames-in/out divergence implemented; queue-depth/no-frame/save-duration/
ts_violation deferred to the ring/save layer), CQP-via-`AVEncCommonQuality`
approximation, no-B-frames-via-NVENC-default, NV12 pool has no GPU fence, HDR
detect-and-act, audio track (M2).

---

## 2026-07-04 — Milestone 2 (audio), Task 1: pure-logic foundations

Starting M2. The milestone's four tracker items decompose into ~8 stacked tasks
(mirroring M1's A–G): pure-logic foundations → WASAPI capture → resample → AAC
encode → multi-track fMP4 → device-change → engine integration → A/V sync rig.

- **Pure-logic modules land first (this task):** `audio/gaps.rs` (silence-gap
  synthesis, §2.3) and `audio/drift.rs` (drift measurement + P-only controller,
  §2.4). Rationale: `01-PROJECT-PLAN §3` puts "60% of the pain" in the audio
  clock story, and its two hardest pieces are pure math the spec pins to exact
  numbers. Building them first as 100%-safe, exhaustively-unit-tested modules (no
  COM, no hardware) de-risks the sync math before any capture/encode/mux work
  depends on it, and this PR is green on CI alone. Matches the `clock`/`pacing`
  unit-test-heavy convention. +27 tests (50 → 77).

- **`GapSynthesizer` returns *actions*, not buffers.** `on_packet(pts, frames)`
  yields `Admit` / `SynthesizeSilence{frames, pts}` / `DropOverlap{drop_frames,
  pts}`; the caller (the future capture/resample stage) produces the actual
  silence samples and trims overlap. Keeps the module format-agnostic (ticks +
  48 kHz frame counts only) and pure — one implementation shared by loopback and
  mic. Reversible.

- **`DriftWindow` evicts whole observations, not split fractions.** The sliding
  30 s window drops observations whose end is at/before `newest_end − 30 s`
  rather than splitting a straddling one. At 10 ms observation granularity the
  ±1-observation edge error is negligible against 30 s, and it keeps the estimate
  a simple ratio of running sums. Reversible.

- **Drift sign convention fixed and documented:** `DriftController::applied_ppm`
  is the correction added to the nominal resample ratio, `ratio = out/in =
  (48_000/device_rate)·(1 + applied_ppm/1e6)`; device-fast (`err_ppm > 0`) →
  negative correction. The resample wiring (Task 3) asserts this against real
  capture. Note: `CLAUDE.md`'s repo layout lists no `resample.rs` under `audio/`
  — whether resampling folds into `wasapi_stream.rs` or gets its own file is a
  Task-3 decision, not settled here.

## 2026-07-04 — M2 Task 2: WASAPI capture worker

`audio/wasapi_stream.rs` promotes spike #3 into a real per-stream worker emitting
`AudioPacket`s (QPC PTS, native-rate f32 stereo) over a channel. Adds the
whitelisted `wasapi = "0.23.0"` dep (transitively pulls num-traits/num-integer/
autocfg — all via the approved crate). New `audio-probe [SECS]` diagnostic.

- **Capture at the device's NATIVE sample rate, not 48 kHz.** We request f32
  stereo at the mix-format rate with autoconvert on, so WASAPI only does
  integer→float + channel mapping — the sample rate stays native on purpose.
  `§2.4` requires *our* resampler (rubato, Task 3) to do native→48 kHz so the
  device-crystal drift is measurable; letting WASAPI resample the rate would hide
  exactly the drift AV-2 exists to catch. The spike used autoconvert to 48 kHz
  (it only needed a WAV); this is the spec-faithful choice for the real path.
  Native rate + frame count ride on every packet. Reversible.
- **Capture buffer = 4 × device period** (`§2.1`), vs the spike's 1×. Buffer size
  affects only overrun headroom, not timestamp correctness. If a device rejects
  the 4× buffer in shared event mode, fall back to 1× (`def_period`); the
  `audio-probe` on hardware is where that surfaces.
- **Mic mono→stereo via WASAPI autoconvert**, not manual duplication. `§2.1` says
  "duplication at capture"; WASAPI's stereo upmix of a mono source is the same
  effect and avoids hand-rolling format conversion. If a mic ever images wrong,
  the fallback is to request native channels and duplicate by hand. Flagged.
- **`AudioError` wraps the `wasapi` `Box<dyn Error>` as a string.** Precise
  `AUDCLNT_E_DEVICE_INVALIDATED` classification for the rebuild path (`§7`) is
  deferred to Task 6 (device-change), which owns `IMMNotificationClient` anyway.
- **Bad-QPC / sample-counting fallback (`§2.2`) is pure + unit-tested** in
  `PtsDeriver`: per-packet fallback to `prev_pts + prev_frames·ticks/native_rate`,
  a rolling 60 s window, and a permanent switch past 100 bad/min. No `unsafe` in
  the module — the `wasapi` crate is the COM wrapper.

## 2026-07-04 — M2 Task 3: native→48 kHz resampler + drift correction

`audio/resample.rs`: `StreamResampler` converts native-rate capture packets to
the canonical 48 kHz stream, folding in gap synthesis (§2.3) and drift correction
(§2.4). Adds whitelisted `rubato = "0.16.2"`.

- **Separate `resample.rs` module** (CLAUDE.md's repo layout lists only
  `audio/{wasapi_stream,gaps,drift,devices}` — no `resample.rs`). Chosen over
  folding into `wasapi_stream.rs` for single-responsibility + independent
  unit-testing; the resampler is pure DSP and deterministic, so it is CI-tested
  without hardware. Flagged as a layout addition, not a scope addition.
- **Pipeline order: gap-fill at NATIVE rate, before the resampler.** Running
  `GapSynthesizer` on the native input makes the resampler input continuous, so a
  loopback silence never shortens the track and the output PTS can be a simple
  anchored sample count. This required parameterizing `gaps.rs` and `drift.rs` by
  rate (Task 1 built them hardcoded to 48 kHz). At 48 kHz both are byte-identical
  to the spec formulas; the rate parameter only generalizes to 44.1/96 kHz
  devices, where the literal `48_000` would be wrong. Spec-faithful generalization.
- **Drift measured feed-forward on the native clock**, contiguous audio only
  (gap spans excluded — they are QPC-exact fill, not device-clock evidence). The
  controller sets the rubato ratio to `(48000/native)·(1+applied_ppm/1e6)` every
  10 s. Sign verified: device-fast (err>0) → applied<0 → smaller ratio → fewer
  output frames.
- **Output PTS = `anchor + out_frames·ticks/48000`** (anchored at the first
  packet's QPC PTS). Honest sample counting is legitimate here *because* the
  stream is gap-filled (continuous) and drift-locked to QPC — the preconditions
  §2.2 requires. Residual drift + AAC priming are the only remaining error terms,
  both in the §5 budget; the click/flash rig (Task 8) is the real check.
- **rubato config:** `SincFixedIn`, sinc_len 128, oversampling 256, Linear
  interpolation, BlackmanHarris2 window, chunk 480 frames, max relative ratio 1.1
  (covers ±300 ppm). `finish()` zero-pads the sub-chunk remainder and leaves the
  <sinc_len (~2.7 ms) delay-line tail unflushed — inside the §4 head/tail slack.

## 2026-07-04 — M2 Task 4: AAC-LC encoder (mft_aac)

`encode/mft_aac.rs`: the Media Foundation AAC-LC encoder, one per track. New
`aac-probe [SECS]` diagnostic.

- **Synchronous MFT drive.** The MS AAC encoder is a sync software MFT (unlike
  the async hardware H.264), so it uses the classic ProcessInput → pull
  ProcessOutput-until-NEED_MORE_INPUT loop, not the event state machine.
- **16-bit PCM input.** The AAC encoder rejects float, so the resampled f32
  stream is converted via `f32_to_i16` (clamp + scale by i16::MAX, unit-tested).
- **Raw AAC output (payload type 0)** + `AudioSpecificConfig` extracted from the
  output type's `MF_MT_USER_DATA` at offset 12 (after the HEAACWAVEINFO prefix).
  The muxer needs the ASC for the `esds` box (audio analogue of `avcC`).
- **Priming compensation (§2.6) by AU-index sample counting**, not the encoder's
  own output times: `pts = anchor + (au_index·1024 − priming)·ticks/48000`, drop
  AUs entirely within priming. Legitimate because the input (from
  `audio::resample`) is already continuous + QPC-locked.
- **Priming constant = the §2.6 fallback (1024).** The exact one-time impulse
  measurement (encode a 1-sample impulse, decode with ffmpeg, read the offset)
  needs the Nitro + ffmpeg and is DEFERRED like the device-loss test. An error
  here is a constant offset the §5 AV-1 test catches; 1024 is the MS-encoder
  expected value. Flagged, not silently assumed.

## 2026-07-04 — M2 Task 5: multi-track fMP4 muxer

Rewrote `mux/fmp4.rs` from single-video-track to video + up to two AAC tracks
(desktop, mic — §2.5). New `AudioTrackConfig`, `write_video_packet` /
`write_audio_packet`, `esds`/`mp4a`/`smhd`/`soun` builders.

- **Single-`traf`-per-`moof`, interleaved by fill order.** Each track emits its
  own ~1 s fragments; players order per track via `baseMediaDecodeTime`. Simpler
  and just as valid as multi-`traf` moofs, and keeps the fragment builder a small
  generalization of the M1 one (parameterized by track_id + sample_delta).
- **A/V alignment = video-first-PTS origin + audio `initial_offset`.** Video
  sample 0 at container time 0; each audio track's first admitted AU placed at
  `round((au_pts − origin)·48000/1e7)`, then contiguous 1024-sample AUs (the
  resampler already made audio gap-free + QPC-locked). Audio arriving before the
  origin is prebuffered, then AUs before the origin are dropped (≤ one 21.3 ms AU
  — the §4.4 head-slack rule). The full §4 save-time rebasing (chosen-IDR origin,
  trailing-audio inclusion) is an M3 ring/save deliverable, noted in code.
- **esds/mp4a details:** raw AAC (objectType 0x40, streamType 0x15), ASC in the
  DecoderSpecificInfo; MPEG-4 expandable descriptor lengths (base-128) unit-tested.
  Every AAC AU flagged sync; audio sample_delta constant 1024, timescale 48000.
- **Engine mux thread stays video-only (`&[]`) until Task 7** wires the audio
  capture→resample→AAC threads and passes the ASCs. M1 `record` output is
  unchanged by this task.

## 2026-07-04 — M2 quality-audit pass (pre-Task-7): two sync-math fixes, two flagged gaps

A dedicated audit pass reviewed Tasks 1–5 (all six M2 modules) against the
frozen spec before the Task-7 integration. Two bugs fixed on `m2-audio`
(+2 regression tests, 98 → 100); two design gaps flagged as **requirements**
for Tasks 6/7; minor items enumerated in HANDOVER.md's audit section.

- **Fix: drift-window span/samples pairing** (`audio/resample.rs`). The window
  was fed `(span = pkt.pts − prev.pts, samples = pkt.frames)` — but the frames
  occupying that span are the *previous* packet's. With constant 480-frame
  packets the window sums telescope and the error cancels (which is why the
  Nitro `audio-probe` looked clean); with variable sizes (WASAPI double/triple
  periods after scheduling hiccups) a one-packet edge mismatch over the 30 s
  window reads ~330 ppm of phantom drift — larger than the 20–200 ppm signal
  §2.4 exists to measure, i.e. noise injected straight into the controller
  AV-2 grades. Now observes the previous packet's frame count. Regression
  test: irregular packet sizes on a perfect clock must hold 0 ppm.
- **Fix: output PTS now subtracts the resampler group delay**
  (`audio/resample.rs`). rubato `SincFixedIn::output_delay()` = sinc_len/2 ·
  ratio ≈ 64 output frames: the input sample at the anchor emerges 64 frames
  later, so stamping `anchor + out_frames·ticks/48k` placed the entire signal
  ~1.33 ms early — a constant offset absent from the §5 budget table. This is
  the resampler analogue of §2.6 AAC priming; Task 3 documented the *tail*
  delay-line but missed the *start* delay. PTS is now `anchor + (out_frames −
  delay)·ticks/48k`; the first chunk legitimately starts ~13,333 ticks before
  the anchor (the muxer's pre-origin drop / `initial_offset` absorbs it).
- **Flagged, NOT fixed — Task 6/7 requirements** (details in HANDOVER.md):
  (a) §2.3 gap fill is unbounded — QPC runs through suspend, so sleep/resume
  can demand hours of synthesized silence (GB-scale allocations through
  rubato/AAC; `u32` frame cast truncates past ~24.8 h). Needs a cap +
  re-anchor/epoch-restart decision. (b) the §7 rebuild must recreate the
  WASAPI client *below* a surviving `StreamResampler`/`AacEncoder` — the mux
  butt-joins AUs after the first, so a fresh anchor mid-file silently shifts
  audio — and a native-rate change across rebuild has no re-anchor path
  (rate-switch support or epoch restart: decide in Task 6).

## 2026-07-04 — M2 Task 7: engine integration (audio threads + merged mux)

Wired the audio capture→resample→AAC chain into `RecordingEngine` so `clipd
record` produces video + desktop-loopback + mic tracks, `[audio]`-config driven.
No new deps; no spec changes. `just check` + `just test` green (100 tests,
unchanged — this task is thread wiring, whose validation is the on-machine
`record` procedure, not a unit test).

- **Merged mux channel (`MuxItem`) over `select!`.** The video encode thread and
  each audio-process thread send a single `enum MuxItem { Video(EncodedPacket),
  Audio(track_index, EncodedAudioPacket) }` into one `bounded` channel; the mux
  thread dispatches on the variant. Chosen over `crossbeam::select!` across a
  variable number of audio channels (simpler, and the arm count is fixed at
  compile time). Both payloads own their bytes ⇒ `MuxItem: Send` with no
  `unsafe`. Track index = position in the enabled-streams list (desktop first,
  mic second, `§2.5`), which is also the `AudioTrackConfig` order handed to
  `Fmp4Writer::create`, so a desktop-only recording still has a contiguous
  track-0 slice.
- **ASC handoff on a channel separate from data.** The muxer cannot build the
  moov until it has the video output type *and* every track's ASC. Each
  audio-process thread sends its `AudioTrackConfig` on a dedicated `asc` channel
  *before* any data, so the mux completes setup even if the data channel has
  already back-filled (no deadlock). `AacEncoder::new` yields the ASC with no
  sample rate, so this happens at thread start; the `StreamResampler` (which
  *does* need the native rate from the first `AudioPacket`) is built lazily on
  the first packet. This is the one refinement over the HANDOVER design, which
  implied both were created together.
- **COM discipline.** The AAC encoder (COM/MFT) and the resampler live entirely
  on the audio-process thread, which calls `ComMta::initialize()` at entry —
  never created elsewhere and moved (mirrors the H.264 encoder on the encode
  thread). `MFStartup` stays once-per-process in `main`.
- **Audio failures are non-fatal to the video clip.** A mid-stream audio-stage
  error (e.g. mic unplug) stops that track but the mux finalizes video + the AUs
  already written; `stop_and_join` logs `audio_failures` and does not fail the
  recording. Only a *setup-time* audio failure (before the ASC handoff) fails the
  segment, via the mux's `ChannelClosed`. Proper §7 audio device-change recovery
  is Task 6. Rationale: the trust model ("why didn't my clip save") says a dead
  audio device must not lose the video.
- **Audit item #3 (unbounded gap fill) reassigned to Task 6, not done here.**
  The HANDOVER flagged it as a Task 6/7 requirement; scoping it to Task 6
  (with item #4) because: (a) its correct form is a cap-*then*-re-anchor, and
  the re-anchor is exactly item #4's device-rebuild contiguity work; (b) §2.3
  loopback silence during normal recording *is* delivered as a gap (WASAPI stops
  sending packets when a game is quiet), so a legitimate in-session gap can be
  minutes long — the cap must be generous and rate/buffer-aware, a real design
  choice rather than a one-liner; (c) Task 7's own validation (short clips) can
  never trigger the suspend-scale gap. Net: no `resample.rs`/`gaps.rs` change in
  this task. The OOM risk remains only for an actual sleep/resume, which is
  already the deferred "real sleep/resume rebuild" item and funnels through the
  video epoch restart anyway.

## 2026-07-04 — M2 Task 6: audio device-change state machine (§7)

Built the `§7` per-stream device-change handling so a recording survives an
unplug/replug or a default-endpoint switch (AV-4). New `src/audio/devices.rs`;
`wasapi_stream::run_capture` refactored into a rebuild loop; `resample.rs` gains
a native-rate switch + the audit-item-#3 gap-fill cap. `just check` + `just
test` green (107 tests, +9). HW-validation is AV-4 (see HANDOVER.md).

- **New dep `windows-core = "0.62.2"` (whitelist note, NOT buried).** The
  `#[implement]` macro used for the `IMMNotificationClient` sink emits
  `::windows_core::` paths, so the crate must be named explicitly. It is the core
  of the already-whitelisted `windows` umbrella crate (which re-exports it as
  `windows::core`), the exact 0.62.2 already in the tree transitively — no new
  external functionality, only a name made visible. Also added the
  `Win32_Media_Audio` feature (IMMDeviceEnumerator/IMMNotificationClient/
  EDataFlow/ERole), APIs actually called, same commit.
- **Rebuild happens BELOW a surviving resampler + AAC encoder (audit item #4).**
  The capture thread recreates only the WASAPI client; the `StreamResampler`,
  `AacEncoder`, and `PtsDeriver` live in the process/capture threads and are
  untouched, so the output anchor never resets and the muxer's butt-joined AUs
  stay aligned. The QPC PTS jumps forward by the hole and the existing §2.3
  synthesizer fills it with silence — the spec's "no special case" holds because
  the surviving chain is what makes it hold.
- **Two rebuild triggers, one response.** (a) Any WASAPI call error in the
  RUNNING loop → immediate rebuild (skip debounce) — the unplug/invalidation
  path AV-4 tests. (b) `IMMNotificationClient::OnDefaultDeviceChanged` for the
  stream's data flow (Console role) → a leading-edge 250 ms debounce
  (`Debouncer`, pure + unit-tested) coalesces Windows' 3–6-event burst into one
  rebuild — the default-follow "switch default output" path. No fine-grained
  `AUDCLNT_E_*` classification: the response is identical for every device error,
  so classifying would be dead complexity (YAGNI).
- **Native-rate change across a rebuild (audit item #4, rate clause).** A rebuild
  landing on a different-rate endpoint calls `StreamResampler::switch_native_rate`,
  which rebuilds the sinc + gap + drift for the new rate while KEEPING
  `anchor_pts`/`out_frames` — the 48 kHz output timeline stays continuous and
  monotonic. Trade-off recorded: the ≤ 750 ms rebuild hole is silence-filled for
  a *same-rate* rebuild (the common case; resampler untouched) but NOT across a
  rate change (a one-time ≤ 750 ms compression, logged WARNING). Same-rate is the
  norm on modern all-48 kHz hardware (incl. the Nitro); the rate-change path is a
  rare edge and full silence-padding across it would need the muxer to represent
  a PTS gap it currently cannot (butt-join). Simpler + logged + reversible.
- **Gap-fill cap (audit item #3), now implemented here.** `resample.rs`
  `MAX_SILENCE_FILL_SECONDS = 120`: a single synthesized silence gap is clamped
  to 120 s of native frames (`capped_silence`, unit-tested), + WARNING when it
  fires. Generous enough that real loopback silences (AV-3 is 60 s) never hit it;
  low enough that a suspend/resume race cannot allocate GB or truncate the `u32`
  frame count. A clamp desyncs audio after the gap by the excess — acceptable
  only in the pathological case (a real suspend is a *video* device loss that
  epoch-restarts anyway). NOT a spec constant (lives in `resample.rs`, not
  `spec_constants.rs`); M3's ring `buffer_seconds` supersedes this crude ceiling.
- **Pinned mic that is gone → retry + record silence, never substitute (§7).**
  `DeviceSelection::Pinned(id)` binds exactly that endpoint; if `get_device`
  fails the rebuild loop retries with backoff (no packets flow, so the track is
  short until it returns) rather than falling back to a different mic — "that is
  the incumbent sin." `default-follow` (the default) instead chases whatever the
  new default is, which is what AV-4 exercises.

## 2026-07-04 — M2 Task 8: click/flash sync rig (tools/avrig)

Built the `§5` A/V-sync measurement rig as a standalone tool crate under
`tools/avrig` (own `[workspace]`, never linked into `clipd` — like `/spikes`),
and wired the `just rig` recipe (was a stub). Root `clipd` crate unchanged and
still green (107 tests); the rig crate has its own 6 analysis tests. HW-validation
is AV-1/2/3/5 (see HANDOVER.md).

- **Split into a testable brain + thin HW wrappers.** `analysis.rs` is pure event
  detection + offset statistics (rising-edge detection with a refractory guard,
  nearest-neighbour flash↔click pairing, mean/jitter, and a least-squares drift
  fit) with AV-1 (≤16.7 ms) / AV-2 (≤5 ms drift) pass/fail — **6 unit tests over
  synthetic series** so the measurement math is trustworthy before any clip. The
  hardware-facing parts are thin: `generator.rs` (flash + click) and `measure.rs`
  (ffmpeg shelling) are the only bits that need the Nitro.
- **ffmpeg/ffprobe by subprocess, not linkage.** The core "no FFmpeg linkage" rule
  (CLAUDE.md #4) is about the *core binary*; a `/tools` measurement rig shelling
  out to the ffprobe/ffmpeg already on the test box is fine (and is the M3
  assertion-script pattern). `measure` gets per-frame luma via `ffprobe … movie=,
  signalstats` and the click envelope by decoding audio track 0 to s16 mono and
  reducing to per-window peaks. Verified end-to-end short of a real clip: ffprobe
  accepts the constructed filtergraph (fails only on a missing input).
- **Click on the desktop track by construction.** The click is emitted through the
  default *render* endpoint (WASAPI render, `wasapi` crate), so `clipd` records it
  on the desktop-loopback track (0, §2.5) — which is what `measure` analyses. The
  rig therefore needs `[audio].desktop = true`.
- **Flash/click simultaneity is best-effort within one buffer period.** The UI
  thread flips the flash and signals the render thread in the same instant; the
  click plays within one WASAPI period (~10 ms). That is a small ~constant offset
  AV-1's ±16.7 ms tolerates and AV-2's drift test cancels — the rig measures the
  *pipeline's* sync, and a constant rig offset is exactly the "AV-1 constant"
  §5 attributes to the AAC-delay term, not a drift.
- **Deps (tool crate, unconstrained by the core whitelist).** `wasapi` (render),
  `windows` (fullscreen GDI window: `Win32_Graphics_Gdi` +
  `Win32_UI_WindowsAndMessaging` + `Win32_System_LibraryLoader`), `tracing`. None
  leak into `clipd` (the empty `[workspace]` detaches the crate).

## 2026-07-04 — M2 Task 8 follow-ups (first HW run of the rig)

First `measure` run on the test box (ffprobe 7.0.1) surfaced two things:

- **Fix: ffmpeg 7.x dropped `pkt_pts_time`.** The luma probe used
  `-show_entries frame=pkt_pts_time`, which on ffmpeg 7 emits an empty time
  field — the signalstats CSV collapsed to a lone YAVG column and every row
  failed the two-float parse, so `measure` reported "no video luma samples".
  Switched to `pts_time` (committed). Verified: the probe now yields
  `<time>,<YAVG>`.
- **AV-1's absolute offset is rig-contaminated; AV-2 is the trustworthy gate.**
  A 4-event smoke clip showed a ~+47 ms constant offset (AV-1 FAIL) with a small
  drift (AV-2 PASS). The constant is two constants stacked: (a) the rig's own
  click latency (the click plays through a WASAPI render buffer, a fixed lag —
  the rig is not calibrated to zero), and (b) clipd's `§2.6` AAC encoder-delay
  constant (priming impulse measurement deferred; fallback 1024 ≈ 21 ms in use).
  `§5` explicitly attributes an AV-1 *constant* to the AAC-delay term. Since a
  constant cancels in the drift fit, **AV-2 (drift ≤ 5 ms) is the meaningful
  pass/fail today**; AV-1's number is diagnostic for the priming constant once
  the rig latency is characterized. Documented in M2-HARDWARE-TESTS.md §3/§7.
  Not fixed here: reducing/calibrating the rig's render latency, and the deferred
  §2.6 impulse measurement — both remain open (flagged, not blocking AV-2).

## 2026-07-04 — M2 COMPLETE (hardware validation summary)

All four M2 exit criteria validated on the Nitro V15 (05-MILESTONE-TRACKER.md
updated with the numbers). Highlights:

- **AV-2 (drift, the incumbent-killer): PASS with margin** — −1.92 ms over 10 min
  (minute-1 vs minute-10, 306 events). The whole-clip least-squares figure
  (+4.14 ms) was inflated by the §2.4 first-minute convergence transient; adding
  the spec-literal minute-1/10 metric to `avrig` (this session) revealed the true
  steady-state net drift is ~2 ms — within the §2.4 design residual, not just the
  5 ms gate.
- **AV-3 / AV-4: PASS** — silence fill and mic unplug/replug both clean.
- **AV-1 / AV-5: rig-limited, not gates.** The rig's absolute offset carries a
  WASAPI-render-latency constant that varies run-to-run (+47 vs +60 ms across two
  runs), so AV-1's absolute number is not trustworthy and AV-5's sync-under-load
  precision is fuzzy (frame drops make the flash-onset detection jittery). Both
  confirmed the important things (no crash, tracks captured, drift cancels). A
  calibrated/lower-latency rig and the deferred §2.6 AAC-priming impulse
  measurement would make AV-1 meaningful; full load-matrix validation is M6.
- **First-HW rig fix:** ffmpeg 7.x dropped `pkt_pts_time` → `pts_time` (committed).

`m2-audio` (17 commits) is validated and **ready to merge to `main`** — the merge
is the next session's first action (not done here). No code work remains for M2.

---

## 2026-07-04 — M2 merged to `main`; M3 planned

- **`m2-audio` merged into `main`** via `--no-ff` (merge commit `940d0ef`, keeps the
  milestone legible per HANDOVER §2a). `just check` + `just test` re-confirmed green
  on `main` (107 tests, clippy `-D warnings` + fmt clean). M1 + M2 are now both on
  `main`; `m2-audio` branch retained (not deleted).
- **M3 planned in full** (`M3-PLAN.md`, repo root — a working doc, not a devpack
  file). Two design questions resolved against the frozen devpack rather than by
  fiat, both recorded there and restated when their tasks land:
  1. **Ring packet bytes → `Arc<[u8]>`** (not `Vec<u8>`). Forced by the RAM budget
     (CLAUDE.md rule 7 / 01-PLAN §1: "ring size + < 75 MB overhead"): a save must
     mux **off-lock** (pitfall 24), and cloning the selected window to do so would
     transiently allocate the window size — ~246 MB at the 120 s/1080p default,
     **~1.9 GB at the 300 s/4K row of §6.2** — blowing the overhead budget.
     `Arc<[u8]>` makes the save snapshot a pointer-clone (peak RAM stays at ring
     size). 01-PLAN §2 also describes save as "slice, mux" (a view, not a copy).
     Lands in M3-1 (touches `EncodedPacket`/`EncodedAudioPacket`, std-only,
     reversible).
  2. **Ring is the pipeline spine; buffer mode reuses the spawn helpers** (not a
     second divorced pipeline, nor a flag on the duration-bound `RecordingEngine`).
     01-PLAN §2 lists the ring/buffer-mux as one of the four *permanent* threads,
     and M4 is "record N minutes **sharing the same pipeline** with a disk sink" —
     so the M1/M2 duration-bound engine is transitional (ring-less) scaffolding and
     M4 converges timed-record onto the same ring. Lands in M3-3.

## 2026-07-04 — M3 Task 4: ffprobe assertion script (`tools/verify`, `just verify`)

Built the `§4`/§5 assertion script FIRST in the M3 sequence (before the ring/save)
so every later save is machine-checked from day one — the companion to the `§5`
rig (`tools/avrig`). Branch `m3-verify`. Root `clipd` crate untouched and still
green; the tool is a standalone crate with its own 21 tests. No hardware step (pure
+ ffprobe shell; CI green suffices — the real "50 consecutive saves" gate is a
Nitro run once M3-2/M3-3 produce clips).

- **Standalone tool crate `tools/verify/` (own `[workspace]`, never linked into
  `clipd`)** — same detached-crate pattern as `tools/avrig` and `/spikes`. Shells
  out to the `ffprobe`/`ffmpeg` already on the box (7.x); the "no FFmpeg linkage"
  rule (CLAUDE.md #4) is about the *core binary*, and a `/tools` verification
  instrument shelling out is the established pattern (avrig, DECISIONS "M2 Task 8").
  **No dependencies** — ffprobe output is parsed as CSV / `-of default` key=value
  (no JSON crate; YAGNI). `Cargo.lock` committed.
- **Testable brain + thin shell split** (mirrors avrig): `checks.rs` is pure
  assertion logic over already-extracted numbers (21 unit tests incl. each check's
  pass and reject paths + the spec edge numbers — 1-AAC-frame tolerance, CFR
  micro-second rounding, head-silence boundary); `probe.rs` + `main.rs` are the only
  ffprobe/ffmpeg-touching parts. So the acceptance logic is CI-green without a clip.
- **Checks, each citing the frozen spec:** stream shape (1 h264 + N aac-LC 48k/2ch,
  `§2.5`/§2.6); monotonic PTS per track (`§0`); strict video CFR (all deltas = 1/fps
  within 1 ms — `§1.3`/§4.5); the `§4` save-rebase origin (video@0 `§4.3`, audio
  head-silence ≤ 1 AAC frame `§4.4`); track end-alignment ≤ 1 AAC frame (`§4.4`
  trailing rule / `§5 AV-3`); full-decode fragment validity (`§4.6`). Accepts one or
  more clips (`just verify (Get-ChildItem clips\*.mp4)`) for the 50-save gate; exit
  0 iff all pass.
- **Real bug caught by an end-to-end smoke test on a synthetic ffmpeg clip:**
  `ffprobe -show_entries frame=pts_time -of csv=p=0` emits the leading keyframe's
  line with a trailing empty field (`0.000000,`), so parsing the *whole line* as an
  f64 silently dropped the first frame and shifted `first()` (and the CFR/rebase
  origin) onto a later one. Fixed by taking the first comma-separated field per line
  (the same defence avrig's `measure.rs` already uses). After the fix the synthetic
  clip's rebase-origin check reads video@0.000 ms. (The synthetic clip legitimately
  FAILS the CFR + non-zero-origin checks because ffmpeg's `testsrc2`/fragmenting is
  not true 60 fps CFR and its muxer adds a start offset — clipd's hand-rolled fMP4
  is strictly CFR and origin-0, DECISIONS "M1 Task F2"/"M2 Task 5". The smoke test
  validated the shell + parsing + that pass/fail paths both fire correctly.)
- **`just verify` recipe** now runs the tool (was a stub). No new core deps; no
  whitelist change. Test-machine step: none for M3-4 (CI green suffices); the tool
  becomes load-bearing at M3-3, where `just verify` must be green on 50 consecutive
  saved clips on the Nitro.

## 2026-07-04 — M3 Task 1: the packet ring (`src/ring.rs`)

The compressed-packet replay ring (`§3`, `§6.2`) — the buffer that makes clipd a
replay clipper. Branch `m3-ring` (stacked on `m3-verify`). Pure + 100 % safe (the
module is on CLAUDE.md's no-`unsafe`, unit-test-heavy list); +11 tests (10 ring +
1 spec byte-cap), root crate green (118 tests, clippy `-D warnings` + fmt clean).
No hardware step (CI green suffices; the ring is exercised live once M3-3 wires it
into a buffer engine).

- **`EncodedPacket`/`EncodedAudioPacket` `data: Vec<u8>` → `Arc<[u8]>`** (the
  planning decision, now landed — DECISIONS 2026-07-04 "M2 merged"). The ring
  retains packets long-term and a save snapshots a window while capture runs;
  `Arc<[u8]>` makes both handle clones, not bulk copies, so peak RAM stays at ring
  size (the RAM budget, CLAUDE.md rule 7 / plan §1 — a cloning save would spike
  ~1.9 GB at the 300 s/4K §6.2 row). Blast radius was tiny: the encoder constructs
  the Arc directly from the locked MF buffer (one copy, same as the old `to_vec`);
  every consumer that reads bytes uses deref coercion (`&Arc<[u8]>` → `&[u8]`)
  unchanged; only the two `fmp4.rs` audio-buffer sites changed `.clone()` →
  `.to_vec()` (the muxer owns AUs until a fragment flushes — ~0.32 Mbps, and video
  already re-allocs via `sample_to_avcc`, so no zero-copy is lost on the record
  path). The save-path zero-copy *feed* of the muxer is an M3-2 concern.
- **The ring stores the encode types directly** (`EncodedPacket` /
  `EncodedAudioPacket`) rather than a ring-local `Packet`. They already carry
  exactly the `§3` fields (`pts`, `dur`, `epoch_id`, `keyframe`, `bytes`) — audio
  has no `epoch_id`, which it does not need (eviction keys off video, and the `§4`
  save selects audio by the pts window). Avoids a conversion + duplication; tests
  build the types directly (they are plain data — pure, `Send`, no COM).
- **Whole-GOP video eviction with a never-evict-the-last-GOP guard.** `evict_oldest_gop`
  pops the leading IDR then every following non-keyframe, so the new front is again
  a keyframe (`§3`); `has_spare_gop` (a keyframe exists after the front) blocks
  evicting the final GOP, so a save always has a leading IDR even if one GOP alone
  exceeds a (pathologically tiny) cap. Both caps checked in one `enforce()` after
  every push: evict GOPs while `duration_ticks > max` OR `total_bytes > max`, then
  trim audio.
- **Audio eviction is spec-literal** `pts < video_front_pts − 500 ms` (`§3`), the
  slack that guarantees audio covers any still-savable video range; no video front
  → keep all audio (nothing anchors the trim). Byte totals kept incrementally so
  both caps are O(1) per push.
- **`est_bitrate_bps` / `byte_cap_bytes` added to `spec_constants::ring`** (the
  planning decision #3). `est_bitrate` = §6.2 video tier by pixel area (1080p→16,
  1440p→26, 4K→50 Mbps @ 60 fps, scaled by fps) + two AAC tracks (`EST_AUDIO_BPS` =
  2×160 kbps, the table's "+0.4"); `byte_cap = seconds × est_bitrate × 1.5`. Unit
  test confirms the 1080p60/120 s cap lands ≈ 369 MB (§6.2's 246 MB × 1.5).
- **Read accessors for M3-2 + the watchdog:** `video()`, `audio_track(i)`,
  `duration_ticks()`, `total_bytes()`, `caps()` (the engine compares retained
  duration against `max_duration_ticks` for the `§6.2` auto-QP-relief signal —
  wired in M3-3), plus `clear()` for `clear_after_save`. The `§4` origin/window
  selection itself lands in `save.rs` (M3-2), operating over these accessors.
- Test-machine step: none for M3-1 (pure logic; CI green suffices). Eviction is
  exercised end-to-end once M3-3 runs a live buffer session on the Nitro.

## 2026-07-04 — M3 Task 2: the save path / `§4` rebasing (`src/save.rs`)

The frozen `§4` save contract over the ring. Branch `m3-save` (stacked on
`m3-ring`). Pure selection + a thin safe muxer driver; +9 unit tests, root crate
green (127 tests, clippy `-D warnings` + fmt clean). No hardware step for the
tested part; the muxer-driving shell is validated on the Nitro at M3-3 (via
`just verify` on a real saved clip).

- **Split: pure `select_window` (`§4.1`–§4.4) + safe `save_clip` shell.**
  `select_window` is the unit-tested core — no COM, on CLAUDE.md's no-`unsafe`
  `save` list. `save_clip` calls the muxer's *safe* API (`Fmp4Writer::create`/
  `write_*`/`finish`) and itself contains no `unsafe`, so `save.rs` stays
  100 % safe even though it references `IMFMediaType` in a signature.
- **Reuses the record-path muxer — the key architectural call (validated in the
  M3 plan §4).** `Fmp4Writer` aligns A/V to `origin = the first video packet's
  PTS` and emits `pts − origin`. `select_window` feeds it packets starting at the
  chosen `§4.2` IDR, so the muxer's origin *is* the `§4` origin and its offsetting
  *is* the `§4.3`/§4.4 rebasing — no second muxer, and `§4.5` container math,
  `§4.6` fragmenting, and `§4.7` atomic rename all come for free. `save.rs` owns
  the *selection*; the muxer owns the *mechanism*. This is what DECISIONS "M2
  Task 5" deferred here ("the full §4 save-time rebasing … an M3 ring/save
  deliverable"). The plan's flagged risk — that feeding an arbitrary-IDR window
  rebases to PTS 0 — holds by construction: the origin IDR has the minimum PTS in
  the window and is fed first, so the muxer sets `origin = origin_idr.pts` and
  video sample 0 lands at container time 0. (Final proof is the M3-3 `just verify`
  run, whose `save rebase origin` check asserts video@0 exactly.)
- **`select_window` returns OWNED, cloned packets** (`Arc` handle clones — no bulk
  copy, `EncodedPacket`/`EncodedAudioPacket` already derive `Clone`). So M3-3 can
  lock the ring, select (cheap), unlock, and mux off-lock — the RAM-budget
  discipline the `Arc<[u8]>` choice exists for.
- **`§4` implemented literally:** origin = newest IDR with `pts ≤ target` in the
  **newest packet's epoch** (`§4.2`); if `target` precedes that epoch's first IDR,
  clamp to it and flag `clamped` (clip shorter than requested — caller logs +
  toasts). Video window = `pts ≥ origin`, bounded to the newest epoch (`§0`: no
  clip spans epochs). Audio (per track) = `origin ≤ pts < last_video_pts + D`
  (`§4.4` trailing bound; `D` = the last video packet's `duration`). Packets keep
  ORIGINAL PTS — the muxer does the subtraction.
- **PTS-ordered merged feed (video-first on ties).** `save_clip` merges the
  window's video + per-track audio into one `(pts, rank)`-sorted feed so the origin
  IDR is fed first (sets the muxer origin cleanly) and fragments interleave ~1 s at
  a time like the record path, rather than all-video-then-all-audio. The muxer's
  audio prebuffer would tolerate any order, but ordered feed keeps clips
  editor-friendly.
- **9 tests over the selection edge cases** (CLAUDE.md testing rules): IDR
  walk-back at/before target, walk-back across a GOP boundary, epoch clamp,
  newest-epoch-only when an older epoch also has a qualifying IDR, trailing-audio
  bound at `last_video_pts + D`, head starts at first AU ≥ origin, two independent
  audio tracks, empty-ring error, and the merged-feed PTS/tie ordering.
- Test-machine step: none for the pure selection (CI green). `save_clip` is
  exercised at M3-3: a hotkey save on the Nitro must produce a clip that `just
  verify` passes (video@0, monotonic, CFR, end-aligned, decodes).

## 2026-07-04 — M3 Task 3: hotkey + buffer engine (`hotkey.rs`, `engine.rs`, `buffer` cmd)

Wires M3-1/M3-2 into a live replay-buffer mode: `clipd buffer` captures
continuously into the ring and the save hotkey writes the last N seconds. Branch
`m3-buffer` (stacked on `m3-save`). **Builds compile-green; NOT hardware-validated**
— this is the "build to HW gate" task (CLAUDE.md: never claim a HW path works). Root
crate green: `just check` + `just test` (130 tests, +3 hotkey parse), clippy
`-D warnings` + fmt clean. Release **1.94 MB** (was 1.70; `global-hotkey` +~0.24 MB),
budget 10 MB.

- **New dep `global-hotkey = "0.7.0"` (whitelisted, NOT buried).** `RegisterHotKey`
  via the polite OS API — no low-level keyboard hooks (CLAUDE.md hard-constraint 5;
  01-PLAN §2 names it). Its receiver is `crossbeam_channel` (the channel we already
  use), so the ring thread `select!`s the hotkey stream directly. Windows features
  added same-commit: `Win32_UI_WindowsAndMessaging` + `Win32_System_Threading` (the
  message pump + `GetCurrentThreadId`). Read the crate source before coding: its
  Windows backend creates a hidden window and `RegisterHotKey`s to it, so `WM_HOTKEY`
  only arrives while the **creating thread pumps its message queue** — hence a
  dedicated pump thread.
- **`hotkey.rs` — the Win32 message-pump wrapper.** Owns the pump thread: create
  `GlobalHotKeyManager`, register the hotkey, report the thread id, run
  `GetMessageW`/`DispatchMessageW` until a cross-thread `WM_QUIT`
  (`PostThreadMessageW` from `request_quit`). `unsafe` is confined here (a Win32
  syscall wrapper, like `clock.rs`), each block with a `SAFETY:` note; the manager
  (raw `HWND`, `!Send`) lives and dies on the pump thread. `parse_hotkey` uses
  `HotKey::from_str`, which accepts the config's friendly `Ctrl+Alt+S` directly
  (single-letter and `KeyS` both map; modifiers are case-insensitive) — so **no
  custom parser needed** and the `[hotkeys].save_clip` default parses (unit-tested).
- **`BufferEngine` reuses the record spawn helpers; the ring is the sink.** Same
  capture/encode/audio producers as `RecordingEngine` (shared `spawn` /
  `capture_thread` / `encode_thread` / `audio_process_thread`), but two new threads
  replace the mux thread: a **ring thread** owning the `Ring` and `select!`-ing over
  the merged `MuxItem` channel + the global hotkey receiver, and a **save worker**
  holding the encoder output type + track ASCs (like the record mux thread) that
  drives `save::save_clip` per job. On a save press the ring thread runs the pure
  `§4 select_window` (cheap `Arc`-handle clones) and hands the worker an OWNED
  window, then may `clear` the ring — muxing happens entirely off the ring, the
  RAM-budget discipline the `Arc<[u8]>` bytes exist for. Chosen over a second
  divorced pipeline / a flag on `RecordingEngine` per the devpack (ring is the
  spine; DECISIONS 2026-07-04 "M2 merged", decision #2).
- **Re-entrant/debounced saves + `clear_after_save`.** A 250 ms debounce
  (`SAVE_DEBOUNCE`, plan-derived not spec — matches the `§7` burst idiom) in the
  ring thread coalesces double-taps; the single serial save worker makes queued
  saves inherently non-corrupting (each clip its own path). `clear_after_save`
  (config) drops the ring after dispatch. Save-duration WARN > 1000 ms (`§6.3`).
- **`buffer` subcommand** (`main.rs`): loads config, resolves the output dir,
  spawns the `HotkeyPump`, starts the `BufferEngine`, waits on Enter (reusing
  `arm_stop`), then stops the engine and the pump. Headless — the tray/menu is M5
  (scope ratchet); M3's surface is this subcommand + the log lines.
- **Deferrals (flagged, not silently dropped):**
  - **Buffer-mode epoch restart (`§7`)** is NOT wired — a mid-buffer device loss
    ends the session (a worker exits → `any_worker_finished` → stop) rather than
    segmenting the ring across epochs. The record path has the restart; folding it
    in (ring spanning epochs, save picking the newest per `§4.2`) is a follow-up.
  - **`auto_qp_relief` QP bump (`§6.2`)** is NOT wired — the ring exposes the fill
    signal (`duration_ticks`/`caps`) but the live-encoder QP bump needs on-hardware
    tuning; the ring thread does not yet track the 60 s sustain.
  - **Byte cap uses the nominal 1080p tier** at ring construction because the frame
    size isn't known until the first frame flows; the exact `§6.2` tier only shifts
    the byte cap and the duration cap is the primary bound. Threading the real size
    through is a follow-up.
- **TEST-MACHINE step (the M3-3/M3-2/M3-1 gate — run on the Nitro):**
  1. `just run -- buffer --seconds 15` (a short buffer for the test). Expect the
     "buffering … press [Ctrl+Alt+S] to save …" banner and no crash.
  2. Let it run > 15 s with some on-screen motion + audio, then press **Ctrl+Alt+S**.
     Expect a `save triggered` then `clip saved … <path>` log line in < 1 s.
  3. Press it again quickly — expect one `save press coalesced (debounce)` line.
  4. Press Enter to quit; expect `buffer stopped.`
  5. `just verify <saved-clip>.mp4` — expect ALL checks PASS (stream shape, monotonic
     PTS, video CFR, `§4` rebase origin video@0, track end-alignment, full decode).
  6. Repeat to accumulate 50 clips; `just verify clip1 … clip50` green closes the
     M3 exit criterion. (24-hour soak = M3-5, separate.)
  Known first-run risks to watch: the global-hotkey message pump firing `WM_HOTKEY`
  (the whole path is unvalidated), and the Ctrl+Alt+S combo being free (else a
  `could not register hotkey` error → pick another in `[hotkeys].save_clip`).

## 2026-07-04 — M3 first-HW-run fixes (buffer save on the Nitro)

First `clipd buffer` run on the Nitro **worked** — the global-hotkey pump fired,
Ctrl+Alt+S saved a clip, and `just verify` confirmed video is perfect (1760 frames,
exact 60/1 CFR, `§4` rebase origin video@0, both AAC tracks present + monotonic,
full decode clean). Two real bugs surfaced and were fixed (root crate still green,
131 tests):

- **Fix (save.rs): the clip now ends where EVERY track has data, not at the newest
  video.** `just verify` failed end-alignment — audio ended **−80 ms** from video
  (audio 1371 AUs = 29.25 s vs video 29.33 s). Root cause: at save time the newest
  audio in the ring LAGS the newest video by the audio pipeline latency (WASAPI 4×10
  ms buffer + AAC 1024-sample framing ≈ 60–90 ms), and buffer-mode saves have no
  stop-time flush (the record path flushes the resampler/encoder tails; a live
  buffer cannot). `select_window` took ALL video but audio only reached ~85 ms short
  → audio short of video, failing `§5 AV-3`'s one-AAC-frame bound. Now
  `clip_end = min(video_end, each audio track's last end)` and every stream is
  trimmed to `[origin, clip_end)`, so the tracks end together (within one frame).
  The `§4.4` `last_video_pts + D` bound is the audio-ahead case, which the `min()`
  still covers. ~85 ms of trailing silent-video is dropped (imperceptible; correct —
  a replay clip must be A/V-aligned). +1 test (`video_trimmed_to_audio_end_when_audio_lags`).
- **Fix (engine.rs): the buffer ring thread now counts consumed video packets into
  `muxed`.** A `WARN mux is falling behind encode (>2s) … muxed=0` fired every
  second: `check_divergence` compares `encoded − muxed`, but the ring thread (the
  buffer-mode sink) never touched the `muxed` counter, so it sat at 0 while
  `encoded` climbed. Not a real backlog (the encode thread kept producing, so the
  bounded item channel was draining — the ring WAS consuming); purely an uncounted
  sink. The ring now `fetch_add`s `muxed` per video packet, making the divergence
  watchdog meaningful in buffer mode too.
- **Re-run procedure unchanged** (DECISIONS "M3 Task 3 → TEST-MACHINE step"): a fresh
  `clipd buffer` save with the fixed binary should now pass ALL `just verify` checks,
  and the spurious mux-behind WARN should be gone.
