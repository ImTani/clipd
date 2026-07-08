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

### Second-run refinement — retain one GOP of pre-roll margin

The re-run **passed all 8 `just verify` checks** (end alignment "video end 29.217s;
2 audio tracks within 21.33 ms"; no mux-behind WARN). But a `buffer --seconds 30`
save produced a **29.2 s** clip with a `clip shorter than requested … target
predates the current epoch's first IDR (§4.2)` WARN on every save.

- **Root cause (expected, not a bug):** a full-length save sets `target = now −
  buffer_seconds`, which lands on the ring's OLDEST edge. Whole-GOP eviction (§3)
  keeps ~buffer_seconds but the oldest retained IDR is usually a fraction newer than
  the target (the GOP straddling `now − buffer_seconds` was evicted), so
  `select_window` finds no IDR ≤ target and clamps to the epoch's first IDR — a
  ~1-GOP shortfall and a WARN on *every* save.
- **Fix (engine.rs):** the ring now retains `buffer_seconds + one GOP` (2 s default,
  1 s in `precise_mode`) — both the duration and byte caps use the padded length.
  This guarantees an IDR at/before `now − buffer_seconds`, so a full-length save
  yields ~buffer_seconds (up to §4.2's one-GOP pre-roll) with no clamp. `buffer_seconds`
  remains the SAVEABLE length; the margin is the standard replay-buffer difference
  between "hold N seconds" and "let me save N seconds ending at any frame" (OBS et al.
  do the same). Cost: one GOP of extra RAM (~2 s / 120 s = 1.7 %). The §4.2 clamp WARN
  now signals only a genuine shortfall (buffer not yet full, or a device-loss epoch
  boundary within the window). Slightly exceeds §3's literal `buffer_seconds` cap — a
  deliberate, reversible UX call recorded here, not a spec change.

### Soak (M3-5) — ~12 h partial run on the Nitro: no leak, saves stayed perfect

Ran `clipd buffer --seconds 30 --autosave 3600` for **~11.8 h** (707 one-per-minute
WorkingSet samples in `ram.csv`) rather than the full 24 h. Strong PASS signal on
both soak criteria:

- **RAM flat / no leak.** Trend **+0.22 MB/hour** (+2.6 MB over the whole run — noise
  within the working-set band); mean 45.8 MB; steady-state 30–66 MB (the 124 MB max
  is startup warmup); **last-hour avg 53.7 MB < first-hour avg 72.5 MB** (ends lighter
  than it started). A real ring/handle leak would climb tens of MB/hour. The shape is
  textbook: hourly dips to ~30 MB at each autosave (`clear_after_save` empties the
  ring → process floor → refills over 30 s); a benign working-set level-shift to a
  ~55 MB plateau mid-run (activity/CQP-bitrate change) that plateaus, not climbs.
- **Saves stayed correct throughout.** All **13** accumulated clips (hours 0–12,
  including the last at ~11.8 h) pass ALL 8 `just verify` checks (13/13). This is the
  "hour-N clip is perfect" half of the criterion, for 12 h.
- **Not yet closed (for the literal M3-5):** the full **24 h** duration, and ideally
  sampling **Private Bytes / commit** (WorkingSet is Windows-trimmed — a decent but
  not gold-standard leak metric) plus **handle count** (this run inferred "no handle
  leak" from flat RAM, not a direct handle sample). The 12 h WorkingSet result is
  strong preliminary evidence; a clean 24 h Private-Bytes+handles run formally closes
  it. Tracker M3-5 left unchecked pending that.

### 50-consecutive-saves criterion CLOSED — 73/73 on the Nitro

The orchestrator ran the save path to **73 consecutive saved clips** on the Nitro and
`just verify` passed **all 73** (all 8 checks each) — exceeds the 50-clip bar. Combined
with the 13 soak clips (all perfect, hours 0–12) this thoroughly exercises the `§4`
save path across content, timing, and two audio device configs. Tracker
"ffprobe assertion script green on 50 consecutive" checked off. M3 is merged to `main`
on this basis; only the full 24 h soak remains open (partial 12 h clean above).

## 2026-07-05 — M3-5 soak reclassified: acceptance item, not a milestone blocker

**Orchestrator decision:** the full 24 h soak is moved OUT of the M3 gate and INTO the
"run once everything is working" acceptance pass. It no longer blocks M4 or any
subsequent milestone.

- **Why.** The ~12 h WorkingSet soak already produced the load-bearing evidence: RAM
  trend **+0.22 MB/h** (flat, ends lighter than it started), 30–66 MB steady-state
  band, and **13/13** accumulated clips passing all 8 `just verify` checks (hours
  0–12). A ring/handle leak of any consequence climbs tens of MB/h and would already
  be unmistakable at 12 h. What the literal 24 h + Private-Bytes/HandleCount sampling
  adds is *formal closure and a gold-standard metric*, not new risk discovery — so it
  is a confirmation run, best done against a near-final binary, not a prerequisite for
  building the next feature.
- **What this changes.** Tracker M3-5 stays `[~]` (partial, 12 h clean) rather than
  blocking; the milestone is treated as effectively met (4/5 formally closed + soak
  partial-but-clean, consistent with the M3 merge to `main`). The 24 h run is folded
  into the pre-1.0 acceptance sweep (alongside the M6 hardware matrix), where a stable
  release-candidate binary makes the measurement meaningful. Procedure is unchanged —
  the `--autosave N` hook + Private-Bytes/HandleCount sampler from HANDOVER §2a.
- **Reversible / logged, per CLAUDE.md ambiguity rule 3.** Nothing about the ring or
  save path changes; this is purely a sequencing call. If any later soak or the 24 h
  run surfaces growth, it reopens immediately as a bug.

## 2026-07-05 — M4 planned (`M4-PLAN.md`); D1–D4 resolved against the devpack

M3 effectively met (soak reclassified above) → M4 opened. `M4-PLAN.md` (repo root)
mirrors `M3-PLAN.md`: scope, the substrate that already exists (`restart_epoch`, the
epoch-agnostic ring, `select_window`'s newest-epoch selection, the record epoch loop,
the already-present-but-unused `FocusedWindow`/`Monitor(index)`/`record_toggle`
config), four tasks (M4-1 window/target capture · M4-2 resize/close → buffer-mode
epoch restart + per-epoch save output type · M4-3 timed-record disk sink · M4-4 second
hotkey + docs), and the test matrix. The four design decisions resolve from the
devpack under the non-iterative contract (no orchestrator question needed):
- **D1 timed-record → tee off the ring to the existing `mux_thread`** — `01-PLAN §6 M4`
  ("sharing the same pipeline with a disk sink") + `§2` (ring is the spine) + logged
  M3 decision #2. Consequence: `RecordingEngine` becomes redundant — keep it through
  M4, retire in a later cleanup once the converged path is HW-validated.
- **D2 window close / exclusive-FS → fall back to monitor, new epoch, log** — pitfall 8
  + `§6 M4` + `§7` (buffer retained across a capture-target change).
- **D3 include `Monitor(index)`** — pitfall 31; the schema already ships it.
- **D4 cursor stays the explicit `cursor: bool` for M4; per-target `auto` tri-state
  deferred to the M7 settings** — pitfall 10's "expose as config" is met; the schema
  lacks an "unset" state and mid-milestone schema churn (pitfall 30) isn't worth it.

## 2026-07-05 — M4-1: window & target capture (`wgc.rs`, `engine.rs`, `main.rs`)

First M4 task, branch `m4-window-capture`. **Builds compile-green; NOT
hardware-validated** (the focused-window / monitor-index paths need the Nitro —
CLAUDE.md: never claim a HW path works). Root crate green: `just check` + `just test`
(**133 tests**, +2 for the target→source mapping), clippy `-D warnings` + fmt clean.
Release **1.95 MB** (was 1.94; +0.01), budget 10 MB.

- **`CaptureSource` — a config-agnostic capture descriptor in `capture/wgc.rs`.**
  `{ PrimaryMonitor, Monitor(u32), FocusedWindow }`. Chosen over reusing
  `config::CaptureTarget` so the capture layer never depends on the config schema
  (mirrors the audio precedent: `DeviceSelection` is built from config strings in
  `main.rs`, not imported into the engine). `main.rs::capture_source()` maps
  `CaptureTarget → CaptureSource` (total 3-arm match, unit-tested); config *parsing*
  of the string/int target forms was already tested in `config.rs`.
- **`WgcCapture::start(gpu, source, cursor)` — one entry point; shared
  `start_for_item`.** Refactored the M1 `start_primary` body into `start_for_item`
  (pool + free-threaded handler + session) and a `start` dispatcher that resolves the
  source to a `GraphicsCaptureItem`: `CreateForMonitor` (primary via
  `MonitorFromPoint`; index via `EnumDisplayMonitors`-order) or `CreateForWindow`
  (foreground HWND). `start_primary` kept as a thin wrapper for the existing probes.
  `capture_thread` (shared by record + buffer) now takes the `CaptureSource`; threaded
  through `RecordParams`/`BufferParams` and set in `main.rs` from `cfg.capture.target`.
- **Fallback-to-primary keeps the buffer alive (D2, pitfall 8).** `FocusedWindow`
  resolves `GetForegroundWindow` **once** at start (whatever is focused then) and
  falls back to the primary monitor (with a WARN) when there is no foreground window
  or `CreateForWindow` errors (uncapturable window). A `Monitor(index)` out of range
  likewise falls back + WARNs. True exclusive-fullscreen usually yields an HWND but
  delivers no frames — swapping *that* to the monitor is the M4-2 no-frame watchdog
  (`§6.3`), noted in code.
  - **Removed a broken self-added "don't capture my own terminal" guard.** An earlier
    draft skipped the foreground window when its PID == our PID. That check is dead
    code: a console app owns no top-level window, so `GetForegroundWindow` returns the
    **terminal's** window (a different process) and the PID never matches; under
    ConPTY/Windows Terminal there is no reliable child→terminal-window mapping at all.
    It was also a self-added extra beyond the devpack (CLAUDE.md scope discipline).
    Dropped it (and the `GetWindowThreadProcessId`/`GetCurrentProcessId` imports);
    `focused-window` now honestly captures whatever is foreground at start. The
    terminal-launch awkwardness is a known pre-tray CLI limitation the **M5 tray**
    resolves. (Still **no new `windows` features** — this removed two imports.)
- **`Monitor(index)` = `EnumDisplayMonitors` order (D3).** A small `enumerate_monitors`
  helper (a `MONITORENUMPROC` callback appending HMONITORs) indexes the OS monitor
  list. `unsafe` confined to this OS-wrapper module with `SAFETY:` notes (the callback
  runs synchronously on the calling thread; the `&mut Vec` outlives the call).
- **No new deps; no new `windows` features.** `GetForegroundWindow` /
  `GetWindowThreadProcessId` (`Win32_UI_WindowsAndMessaging`) and `GetCurrentProcessId`
  (`Win32_System_Threading`) came in with the M3 hotkey pump; `EnumDisplayMonitors` /
  `HDC` (`Win32_Graphics_Gdi`) and the WGC capture interop (`CreateForWindow`) were
  already present. `BOOL` is `windows::core::BOOL` in 0.62 (no `TRUE` const → `BOOL(1)`).
- **`window-capture-probe [SECS]`** (new subcommand, in `--help`): 3-s countdown → capture
  the focused window → report frames + size. The M4-1 HW checklist tool (mirrors
  `capture-probe`); run via `just run -- window-capture-probe` like the other probes
  (no new justfile recipe — consistent with the existing probe surface).
- **Banners are now target-aware** (`target_label`): record/buffer print "focused
  window" / "monitor N" / "primary monitor" instead of hard-coded "primary monitor".
- **Deferred to M4-2 (flagged, not silently dropped):** a mid-capture **resize**
  (`ContentSize` change → pool `Recreate`) or window **close** (`Closed` event) is not
  yet handled — it still surfaces as a stage error (pitfall 11, unchanged from M1).
  The epoch-restart that turns those into segment cuts, and the no-frame watchdog that
  swaps an exclusive-FS window to the monitor, are M4-2.
- **TEST-MACHINE step (run on the Nitro — the M4-1 gate):**
  1. `just run -- window-capture-probe 8` — during the countdown alt-tab to a
     borderless/windowed app (a browser, a windowed game). Expect
     `capturing focused window WxH …` with W×H = the **window** size (not the full
     1920×1080 monitor), then `delivered N frames … (fps)` with N > 0 and the size
     echoed. Keep the window active for a real fps.
  2. (No config exists by default — clipd never writes one.) Create
     `%APPDATA%\clipd\config.toml` with `[capture] target = "focused-window"`
     (`--check-config` prints the effective config to confirm).
  3. With that config, `just run -- buffer --seconds 15`. Buffer mode resolves the
     foreground **at start** (no countdown) — from a terminal that is the terminal
     window itself, which is fine: the point is the `§4` save path runs on a
     `CreateForWindow` source, not what's in frame. Let it run > 15 s, press
     Ctrl+Alt+S, Enter. Expect a saved clip; `just verify <clip>` — all 8 checks
     still PASS (the §4 save path is untouched under window capture).
  4. Set `target = 1` (a second monitor if present, else expect the out-of-range WARN
     + primary fallback) and `target = "primary"` — confirm each captures as labelled.
  Known first-run risks: a window that can't be captured (elevated/protected) → the
  fallback WARN + primary (correct, not a crash); exclusive-FS delivering 0 frames
  (expected until the M4-2 watchdog swaps it).

### M4-1 first-HW-run fix — odd window dimensions (NV12 needs even)

First `buffer` run with `target = "focused-window"` on the Nitro **crashed the
capture thread** immediately: `convert stage: … The parameter is incorrect
(0x80070057)`. Root cause: the focused window was **odd-sized** (a terminal ~1115 px
wide), and NV12 (4:2:0 chroma) — plus the H.264 encoder — require **even** width and
height. Monitor capture is always 1920×1080 (even), so M1–M3 never hit this; window
capture can be any size. A real, expected M4 bug (pitfall 11 neighbourhood), caught on
HW exactly as the process intends.

- **Fix (`convert.rs`): the converter rounds the output down to the largest even box
  and the video processor scales the (possibly odd) input into it.** `Converter::new`
  sets the VP content desc `Input = actual` capture size, `Output = (w & !1).max(2) ×
  (h & !1).max(2)`, and sizes the NV12 pool at the even output. At most a 1-pixel edge
  is scaled off — imperceptible. `dimensions()` now returns the even size.
- **Fix (`engine.rs`): the encode thread is handed the converter's EVEN output size,
  not the raw capture size.** `capture_thread` now builds the converter first, then
  `size_tx.send(converter.dimensions())`, so the encoder's `MF_MT_FRAME_SIZE` matches
  the NV12 frames it receives. (The encoder sets only `MF_MT_FRAME_SIZE` from these —
  no mod-16 assumption; NVENC pads internally + sets the SPS crop, so even is enough.)
- **Verified on the Nitro (RTX 4050), not just claimed.** New HW-gated test
  `convert::tests::odd_window_dimensions_convert_to_even_nv12` (`#[ignore]` — needs a
  GPU video processor; CI/`just test` skip it): `Converter::new(1115, 627)` →
  `dimensions() == (1114, 626)`, and the VP Blt of an odd BGRA input into the even
  NV12 output **succeeds**. Ran green here via `cargo test --lib --ignored`. The full
  window→encode→save chain at odd-derived dims is the orchestrator's `buffer` re-run.
- Root crate still green: `just check` + `just test` (**133** + 1 HW-skipped), fmt +
  clippy clean.

### M4-1 HW-run finding (DEFERRED, not M4) — mic-track startup head-silence on early saves

Verifying the M4-1 focused-window clips surfaced a **pre-existing** save-path edge (not
caused by M4-1; my changes are video-only). Of 7 ad-hoc test clips, **video is flawless
on all 7** (rebase@0, exact CFR, monotonic, full decode); 4 fail **only** the `§4.4`
audio-head-silence check, always on the **mic** track (`a:1`), by 28–63 ms (>1 AAC
frame of 21.33 ms). The desktop track (`a:0`) always passes.

- **Root cause.** All 4 failing clips are **shorter than the 15 s buffer** — saved
  *before the ring filled*, so `select_window`'s origin clamps to the epoch's first IDR
  ≈ **capture start**. The mic (WASAPI) delivers its first AAC AU 28–63 ms *after* the
  first video frame (device startup latency), so the mic track's head-silence exceeds
  one AAC frame. Jitters run-to-run (some early saves pass) — a startup race, not a
  systematic fault.
- **Why M3 never saw it.** M3's 73/73 used `--autosave 3600` on an always-full buffer,
  so the origin was never at capture start and the mic was long warmed up. Confirmed by
  contrast: a full-buffer M3 soak clip (`clipd_1783169494117.mp4`) **passes** the check
  cleanly (`audio head ≤ 21.33 ms`). It would fail identically on primary-monitor
  capture under the same "save within 15 s of a fresh start" conditions.
- **Deferred, out of M4 scope.** The clean fix is to **synthesize leading silence for a
  late-starting audio track at save time** (spec-consistent with `§2.3` gap synthesis —
  fill `[origin, first_track_pts)` with whole silence AAC frames so every track starts
  at the origin), or to accept it for origin-at-capture-start clips. This is an M2/M3
  audio-save-path refinement, **not** window mode / timed recording — logged here as a
  follow-up, not fixed under M4-1 (scope discipline). In normal continuous use the
  buffer is always full within N seconds of launch, so this only affects a clip whose
  window includes the very first ~1 buffer of a fresh session.

## 2026-07-05 — M4-2 CORE: buffer-mode epoch restart + device-loss trigger (self-verified on HW)

The core of M4-2 (`05-MILESTONE-TRACKER` M4: "window resize/close mid-buffer handled").
This turn builds the **epoch-restart machinery** and wires the **device-loss** trigger
(self-verifiable via the synthetic `--simulate-device-loss` hook); the window
resize/close + no-frame triggers ride the same machinery and are the next increment
(they need manual window interaction on HW). Also closes the deferred `§7` buffer-mode
device-loss restart (HANDOVER §2c) **and** M1's long-open sleep/resume path (HANDOVER
§5) by construction. Root crate green: `just check` + `just test` (**135** + 1
HW-skipped), clippy `-D warnings` + fmt clean. **`main` behaviour unchanged for the
non-restart path** (record + normal buffer save still green).

- **Persistent core vs rebuildable producers (`engine.rs`).** `BufferEngine` is now a
  thin handle over a `buffer_supervisor` thread. The supervisor spawns the **ring
  thread + save worker ONCE** (persistent core) and retains the tx ends of the
  producer→core channels (`item`, `mt`, `asc`) so a producer set exiting does **not**
  disconnect and tear the core down. It then runs an **epoch loop**: spawn a
  `ProducerSet` (capture/encode/audio) for epoch E feeding the SAME ring via fresh
  channel clones; on a device loss (`is_device_lost` on capture/encode) bump E, sleep
  the `§7` 500 ms budget, rebuild the D3D device (`rebuild_gpu`, retry ≤ 2 s), and
  respawn into the same core. The ring is **never** torn down — a save right after the
  restart still finds the last pre-loss GOPs (`§7` "older epochs remain saveable").
- **Per-epoch output type in the save worker (the "one missing link").** The `mt`
  channel now carries `(epoch_id, SendMediaType)`; the save worker is a `select!` loop
  holding **one output type per epoch seen** (a resolution change = new SPS/PPS) plus
  the epoch-invariant ASCs, and `process_save_job` muxes with the type matching
  `window.epoch_id` (`§4.2`). `select_window` already returned the epoch; this closes
  the loop. Pure selection helper `epoch_index` is unit-tested (exact match; older
  epochs stay addressable after a restart). The types list is unbounded over a session
  but grows one small COM object per restart (rare) — acceptable, noted in code.
- **Per-epoch stop flag (mirrors `RecordingEngine`).** Each `ProducerSet` owns an
  `epoch_stop` distinct from the session `stop`, so a device-loss rebuild is not
  mistaken for a user stop, and a device loss (which only exits capture/encode) can
  still bring the independent audio threads down before the rebuild.
- **Shutdown ordering.** On session stop the supervisor drops `item_tx` → the ring's
  `item_rx` disconnects → ring exits (drops its save-job sender) → the save worker's
  `save_job_rx` disconnects → it drains and exits; `mt_tx`/`asc_tx` are dropped only
  *after* the save join so the save-worker `select!` never busy-spins on a disconnected
  type/ASC channel.
- **Grid epoch (`pacing.rs`).** New `PacingGrid::with_default_grace_at_epoch(fps,
  epoch)` so a rebuilt capture thread tags its frames with the continuing epoch id
  (not reset to 0). `capture_thread`/`encode_thread` gained an `epoch` param; the
  record path passes 0 (single-epoch per segment; `mux_thread` ignores the tag).
- **New `BufferParams` fields:** `adapter: AdapterSelection` (to rebuild the device on
  loss) and `simulate_loss_after: Option<u64>` (the test hook). `main.rs buffer` gains
  a hidden `--simulate-device-loss N` flag (like the record path's).
- **New dep/features:** none. New `EngineError::Gpu(#[from] GpuError)` variant for the
  rebuild path.
- **SELF-VERIFIED on the Nitro (RTX 4050), not just claimed.**
  `buffer --autosave 8 --simulate-device-loss 5`: the loss fired at 5 s
  (`0x887A0005` = `DXGI_ERROR_DEVICE_REMOVED`), the supervisor logged
  `device lost mid-buffer — rebuilding into a new epoch (§7) epoch=1`, and **both**
  post-restart autosaves `clip saved` and passed **all 8 `just verify` checks** (2/2).
  The `§4.2` "clip shorter than requested" WARN correctly fired (epoch-1 content < 120 s
  post-restart → clamp to epoch 1's first IDR), proving the save selects the newest
  epoch. Clean `buffer stopped.` shutdown.
- **NEXT (needs the orchestrator's manual HW test — the natural gate):** the window
  **resize** (`ContentSize` change → epoch), **close** (`Closed` → monitor fallback),
  and **no-frame** (`§6.3` > 1 s, exclusive-FS) triggers. These reuse this machinery
  (each just makes the capture thread end its epoch with a restartable outcome) but
  can only be validated by resizing/closing a real window — WGC's event semantics
  (does `ContentSize` report the new size? does `Closed` fire?) want observing on HW
  before the final wiring. Auto-QP-relief (`§6.2`) still deferred.
  - **Observation surface built this turn (additive, low-risk):**
    `CapturedFrame::content_size()` (the resize signal) and `WgcCapture::is_closed()`
    (an item `Closed`-event flag, registered/removed with the capture) — the engine
    doesn't use them yet. Plus a **`window-events-probe [SECS]`** diagnostic that
    watches the focused window and logs every `ContentSize` change and the `Closed`
    event. **This is the orchestrator's next HW test:** run it, resize the window,
    drag it across monitors (DPI change), then close it, and report the logged events
    — that behaviour is the empirical input the resize/close trigger wiring needs.

### M4-2 `window-events-probe` HW findings (2026-07-05) + `ResizeTracker`

Ran on the Nitro (resize, monitor drag, close):
- **Resize = a continuous flood of `ContentSize` changes** — a new size on ~every
  delivered frame during the drag (dozens/second), through a whole range of (often
  ODD) sizes, then WGC goes quiet once the drag settles (static window → no frames,
  `§1.2`). **The pool stayed at the start size throughout** (WGC does not auto-resize
  it). ⇒ the resize trigger **must debounce** and restart the epoch ONCE at the
  settled size, never per change; and the settle check must be **time-based**, not
  frame-driven (no frame arrives after the drag stops).
- **A monitor/DPI switch reads as a large `ContentSize` jump** — same code path as a
  resize.
- **Odd sizes are the norm mid-drag** — the M4-1 even-dimension converter fix is
  load-bearing for window mode.
- **`Closed` event UNCONFIRMED** — no `[closed]` line appeared and the probe ended via
  Ctrl+C (`STATUS_CONTROL_C_EXIT`), so it's ambiguous whether closing the window fired
  `Closed` or the operator just stopped early. **Re-test needed:** close the window and
  wait ~5 s (don't Ctrl+C). This matters because for a *window*, "no new frames" cannot
  distinguish a static window from a closed one (the grid resubmits the last frame
  either way), so `Closed` is the only reliable close signal — the `§6.3` no-frame
  watchdog only catches a target that NEVER delivered a first frame (exclusive-FS).
- **Built `capture/resize.rs::ResizeTracker` (pure, 6 unit tests)** — debounces the
  ContentSize stream into a single settled size (`observe` per frame + a time-based
  `poll`), default settle 400 ms. Captures the trickiest part of the resize trigger,
  fully tested without HW; slots into the capture thread when the triggers are wired.
- **Still open (the wiring, HW-gated):** feeding `ResizeTracker`/`is_closed()`/the
  no-frame check from the capture thread into a producer→supervisor restart that can
  target a DIFFERENT source (resize = the SAME window at the new size — needs the
  resolved HWND threaded so `FocusedWindow` doesn't re-resolve to a different window;
  close/exclusive-FS = the primary monitor). Needs the `Closed` confirmation + HW
  validation of the actual restart.

### M4-2 window triggers WIRED (`Closed` confirmed NOT firing → `IsWindow` polling)

Second `window-events-probe` run (ran the full 30 s, no Ctrl+C): closing the window
produced **`closed=false`, no `[closed]` line** — WGC's `GraphicsCaptureItem.Closed`
**does not fire on window close** on this Win11 build (minimize/restore also silent).
Decisive: the close detector cannot rely on `Closed`. Wired all three triggers into
the capture thread → the M4-2-core supervisor:
- **Resize → `ResizeTracker`** (settled ContentSize) → restart re-targeting the SAME
  window at the new size via **`CaptureSource::Window(hwnd)`** (a new internal,
  non-config source variant carrying the resolved `HWND` as `isize`, so the rebuild
  pins the same window instead of re-resolving `FocusedWindow` to whatever is focused
  then). The new epoch's `WgcCapture` re-reads the window's current size → new pool +
  converter + encoder at the settled size.
- **Close → `IsWindow(hwnd)` polling** (every 250 ms; `Closed` is kept as a
  best-effort secondary, e.g. monitor removal). `IsWindow` flips false on destroy but
  stays true while minimized, so a minimize is correctly NOT a close (matches the
  probe). → fall back to `PrimaryMonitor`.
- **No-frame → exclusive-fullscreen:** a window that never sets the grid base within
  `NO_FRAME_TIMEOUT` (1 s, `§6.3`) → fall back to `PrimaryMonitor`. Window-source only.
- **Protocol:** the capture thread (buffer mode passes a `RestartRequest =
  Arc<Mutex<Option<CaptureSource>>>`; record passes `None` → no triggers, M1 behavior
  preserved) records the next source on a trigger and returns `Ok`; `ProducerSet`'s
  `restart_request` is read in `join_and_classify` → new `ProducerOutcome::Restart(src)`
  → the supervisor bumps the epoch, sets `current_source = src`, and rebuilds with NO
  device rebuild and NO recovery sleep (distinct from device loss). `check_restart_triggers`
  runs every loop iteration (fires even on a static screen where no frame drives the loop).
- Root crate green: `just check` + `just test` (**141** + 1 HW-skipped), clippy/fmt
  clean. **Device-loss restart re-verified on HW after this change (no regression):**
  `--simulate-device-loss` → rebuild epoch 1 → post-restart clip passes all 8 checks.
- **NEEDS ORCHESTRATOR HW VALIDATION (can't self-test — needs a real window):**
  `target = "focused-window"`, `buffer`, then (a) **resize** the window → expect one
  `capture size settled — restarting epoch` per settle + saves keep working; (b)
  **drag across monitors** → same; (c) **close** the window → expect
  `captured window closed — falling back to the primary monitor` and the buffer keeps
  running on the monitor; (d) **minimize/restore** → expect NO restart. Each saved
  clip should `just verify` clean (single-epoch, no span). Auto-QP-relief still deferred.

## 2026-07-05 — M4-2 AMENDMENT: window resize → FIXED CANVAS (no epoch), not a cut

**HW finding (orchestrator).** With resize-as-epoch wired, resizing the captured
window truncated every save to *since the last resize* — the `§4.2` epoch clamp
("clip must not span epochs", `§0`) biting on each `ResizeTracker` settle. Correct to
the letter of `§0` but **wrong replay-buffer UX** (a resize before a great moment loses
the history). Orchestrator decision: adopt **pitfall 11's "fixed output resolution
chosen at buffer start"** for window resize.

- **`§0`/pitfall-11 amendment (this dated entry is the record `§0` interpretation
  the plan asks for).** A *window resize* keeps the **encoded (output) resolution
  fixed**, so it is NOT a `§0` "resolution change" and does **not** start a new epoch:
  the video processor rescales the resized window content into the fixed canvas, and
  the clip spans the resize. The epoch machinery is retained ONLY for genuine
  output changes / capture-target changes — **window close → monitor** and **device
  loss** — which remain cut-at-the-boundary (a clip must not span *those*).
- **Aspect policy = LETTERBOX / PILLARBOX (never stretch).** A window resize changes
  aspect, not just size; the VP scales-to-fit and centers within the canvas with black
  bars, never distorts. **Real UX cost:** clips gain bars after a resize to a
  different aspect — stated here and in the README limitations list.
- **Canvas sizing = a CONFIG KEY, not a hidden heuristic.** "Window size at buffer
  start" was rejected as fragile (start small → maximize → everything downscaled).
  Rule: canvas = the **capture monitor's resolution**, capped at a configured
  **encode-height ceiling**, dimensions rounded to even, fixed for the session (so a
  drag across monitors rescales into the same canvas — no epoch). New config
  `[encode].max_height` (see config.rs).
- **Tracker/plan:** the M4 resize item is reworded to the fixed-canvas behavior; a
  SEPARATE item keeps the **cut path** (close→monitor, device loss) with its own
  no-crash test. M4-PLAN amended.
- **Acceptance procedure (devflow; run on the Nitro):** buffer running on
  `target = "focused-window"`; resize the window **twice** (grow AND shrink, changing
  aspect), then save. The clip MUST contain the **full requested duration**;
  `just verify` green; **one resolution** in `ffprobe` (single canvas, no epoch span);
  and an **`avrig` click/flash pair straddling a resize** to prove the grid/audio sync
  rode through the frame-pool recreation (the `§1.2` resubmit rule should cover the
  brief no-frame gap during the pool rebuild — one explicit measurement).

### Fixed-canvas IMPLEMENTED (compile-green; monitor path + letterbox VP self-verified)

- **`capture/canvas.rs` (pure, 7 tests):** `canvas_size` (monitor res capped at
  `[encode].max_height`, evened) + `letterbox_rect` (integer scale-to-fit, centered,
  even edges — pillarbox/letterbox, never stretch).
- **`convert.rs`:** `Converter::new(gpu, input, canvas, fps)` — VP scales a variable
  input into the fixed canvas via `SetStreamSourceRect`/`SetStreamDestRect`
  (letterbox) with an opaque-black `SetOutputBackgroundColor` for the bars. Rebuilt
  cheaply per resize.
- **`wgc.rs`:** `recreate_pool` (`FramePool::Recreate` at the new content size; keeps
  the `FrameArrived`/`Closed` subscriptions) + `window_monitor_size`
  (`MonitorFromWindow`) for the canvas basis.
- **`engine.rs` capture thread:** computes the canvas at start, sends the encoder the
  CANVAS (fixed); on a `ResizeTracker` settle it recreates the pool + rebuilds the
  converter to the canvas and **continues the SAME epoch** (drains the old-size frame
  from the cell first). Close / no-frame remain epoch restarts → monitor (`check_
  target_change`). Record passes `None` (no triggers).
- **`config.rs`:** new `[encode].max_height` (default 2160, range 480–4320), validated.
- **Self-verified on the Nitro:** the HW letterbox test (`odd_input_scales_into_fixed
  _canvas`, 1115×627 → 1920×1080) passes on the RTX 4050; a monitor-capture buffer +
  device-loss restart saved clips that `just verify` clean at a single **1920×1080**
  resolution (`ffprobe`). Root crate green: `just check` + `just test` (**148** + 1
  HW-skipped), clippy/fmt clean, release 2.01 MB.
- **STILL NEEDS the orchestrator's window HW acceptance** (can't self-test — needs a
  real window): the resize acceptance procedure above (resize grow+shrink → full-
  duration clip, one resolution, letterbox bars on aspect change; + the avrig
  straddle). Limitations in `LIMITATIONS.md`.

## 2026-07-05 — M4-2 AMENDMENT 2: window CLOSE also spans (fixed canvas), not a cut

**HW finding (orchestrator).** With close→monitor as an epoch cut, a save after closing
the captured window contained only the *post-close monitor* footage — the pre-close
window footage was dropped by the `§4.2` clamp (same truncation the resize fix removed,
now for close). Orchestrator decision: **extend the fixed-canvas span to window close**.

- **Close / exclusive-fullscreen no-frame are now handled IN-THREAD**, like resize:
  the capture thread switches its source to the primary monitor scaled into the SAME
  canvas (same encoder), so **no epoch starts and the clip keeps the pre-close window
  footage**, then continues on the monitor. (Also fixes the resize artifact context:
  a resized-away region self-cleans on the pool recreate — noted in `LIMITATIONS.md`
  as a mid-drag cosmetic transient.)
- **Only a DEVICE LOSS now restarts the epoch** (its encoder rebuild is unavoidable) —
  reverses the earlier "close→monitor is a cut path" framing (Amendment 1).
- **Simplification:** the whole `restart_request` / `ProducerOutcome::Restart` /
  `RestartRequest` supervisor machinery is **removed** — the capture thread handles
  resize/close/no-frame in-thread (a `triggers_enabled: bool` replaces the
  `Option<Arc<Mutex<…>>>`), and the supervisor's only restart trigger is a device loss
  (rebuild same source + device). `check_target_change` → `should_fall_back_to_monitor`
  (returns `bool`; the caller does the in-thread monitor switch). Net: less code, one
  restart path.
- Root crate green: `just check` + `just test` (**148** + 1 HW-skipped), clippy/fmt
  clean. **Device-loss restart re-verified on HW after the refactor** (no regression):
  `--simulate-device-loss` → rebuild epoch 1 → post-restart clip saves clean.
- **NEEDS the orchestrator's window HW re-test:** resize (spans, as before) AND now
  **close the window mid-buffer, then save** → the clip must contain the window footage
  BEFORE the close plus the monitor tail AFTER, at one resolution, `just verify` green.

## 2026-07-05 — M4-3 timed-record disk sink + M4-4 record-toggle hotkey (self-verified)

Closes M4 (window mode + timed recording). Timed recording = **tee off the ring** (D1):
the ring thread forwards each `MuxItem` to the **mux worker** (the former save worker,
now driving both one-shot saves AND a live `Fmp4Writer`). Root crate green: `just check`
+ `just test` (**148** + 1 HW-skipped), clippy/fmt clean, release 2.05 MB.

- **Mux worker (`engine.rs`).** `MuxItem` is now `Clone` (Arc bump) so items tee cheaply.
  The worker `select!`s over saves + `rec_ctrl` (Start/Stop) + teed `rec_item`s, and
  reuses the cached per-epoch output type + ASCs to open a recording writer. A
  device-loss epoch change finalizes the recording (`§0`); a full tee channel or write
  error stops it. `record` filename `<product>_rec_<ms>.mp4`.
- **§4-clean edges — the real work (per the `§5` AV-3 bar; the devpack gives recordings
  NO exemption).** A naive live tee had 129 ms head-silence + 90 ms early audio end.
  Fixes: (1) **head** — the worker BUFFERS audio while `Pending` and, on the first teed
  video IDR, replays it into the writer so the writer's own prebuffer admits it at the
  origin (`§4.4` ≤ 1 AAC frame); (2) **tail** — on stop the RING THREAD `Draining`s: it
  tees only audio until it reaches the last teed video PTS (or a 500 ms timeout), then
  sends `Stop`, so audio ends with video. **Self-verified:** `--record-secs 8` → an 8 s
  1920×1080 recording PASSES all 8 `just verify` checks (log: `prebuffered=12` audio AUs,
  `audio drained to the video tail`).
- **Buffer protection.** The tee uses `try_send`; if the mux worker falls behind the
  disk, the recording stops (WARN) rather than stalling capture — the replay buffer is
  the primary feature.
- **M4-4: two hotkeys, tolerant registration.** `HotkeyPump::spawn(&[save, record])`
  registers both; the ring thread's `select!` dispatches by id. **Registration is now
  non-fatal** — a hotkey already owned by another app (the Nitro has **Ctrl+Alt+R**
  taken) warns and is skipped, so buffer mode still runs and save works. Recommend
  changing the default `record_toggle` or documenting the override. Also a hidden
  `--record-secs N` test hook (auto start-at-buffer-start + stop after N) drove the
  self-verify.
- **Deferred (flagged):** segment-on-epoch for a recording that outlives a device loss
  (v1 stops it — device loss is rare); force-IDR-on-start (not needed — the drop-until-
  first-IDR gives a clean keyframe open within ≤ 1 GOP). `RecordingEngine` (the M1/M2
  ring-less disk path) is now fully redundant with the buffer engine + this disk sink;
  retiring it is a separate cleanup once the converged path is orchestrator-validated.
- **NEEDS the orchestrator's HW check (record hotkey):** with a FREE `record_toggle`
  combo, press it to start, let it run, press again to stop → `just verify` the
  `<product>_rec_*.mp4` green (the `--record-secs` path is already self-verified).

## 2026-07-05 — save-path mic head-silence fill (closes the M4-1 deferred finding)

Branch `fix-save-mic-head-silence`. Fixes the pre-existing `§4.4` failure logged in the
"M4-1 HW-run finding" above: a clip **saved before the ring fills** clamps its origin to
~capture-start, but the mic's first AAC AU lands 28–63 ms later (WASAPI device startup),
so the mic track (`a:1`) started > 1 AAC frame after the origin and failed the `just
verify` audio-head-silence check. Video and the desktop track were always fine.

- **Fix location = the muxer (`mux/fmp4.rs`), not the save selector.** `Fmp4Writer` is
  shared by BOTH the `§4` save path (`save.rs::save_clip`) and the live record path
  (`engine.rs` mux worker), so one change covers early saves AND any cold-start
  recording. The muxer stays pure/no-COM and unit-testable.
- **Synthesize leading silence (`§2.3`-consistent).** New pure `plan_head_fill(pts,
  origin, have_template)` returns `(silent_aus, offset)`: with a template and a gap ≥ 1
  AU it prepends `gap/1024` whole silent AUs and sets the residual `gap%1024` (< 1 AU) as
  the track's `initial_offset`, so the track *starts* at the origin within ≤ 1 AAC frame
  while the first real AU still lands sample-accurately (`offset + k·1024 == gap`). With
  no template `(0, gap)` = the legacy `§4.4` head slack — a safe fallback, zero behavior
  change. `place_audio` gained a `push_au` helper so the silence loop and the real AU
  share the same pending/flush path (fragment cuts at ~1 s unchanged).
- **Silence template source (`encode/mft_aac.rs`).** New `AacEncoder::silent_au(bitrate)`
  encodes one steady-state AAC-LC silence AU on a **throwaway** encoder (never the live
  one — reusing it would corrupt `anchor_pts`/`au_index`), feeding ~8 zero-PCM frames to
  clear the 1024-sample priming and returning the last (steady) AU. A silent AAC-LC frame
  at the fixed 48 kHz/stereo/bitrate config is content-deterministic, so one AU repeats
  cleanly. `audio_process_thread` populates `AudioTrackConfig::silent_au` **best-effort**:
  on failure it `warn!`s and leaves it empty (→ legacy behavior, no hard failure).
- **No deps, no `windows` features, no new `unsafe`** (the template reuses the encoder's
  existing COM path; `plan_head_fill`/`place_audio` are 100 % safe). +4 pure unit tests
  (`plan_head_fill` spec edges; `place_audio` prepend / no-template / pre-origin-drop):
  root crate `just check` + `just test` = **153** + 1 HW-skipped, clippy `-D warnings` +
  fmt clean.
- **Ready for the 04-TEST-MACHINE re-run (NOT claimed working):** `clipd buffer`, then
  save within ~15 s of the cold start → `just verify` the clip; the `a:1` mic
  head-silence check should now pass (was 28–63 ms). Full-buffer saves and recordings are
  unaffected (their gap is already < 1 AU, so `silent_aus == 0`).

## 2026-07-05 — retire `RecordingEngine` (converge `record` onto the ring+disk path)

Branch `retire-recording-engine`. The M1/M2 ring-less disk recorder was fully redundant
with the M4-3 tee-off-ring disk sink (planned retirement, DECISIONS "M4-3" / M3 decision
#2). `record --seconds N [--out PATH]` now runs on `BufferEngine`; the parallel machinery
is deleted. **User-confirmed** the two converged behavior changes below before the work.

- **Deleted (`engine.rs`, −~295 lines):** `RecordingEngine` (struct + `start`/
  `stop_and_join`/`any_worker_finished`/`stats`), `RecordParams`, `RecordOutcome`,
  `RecordStats`, and `mux_thread` (the ring-less direct muxer, used only by
  `RecordingEngine`). `main.rs`: the old epoch-loop `run_record`, plus the now-dead
  `segment_path` and `default_output_path` helpers. Shared producers (`capture_thread`,
  `encode_thread`, `audio_process_thread`, `run_capture`, `PipelineStats`, `spawn`, the
  channel caps, `build_gpu`) are untouched — the buffer path already uses them.
- **`record` on the converged path.** `run_record` builds `BufferParams` with a **minimal
  2 s ring** (the recording tees LIVE off the ring — the ring is never read for the file,
  so its size is irrelevant and kept small to protect the RAM budget the old ring-less
  path enjoyed), **no hotkeys** (unused ids; record mode is not hotkey-driven), and the
  new `record_autostart = true`. `--seconds N` → `record_auto = Some(N)` (auto-stops with
  the M4-3 `§4`-clean tail-drain), else records until Enter. `--out PATH` is honored via
  the new `BufferParams::record_out` (threaded to the ring thread's auto-start); default
  is still `<product>_rec_<ms>.mp4`. The process exits N + 2 s after start (grace covers
  the ≤ 500 ms tail-drain) or on Enter.
- **New `BufferParams`/`RingThreadConfig` fields (additive, buffer mode unchanged):**
  `record_out: Option<PathBuf>` and `record_autostart: bool`. The ring thread's auto-start
  now gates on `record_autostart` (was `record_auto.is_some()`); `--record-secs` sets it
  from `record_secs.is_some()`, so the `buffer` hook and normal hotkey-driven buffer mode
  behave exactly as before.
- **Two accepted behavior changes (user sign-off, vs the old `record`):** (1) a
  mid-recording **device loss STOPS** the recording (the old path segmented into
  `clip-1.mp4`; segment-on-epoch is the M4-3-deferred rare case — the buffer itself still
  survives and rebuilds); (2) a **minimal ring is held** during `record` (the old path
  held none). Both are documented and reversible.
- **No deps, no `windows` features, no new `unsafe`.** Net **−~310 lines**. `just check` +
  `just test` = **153** + 1 HW-skipped, clippy `-D warnings` + fmt clean; release **1.98 MB**
  (was 2.05; budget 10). Binary dispatch smoke-tested (`--help`, arg rejection).
- **Ready for the 04-TEST-MACHINE re-run (NOT claimed working):** `record --seconds 8`
  (and `--seconds 8 --out clip.mp4`) → `just verify` green; `buffer` save + `--record-secs`
  unchanged (regression check). Deferred HW pass runs alongside the mic-head-silence check.

## 2026-07-06 — strict devpack + adversarial review of both changes (pre-sign-off)

Ran a strict devpack pass + an independent adversarial Rust review over the full diff
(vs `9c30af1`). No dep/feature/`unsafe`/budget/scope violations. Two findings; one fixed,
one documented as a pre-existing within-tolerance latent:

- **FIXED — head-silence fill was unbounded.** `plan_head_fill` (`mux/fmp4.rs`) placed no
  cap on the synthesized silent-AU run, so a track that legitimately starts many seconds
  after the origin (a device held exclusively for a long time, then a save straddling the
  pre-start region) could burst thousands of cloned AUs + fragment flushes onto the mux
  thread in one `place_audio` call. Added `MAX_HEAD_SILENCE_AUS` (~2 s of AUs — far beyond
  real device-startup latency, incl. the `§7` 750 ms rebuild); any excess stays as an
  implicit offset, and the real AU still lands sample-accurately (`offset + k·1024 ==
  gap_units`). +1 cap test. The M4-1 target case (mic ~30–60 ms late → `k`≈3) is far under
  the cap and unchanged.
- **DOCUMENTED (pre-existing M4-3, not introduced here; within spec tolerance) — the
  `Draining`→`Stop` tee/ctrl cross-channel race.** At a timed-record stop the ring thread
  tees the tail catch-up audio AU on `rec_item` then sends `RecordCtrl::Stop` on `rec_ctrl`;
  the mux worker's `select!` has no cross-channel ordering, so it can finalize before that
  last AU, dropping it. Worst case: the recording's audio ends exactly **1 AAC frame** short
  of the video tail — still within the `§5` AV-3 "audio within 1 AAC frame of video" bound
  (which is why M4-3 self-verified green). This work only *routes* `record --seconds` through
  the already-validated M4-3 `Draining` path (the `--record-secs`/hotkey path used it since
  M4-3); it does not touch that code. Left as a flagged latent (a real fix — e.g. draining
  `rec_item` before finalize — is M4-3 core and out of this task's scope).
- **Doc hygiene (surfaced by the review):** `main.rs`'s module header and `--help` footer
  still claimed "engine not yet implemented (Milestone 0 pending)"; corrected to describe the
  wired `record`/`buffer` dispatch, and the no-arg branch now prints usage instead of the
  stale message.

## 2026-07-06 — HW validation (both follow-ups closed on the Nitro V15)

Orchestrator ran the deferred 04-TEST-MACHINE pass on the Nitro; both changes confirmed on
hardware (the machine says it works, not the agent):

- **Mic head-silence:** a cold-start save (within the first buffer of a fresh `clipd buffer`)
  now passes the `§4.4` audio-head-silence check on the `a:1` mic track (was 28–63 ms over).
- **Converged `record`:** `record --seconds N` (± `--out`) writes a clean clip passing `just
  verify`; `buffer` save + `--record-secs` unaffected (no regression).

Both HANDOVER §2c items are marked DONE + HW-VALIDATED. The one carried-forward flag is the
pre-existing M4-3 `Draining`→`Stop` cross-channel race (within `§5` AV-3 tolerance; not a
blocker) — a candidate for its own small task if the tail-alignment is ever tightened.

## 2026-07-06 — M5 plan: shell & trust (design decisions, pre-implementation)

Wrote `M5-PLAN.md` (repo root) — the Milestone-5 design against `05-MILESTONE-TRACKER.md`
M5 and `01-PROJECT-PLAN.md §5.5`. No code written yet. Two behavioral choices locked with
the orchestrator up front so the tray/engine seams are built to them:

- **Tray Pause = stop ingesting new footage; keep the buffer active (retained), pipeline
  running.** A Pause menu press makes the ring thread stop pushing new packets into the
  `Ring` (dropped at the tee point) while **retaining** existing ring contents and keeping
  capture/encode running (pixels discarded before the ring — instant, reversible, no
  teardown). Any in-progress timed recording is stopped (you cannot record while paused).
  A save while paused still works on the already-buffered footage (the buffer is "active").
  On unpause, ingestion resumes; the buffer carries a time gap across the paused span (a
  clip spanning it simply holds less footage — documented in `LIMITATIONS.md`). Rejected for
  now: (a) clearing the ring on pause (would throw away usable footage — orchestrator chose
  to keep it); (b) tearing down capture for zero-GPU-while-paused (that is the ~2 s
  device-loss path, too janky for a frequent toggle — deferred to M10 `buffer_when`). This
  reverses my initial "clear + refuse saves" recommendation per orchestrator direction.
  Trade-off recorded: CPU/GPU are still spent while paused; true suspend is an M10 concern.

- **Tray state icons are generated programmatically (solid colour per state), behind a
  swappable seam.** The four states (Buffering / Paused / Warning / Error) get solid-colour
  RGBA icons built in code (no PNG assets, no licensing, no binary bloat). The icon source
  is isolated behind a single `icon_for(state)` function in `ui.rs` so switching to shipped
  images later is a one-function change (`include_bytes!` a PNG per state) with **no** call-site
  churn — kept deliberately reversible/editable per the orchestrator. Rejected for now:
  shipping designed PNGs (unnecessary for M5; the seam keeps it a trivial future swap).

New deps (both already on the CLAUDE.md rule-2 whitelist; called out here per rule 2):
`tray-icon` (pulls `muda` transitively for menus) and `tracing-appender` (rolling file log).
New `windows` feature `Win32_System_Registry`, added in the start-with-Windows commit that
calls it (devflow: only APIs actually used), for the single permitted HKCU Run-key write
(CLAUDE.md constraint 5 / 06-SAFETY-AND-VMS.md). Release-size impact will be measured via
`just release` and reported (budget 10 MB; currently 2.05 MB). Full details, task breakdown,
and the main-thread-message-pump + `EngineCommand`/`ShellSignal` seam are in `M5-PLAN.md`.

## 2026-07-06 — M5 T2 (tray shell): dep scoping + deny graph-targets

Implemented the tray shell (`ui.rs`) + the `EngineCommand`/`ShellSignal` engine seams.
Three follow-on config choices, recorded per CLAUDE.md (dep/config changes are never buried):

- **`tray-icon` with `default-features = false` + `common-controls-v6`.** Its default
  features are the Linux desktop bits (`libxdo`, `gtk`, `libappindicator`); dropping them
  keeps the graph lean. On `x86_64-pc-windows-msvc` the PNG/x11/gtk deps are already
  target-gated out, so icons are built from RGBA in `ui.rs` (no image decoder linked).
  `common-controls-v6` gives the modern Win32 menu styling (a manifest-only cost).
- **`deny.toml` `[graph] targets = ["x86_64-pc-windows-msvc"]`.** cargo-deny checks ALL
  targets by default, so it flagged `option-ext` (MPL-2.0), reached only via `tray-icon`'s
  **Linux-only** `dirs` dep — code this Windows binary never compiles. The product is
  Windows-only and the toolchain is pinned to that triple, so scoping deny to it makes the
  check evaluate exactly what ships (also prunes the x11/gtk multiple-versions noise). No
  new license was allow-listed; the MPL crate simply isn't in the Windows graph. Simpler +
  more accurate than broadening the license allow-list for a crate we don't build.
- **Binary size:** `just release` = **2.48 MB** (was ~1.98 MB); +~0.5 MB for `tray-icon` +
  `muda` + `tracing-appender`. Budget 10 MB — comfortable.

Seam summary: the tray injects the SAME actions as the global hotkeys over an explicit
`EngineCommand` channel (`SaveClip`/`ToggleRecord`/`SetPaused`/`Shutdown`) read in the ring
thread's `select!`; the engine emits `ShellSignal::State(TrayState)` back. The engine stays
fully headless — the `record` subcommand and the hidden `--autosave`/`--record-secs`/
`--simulate-device-loss` hooks keep the Enter/timer loop and never build a tray; if the tray
can't be created, `buffer` falls back to the headless loop (the satellite rule). `SetPaused`
in T2 only reflects state + emits `Paused`; the actual ingest gating is T3.

## 2026-07-06 — M5 T2 fixup: tray binary failed to load (STATUS_ENTRYPOINT_NOT_FOUND)

HW validation surfaced that `clipd.exe buffer` (and every invocation, incl. `--version`)
crashed at load with `0xc0000139 STATUS_ENTRYPOINT_NOT_FOUND` — the OS loader could not
resolve an import, before `main` ran.

- **Cause:** the `tray-icon` `common-controls-v6` feature makes `muda` import v6-only
  `comctl32.dll` functions by name. Those resolve only when the application ships an
  embedded manifest declaring the Common-Controls v6 assembly — which `clipd` does not.
  Without it the import is unresolvable and the process fails to load.
- **Why CI missed it:** `cargo test` links the lib/bin *unit-test* harness, whose linker
  (`/OPT:REF`) dead-strips the tray-building path (no unit test constructs a `TrayIcon`),
  so the offending import was never in the test binary. The real `clipd.exe` reaches
  `TrayIconBuilder::build()`, so the import is present — and fails. Building/checking/
  testing all passed while the shipped binary could not start.
- **Fix:** drop `common-controls-v6` (→ `tray-icon = { default-features = false }`). The
  menu falls back to classic Win32 styling — perfectly adequate for a tray context menu —
  and needs no manifest and no resource-embedding build dep (rejected the alternative of
  adding a manifest via a build script + a non-whitelisted `winres`/`embed-resource`
  crate). Both debug and release (LTO+strip) binaries now load and run `--version`.
- **Regression guard:** added `tests/smoke.rs` (dev-dep `assert_cmd`, allowed by CLAUDE.md)
  that spawns the built binary for `--version`/`--help`/`--check-config`. These load the
  real exe — resolving every import — so a future load-time entrypoint failure fails CI
  instead of shipping. `version_loads_and_runs` reproduces (would have failed) this bug.

171 tests (3 new smoke), just check + deny green, release 2.49 MB.

## 2026-07-07 — Research/recalibration pass: M7+M8′ friends-beta slice (no code)

Orchestrator-directed research pass (web research + devpack re-read); full plan in
`M7-M8-PLAN.md` (repo root). Orchestrator instructions quoted there in §0. Decisions:

- **Sequencing: a reshaped M7+M8 goes BEFORE M6.** The friends beta (GTM §2.5 Phase-0
  "20-user quiet beta") supplies the external hardware M6 needs. M6 closes on beta
  evidence afterward. Orchestrator call, explicitly requested.
- **Frozen-spec amendments (02-AV-SYNC-SPEC.md), orchestrator-approved 2026-07-07**
  (precedent: the two dated M4-2 amendments):
  - **§2.5 track layout**: mixed track FIRST (compat: one-track players/platforms
    play/keep track 1), then optional per-app tracks — game / voice-chat / other-system
    / mic (5 total when `separate_tracks = true`; mix+mic when false). Replaces
    "two tracks, no mixed track in v1".
  - **§2.2 audio PTS for process-loopback streams**: `IAudioCaptureClient::GetBuffer`
    `QPCPosition` used directly as PTS (it IS the master domain). The device-position→
    QPC conversion path cannot run on these clients (DevicePosition always 0, no
    IAudioClock/GetStreamLatency — all E_NOTIMPL). §2.3 gap synthesis + §2.4 drift
    control unchanged. Endpoint streams (mix source, mic) keep the original rule.
  - **§4 finalize**: saved clips get an OBS-Hybrid-MP4-style appended `moov` after the
    fragment stream (Explorer/WMP/editor compat); §4.6 crash-safety intent preserved.
- **M8 reshaped** (08-FEATURE-COMPLETE): include/exclude modes + optional third mixed
  track → the fixed 4-track topology above. "Other system" = exclude-tree(game) and
  therefore ALSO CONTAINS VC audio — the API takes one process tree per client and
  excludes don't compose; `system − game − VC` is inexpressible. Accepted + documented
  rather than research-grade cross-client subtraction (nobody ships that).
- **Game-track binding**: window mode = captured window's tree; monitor mode = none
  until the foreground becomes a fullscreen/borderless app, then that PID's tree
  (sticky while the process lives). Foreground+fullscreen heuristic only — NO game
  database (non-goal intact). Same detector M10's `buffer_when = "fullscreen-app"` needs.
- **Quality UX**: named tiers (Efficient/Default/High/Max) over the CQP engine with
  derived Mbps/RAM feedback; NO raw-Mbps rate-control mode (spec §6.1 rationale stands;
  OBS-Simple-mode precedent). Raw CQ stays TOML-only.
- **MEASURED DEFECT → T0 (urgent)**: 1080p60 saves from the current binary average
  **2.1–5.5 Mbps video** vs spec §6.1's 12–20 Mbps expectation (ffprobe, three clips,
  Nitro, 2026-07-07). The `mft_h264.rs` CQ→`AVEncCommonQuality` linear map
  (23 → 55) was never calibrated (its own comment says "tuned against measured
  bitrate" — no such tuning recorded). Explains the orchestrator's observed color/
  complex-scene degradation. Fix = §6.1 adjustment-rule calibration sweep on HW; also
  check for a silent default `MF_MT_AVG_BITRATE` ceiling in Quality mode.
- **Deps**: `toml_edit` approved for the whitelist, effective when the Slice-A config-
  rewrite task lands (pitfall-30 unknown-key/comment preservation; callout required in
  that task summary). `eframe`/`egui` per the existing CLAUDE.md M7 sanction. The
  process-loopback API needs NO new dep — whitelisted `wasapi` crate exposes
  `new_application_loopback_client` (NB: its `include_tree: false` doc comment is
  wrong — the code does EXCLUDE mode; consider an upstream issue).
- **Platform floor**: per-app tracks runtime-probed, hidden below Win10 19041
  (docs claim 20348; OBS ships at 19041). Mix/mic pipeline unaffected below the floor.

## 2026-07-07 — T0 resolution: §6.1 CQP unreachable on NVENC-MF → bitrate-target amendment

**Frozen-spec §6.1 amendment (overrides 02-AV-SYNC-SPEC.md §6.1), measured on the Nitro
(RTX 4050, Media Foundation NVENC H.264 MFT).** The T0 defect (recorded above) was
investigated with an on-HW rate-control probe (`t0_sweep.ps1`, kept in the repo root as
reproducible evidence; deterministic ffmpeg `mandelbrot`/`testsrc2` fullscreen content
captured via `record --encode-*` hidden hooks). Findings:

- **The handover's assumed root cause was WRONG.** The CQ→`AVEncCommonQuality` map is
  not miscalibrated — the knob is a **no-op**. Sweeping `AVEncCommonQuality` 55→85 moved
  bitrate by <2% (mandelbrot flat ~7.5–8.6 Mbps; testsrc2 flat ~6.6–6.8) in BOTH
  `Quality` and `UnconstrainedVBR` modes. Recalibrating the formula would change nothing.
- **True CQP is unavailable.** `CODECAPI_AVEncVideoEncodeQP` is **rejected** (E_INVALIDARG)
  in every rate-control mode (confirmed VT_UI8 packed-QP, `quality` + `uvbr`). So spec
  §6.1's constant-QP mandate cannot be honoured through the MF-only path (CLAUDE.md rule
  4: no FFmpeg/vendor SDK) on this hardware.
- **Only bitrate controls output, and it does so precisely.** `MF_MT_AVG_BITRATE` /
  `AVEncCommonMeanBitRate` at a 16 Mbps target → 15.5–16.5 Mbps across `uvbr`/`pcvbr`/`cbr`
  (a 60 Mbps target → 60.4 Mbps). PeakConstrainedVBR is genuinely content-adaptive:
  measured 16.4 Mbps on mandelbrot, 15.5 on testsrc2, and **6.0 Mbps on a static desktop**
  — i.e. it keeps CQP's "cheap when idle, full rate when busy" behaviour.

**Decision (orchestrator pre-authorized the "probe CQP, auto-fall-back" path):** the
shipping encoder abandons CQP and targets a bitrate via **PeakConstrainedVBR**:
- Average = the §6.2 per-resolution table (`spec_constants::encoder::video_target_bitrate_bps`):
  1080p60 **16**, 1440p60 **26**, 4K60 **50** Mbps of video, scaled linearly by fps. This
  is the SAME number the ring byte cap already used (`ring::est_bitrate_bps` now delegates
  to it — one source of truth).
- Peak cap = **1.5× average** (`PEAK_BITRATE_HEADROOM`, = `BYTE_CAP_HEADROOM`). Invariant:
  instantaneous bitrate ≤ 1.5× avg ⇒ bytes over any window ≤ 1.5× avg × duration = the byte
  cap, so a peak-capped stream can never blow the ring budget (unit-tested).
- Vestigial: `NVENC_CQ`/`AMF_QP`/`QSV_ICQ` kept for provenance; `AVEncCommonQuality` still
  set (harmless no-op). The named quality tiers (Efficient/Default/High/Max) land in Slice A
  as multipliers over this target.

**Acceptance:** "Default" (PCVBR, 16 Mbps) measured **16.4 Mbps** on the active test scene —
inside §6.1's 12–20 Mbps band — and 6.0 Mbps idle. Shipping-path wiring confirmed via the
`H.264 encoder configured shipping=true rc_mode=1 avg_bitrate_bps=Some(16000000)
peak_bitrate_bps=Some(24000000)` log line. `just check` + `just test` green (173 tests).

**New hidden hooks (calibration harness, like `--record-secs`):** `record`/`buffer` accept
`--encode-rc-mode`, `--encode-quality`, `--encode-qp`, `--encode-avg-bitrate`,
`--encode-max-bitrate` (→ `EncoderOverrides`). All-absent = the shipping path. Not in
`--help`. Reused by Slice A's quality-tier work.

## 2026-07-07 — A1: config schema v2 (quality/resolution tiers), format-preserving rewrite, `toml_edit` whitelisted

**M7 Slice A task A1** (M7-M8-PLAN §3). Config bumps to `config_version = 2` and gains the
rewrite path the UI (A2–A5) will write through. Pure-logic module; `just check` + `just test`
green (184 tests, was 173).

- **`toml_edit` joins the core dependency whitelist** (CLAUDE.md rule 2), pre-authorized by
  the M7-M8-PLAN §0.4 amendment ("toml_edit joins the whitelist when the config-rewrite task
  lands"). Version `0.25.12`, default features only (`display` + `parse`); **no `serde`
  feature**. Rationale: the `toml` serializer emits a fresh document and cannot preserve user
  comments or unknown/forward-compat keys (pitfall 30). Reads still go through `toml`/serde
  into the single typed `Config`; `toml_edit` is only the write serializer, applied field-by-
  field onto the on-disk document — **not a second schema representation** (CLAUDE.md UI rule).

- **Quality tiers are BITRATE MULTIPLIERS, not CQ values.** M7-M8-PLAN §3 A1 literally says
  "per-vendor CQ map", but that text predates the same-day **T0 resolution** (above) and the
  HANDOVER §2 directive; T0 proved CQP is a no-op / rejected on the NVENC-MF path. Following
  the handover: `encode.quality = efficient|default|high|max` maps to multipliers over the
  T0-calibrated `video_target_bitrate_bps`. **Multipliers (orchestrator-selected): 0.6 / 1.0
  / 1.5 / 2.0** → 1080p60 = 9.6 / 16 / 24 / 32 Mbps. `Default` reproduces the T0 baseline
  exactly. The multiplier is a parameter of `video_target_bitrate_bps` and is threaded to
  BOTH the encoder target AND `ring::est_bitrate_bps` (the byte cap), so a higher tier is not
  evicted by a cap sized for `Default`; the `PEAK ≤ BYTE_CAP` headroom invariant is
  multiplier-independent and still holds at every tier (unit-tested per tier).

- **`encode.resolution = native|1440|1080|720`** is the friendly enum; `native` maps to the
  historical `DEFAULT_MAX_ENCODE_HEIGHT` (2160) — **decision: no behavior change vs the v1
  default** for >1080p monitors (orchestrator-selected over "true native / no cap", which
  would raise encode load + byte-cap RAM on 4K/8K, a constraint-7 risk). The v1 raw
  `max_height` survives as an **optional advanced override** (`Option<u32>`, TOML-only,
  omitted from output when unset via `skip_serializing_if`); when set it wins over the tier.
  `effective_max_height()` = `max_height.unwrap_or(resolution.to_max_height())` is the single
  value the capture canvas is built from.

- **v1 → v2 migration is in-memory only** (`Config::migrate`, run on every load); the disk
  file is rewritten to v2 only on an explicit user change (pitfall 30). A v1 file's
  `max_height` is preserved losslessly as the override — or dropped for a clean
  `resolution = "native"` when it equals the historical default cap. Version outside
  `MIN_SUPPORTED_CONFIG_VERSION`(1)..=`CONFIG_VERSION`(2) is rejected, never silently reset.

- **`[audio.tracks]` + `[[audio.vc_apps]]` are schema-only in A1** — parsed, validated,
  round-tripped, seeded (Discord family as the P0 default), but NOT yet consumed by the
  engine. The 4-track pipeline and the VC scanner that read them land in Slice B (M8′); the
  full P1/P2 VC table ships with that scanner. Added now so the v2 file is complete and the
  A5 settings UI has real keys to write.

- **Atomic write** (`Config::write_atomic`) reuses the `§4.7` `.part` → `sync_all`
  (FlushFileBuffers) → rename pattern; implemented locally in `config.rs` (pure `std::fs`,
  keeps the module 100% safe — the muxers' copies are COM-adjacent and not reusable).

- **New `just` recipes:** none. **New constants** (`spec_constants`): `MIN_SUPPORTED_CONFIG_VERSION`,
  `encoder::QUALITY_MULT_{EFFICIENT,DEFAULT,HIGH,MAX}`, `video::RESOLUTION_TIER_{1440,1080,720}`.
  Signature change (ripples to engine/main, all callers updated): `video_target_bitrate_bps`,
  `video_peak_bitrate_bps`, `ring::est_bitrate_bps` each gain a trailing `quality_mult: f64`.

## 2026-07-07 — A2: egui/eframe settings-window skeleton (satellite on its own thread)

**M7 Slice A task A2** (M7-M8-PLAN §3). First UI-module code + first `eframe`/`egui` link.
`just check` + `just test` green (186 tests, unchanged count — the window is GUI/thread code,
covered by the `smoke.rs` load test, not new units); release **8.28 MB** (8,681,984 B) vs the
10 MB budget (+6.1 MB over A1's 2.57 MB, all from eframe/egui/winit/glow).

- **NEW DEPS (both flagged, not buried):**
  - `eframe = { "0.35.0", default-features = false, features = ["glow", "default_fonts"] }`
    — CLAUDE.md "UI rules" sanction egui/eframe for the `ui` module alone. `default-features
    = false` drops wgpu, the Linux backends (wayland/x11), accesskit, and eframe's persistence
    storage; we keep only the glow renderer + bundled fonts. Config is written exclusively
    through the A1 `Config::write_atomic` path, **never** eframe storage (satellite law /
    pitfall 30).
  - `winit = "=0.30.13"` — a **direct** dep used ONLY for
    `EventLoopBuilderExtWindows::with_any_thread(true)`. eframe re-exports the `EventLoopBuilder`
    *type* but not the platform ext trait, so the trait must come from `winit` itself. Pinned
    (`=`) to the exact winit eframe 0.35 resolves, so cargo unifies to one winit and the trait
    applies to eframe's builder. UI-module-only, tightly coupled to eframe.
- **eframe 0.35 has the REDESIGNED `App` trait** (NOT the historical `update(&Context)`):
  `fn logic(&mut self, ctx: &Context, frame)` for non-drawing per-frame work + `fn ui(&mut
  self, ui: &mut Ui, frame)` for drawing (the handed `Ui` has no margin/background — wrap in
  `egui::Frame::central_panel`). Close-intercept + context-publish live in `logic`; widgets in
  `ui`. Anyone porting egui snippets from older docs must translate.
- **Satellite architecture:** the window runs `eframe::run_native` on its OWN thread
  (`settings-ui`), spawned lazily on the first tray "Settings…" click. Win32 message queues are
  per-thread, so the tray/hotkey main-thread pump is untouched. The engine coupling is a single
  clone of `Sender<EngineCommand>` (held for A5/A6; unused by the A2 skeleton). Direction is
  strictly `ui → engine` (enforced by module visibility: `settings` is a private submodule of
  `ui`; nothing in `engine` references it).
- **Reopen without recreating the event loop:** winit permits exactly ONE `EventLoop` per
  process, so closing + reopening cannot re-run `run_native`. The window's close request (the
  `X`) is intercepted (`CancelClose` + `Visible(false)` → hides); the tray re-shows it via a
  cross-thread `egui::Context` clone the app publishes on its first frame (`Visible(true)` +
  `Focus` + `request_repaint`). The UI thread lives until tray Quit, when `SettingsHandle::
  shutdown` sets a quit flag (letting the next close through) and joins; a `Drop` impl is the
  backstop so the thread never outlives the tray.
- **Layout:** `src/ui.rs` → `src/ui/{mod,tray,settings}.rs` (matches `capture/`, `encode/`,
  etc.; `lib.rs` `pub mod ui` unchanged; `ui::Shell` public surface unchanged). Tray unit tests
  moved intact + an `OpenSettings` mapping assertion added.
- **Cold-open < 300 ms (M7 acceptance)** is instrumented (a `cold_open_ms` field on the
  `settings window first frame` log event) but is a **hardware measurement** — not claimed from
  this build. New `just` recipes: none.
- **Post-implementation rust-reviewer pass (static, sandboxed) hardened two lifecycle edges:**
  (1) `open()` now detects a dead UI thread (`thread.is_finished()` — e.g. `run_native` failing
  to make a window/GL context on a VM/RDP/restrictive driver) and disables Settings for the
  session with a logged reason, instead of silently no-opping every future click (no respawn —
  one event loop per process). (2) `shutdown()`'s `join` is now **bounded** (`SHUTDOWN_JOIN_
  TIMEOUT` = 500 ms) with a detach fallback, so a window wedged in a native modal loop (mid
  drag/resize) at Quit cannot stall process exit. Also: context is published synchronously from
  `CreationContext.egui_ctx` (not on first frame), removing a show/close race; `open` takes
  `&Sender` to skip a clone on re-show. Reviewer verified `send_viewport_cmd`/`request_repaint`
  are sound cross-thread (queue into an internally-locked command buffer, no foreign-thread HWND
  touch) against the pinned egui 0.35 source.

### A2 HW validation (Nitro V15, 2026-07-07) + cold-open budget amendment

A2 self-tested on the Nitro (release binary). **Functional lifecycle PASSES on hardware:**
- Window opens on the dGPU (glow/WGL, RTX 4050 Laptop, GL 3.3.0 NVIDIA 576.02).
- Close (X) → `CancelClose` + hidden; re-click → `settings window re-shown` — **no second-event-
  loop panic** (the one-loop-per-process reopen model holds on real hardware).
- Save (`Ctrl+Alt+S`) with the window open → `clip saved … ms=509`; the engine ran unaffected
  under a live UI thread (satellite law holds in practice, not just structurally).
- Tray **Quit** with the window open → clean teardown (`CancelClose was not sent` → loop exits →
  `eframe window closed` → `settings window closed` → engine shutdown → audio/hotkeys stop). **No
  hang**; the bounded-join fallback was not even needed.

**Cold-open: MEASURED 385 ms (release) / 528 ms (debug) vs the 08-FEATURE-COMPLETE < 300 ms M7
target — OVER by ~85 ms. DECISION: accept + document (constraint-7 budget amendment,
orchestrator-approved).** Root cause is driver-bound and one-time: **~338 ms of the total is the
NVIDIA driver creating its first WGL/OpenGL context on the Optimus dGPU** (glutin display +
pixel-format pick); optimization does not touch it — release only shaved the ~190 ms of egui
shader/VAO/first-paint init (528 → 385). It is a **first-open-only** cost: every reopen is
instant (the window persists hidden — verified). The budget's real intent (the UI never stalls
the engine — it is a separate thread) is met everywhere; only the very first window paint is late.

**Rejected: pre-warming a hidden GL context at buffer startup** (would make the first open
instant). Orchestrator-declined — it holds **~30–60 MB dGPU VRAM + a parked thread for the whole
session even if the user never opens Settings**, to optimize exactly one event per session;
violates YAGNI (constraint 8) and the plan's "lazily created from the tray" intent. **Reversible:**
if beta users report the first open feels slow, add pre-warm behind a config flag (opt-in) later.
(Bounding context: the engine already runs D3D11 capture + NVENC on that same dGPU all session,
so a GL context would be incremental, not waking an idle GPU.)

### 2026-07-07 — A3 (VU meters)

VU meters for the two current audio streams (desktop-loopback + mic) in the settings window.
No new dependency (uses whitelisted atomics + the already-sanctioned egui). New module
`src/audio/levels.rs` (pure + safe + 11 unit tests). Choices, all reversible:

- **Level path is a lock-free `Arc<AudioLevels>` keyed by `AudioStreamKind`** — an `AtomicU32`
  pair (peak, rms as f32 bit patterns) per stream, `Relaxed`. The engine's audio-process
  threads PUBLISH; the settings window READS. It deliberately does NOT go through `ShellSignal`
  (the tray's single, state-only consumer). Satellite-law direction stays `ui → engine`
  (`AudioLevels` lives in `audio`; `ui` only holds a clone of the `Arc` and reads). `Relaxed`
  is sound: peak/rms are independent scalars with no cross-field invariant and gate no other
  memory. Keyed by *kind*, not index, so there is zero producer/consumer index coupling.
- **Computed on the raw captured `AudioPacket` (native f32), once per packet, before resample.**
  Resampling barely moves amplitude and the packet is already in hand (no extra copy). Silence-
  flagged packets skip the scan and publish zero. Cost is a single ~1k-sample pass per 10 ms per
  stream — negligible vs the §6.4 audio-CPU budget.
- **Store-latest (not a fetch_max peak-hold).** A VU meter tolerates missing a sub-33 ms
  transient between the ~100 Hz publish and the 30 fps read; store-latest avoids reader/writer
  coupling and a stale-peak spike on window reopen. The "fast tip" comes from the UI's
  instant-attack / slow-release animation (`release_toward`, pure + tested), not from the
  publish side.
- **Meter animation is repaint-gated on a shared `visible` flag** (`Shared.visible`, set by the
  tray on re-show, cleared by the app on close-intercept). A hidden (closed-to-tray) window
  idles at zero CPU; a stale post-hide repaint sees `false` and lets egui idle rather than
  resurrecting a 30 fps spin. The flag — not an inferred per-frame heuristic — is the single
  source of truth for "should animate".
- **`enabled_audio_kinds(params)` is the one source of truth** for both the supervisor's capture
  list and the shell's meter set, so the two can never drift. The `levels` `Arc` is created in
  `BufferEngine::start` (main thread, before the supervisor spawns) so the shell can clone it
  synchronously, and is cloned into every producer set — it survives §7 epoch rebuilds.

Grows to N tracks in Slice B (B1) by widening `AudioStreamKind` + `AudioLevels` together; nothing
else changes. **HW-VALIDATED on the Nitro (2026-07-07):** both meters track their stream (desktop
follows system audio, mic follows speech) and decay to silence when quiet — A3 acceptance met.

### 2026-07-07 — A4 (status strip)

An engine-status panel in the settings window: engine state, buffer fill (seconds held vs
configured + current MiB), capture target, resolution/fps/codec/GPU, stage + dropped-frame
counters, and the last-save result/time. No new dependency (whitelisted std atomics + the
already-sanctioned egui). New pure/safe module `src/status.rs` (11 unit tests). Choices, all
reversible:

- **Status path is a lock-free `Arc<EngineStatus>`, engine PUBLISHES → UI READS — the same shape
  as A3's `AudioLevels`, deliberately NOT `ShellSignal`** (that channel is the tray's single,
  state-only consumer). An immutable header (GPU adapter as `Arc<str>`, fps, configured buffer
  seconds — all known at `BufferEngine::start`) plus per-field atomics (`Relaxed`) for the live
  cells. `Relaxed` is sound: the fields are independent display scalars with no cross-field
  invariant and gate no other memory; the UI takes one decoded `snapshot()` per frame. Satellite
  direction stays `ui → engine` (`status.rs` references nothing under `ui`).
- **The data spans three engine threads; one `Arc` fans out to all of them.** Ring thread
  publishes state (at each transition, alongside the existing `ShellSignal` sends) + buffer fill
  + stage counts (on the 500 ms watchdog tick). Capture thread publishes resolution + capture
  target (at canvas init, and again on a window→monitor fall-back, both no-epoch) + dropped
  frames. Mux worker publishes the last-save outcome (Ok/Failed + wall-clock ms + write duration).
  The supervisor publishes `Error` on a fatal teardown, mirroring the tray's `any_worker_finished`.
  The `Arc` is created in `BufferEngine::start` (before the supervisor moves `gpu`) and survives §7
  epoch rebuilds (each respawned capture thread gets a fresh clone).
- **Dropped frames accumulate as a DELTA, not an absolute store** (`add_dropped`, fetch_add).
  Each epoch's capture thread owns a fresh `PacingGrid` whose drop count restarts at 0 on a
  device-loss respawn; a `store` of the new grid's smaller count would silently erase prior epochs'
  drops. Forwarding each thread's own increments keeps the session total genuinely cumulative
  across rebuilds (caught in rust-reviewer; the original `set_dropped` shipped the doc-vs-behavior
  mismatch this fixes). `captured`/`encoded`/`muxed` reuse the existing `Arc`-atomic `PipelineStats`
  (created once, survives rebuilds) — published into the status on the same tick.
- **Codec is the hardwired "H.264"; the "vendor" readout is the GPU adapter description**
  (`GpuContext.adapter_description`), not the H.264 MFT friendly name. Reading the MFT
  `MFT_FRIENDLY_NAME_Attribute` would add COM plumbing for a cosmetic string (YAGNI); the adapter
  is the device NVENC runs on and is already in hand. A real per-MFT vendor row can come later if
  a task calls for it.
- **Last-save time is stored as a Unix-ms stamp and formatted RELATIVE to now by the UI**
  ("12 s ago", "3 m ago" — pure `format_elapsed`, unit-tested). Avoids timezone/locale formatting
  with no calendar dependency (the whitelist has no `chrono`); the UI reads its own wall clock and
  saturating-subtracts. A requested-but-skipped save (young buffer / config not ready) publishes
  `Failed` so the strip never shows a stale prior success.
- **The panel rides the A3 visibility-gated 30 fps repaint** (unchanged): a hidden window idles at
  zero CPU; while visible, the status refreshes with the meters. Derived mappings
  (`ticks_to_seconds`/`bytes_to_mib`/`fill_fraction`/`format_elapsed`) are pure + unit-tested like
  `levels.rs`.

Grows to N tracks / richer fields in Slice B alongside the rest. Release binary **8,714,240 bytes
(8.31 MB)** vs the 10 MB budget — **+10.5 KB from A3's 8.30 MB** (the status code is tiny). 208
tests (+11). `just check` + `just test` green. **NOT yet HW-validated** — see the A4 checklist in
HANDOVER §5.

### 2026-07-07 — A5 (settings editor)

The settings-window editor: quality tier, resolution, fps, buffer length, output folder,
clear-after-save, desktop audio, and mic policy — edited in a draft `Config` and persisted through
the A1 `Config::write_atomic` path (the single config representation, same typed path as
`--check-config`). No new dependency. All new pure logic unit-tested in `ui/settings.rs` (+8 tests).
Choices, all reversible:

- **Apply model = hot-swap the one field that is genuinely safe, restart-note the rest**
  (orchestrator picked "hot-swap what's safe, else restart-note"). The classification:
  - **`clear_after_save` hot-swaps** via a new `EngineCommand::SetClearAfterSave(bool)`. It is the
    only editable field with zero pipeline side effects — it changes only what the *next* save does
    (whether it clears the ring), is read in exactly one place (`trigger_save`), and the ring
    thread owns its `RingThreadConfig` exclusively (single consumer, no lock, no race). The ring
    thread's `cfg` became `mut` and the new command mutates `cfg.clear_after_save` in-thread.
  - **Everything else is restart-note:** quality/resolution/fps rebuild the encoder or capture
    canvas (epoch-level); buffer length would require live ring-cap resizing + eviction (risky);
    output dir has a second consumer (the tray's "Open folder") and would split-brain; device/mic
    changes rebuild the audio producers. On save, the editor lists exactly which changed fields
    need a restart. Live-applying these is a clean future task, not a v0-beta need.
- **`EngineCommand` dropped `Copy` (now `Clone` only).** A live-apply command may carry an owned
  payload (the future output-dir `PathBuf`), and every variant is only ever sent or matched by
  value, so nothing relied on `Copy`.
- **Mic device selection ships as a policy dropdown {Default (follow) | Off} + an advanced
  pinned-endpoint-id text field, NOT a full enumerated device list.** Rationale (ambiguity
  contract §3 — simpler + reversible + logged): `audio/devices.rs` has no endpoint-enumeration API;
  adding WASAPI `EnumAudioEndpoints` + property-store friendly-name reads is new confined-unsafe COM
  whose device-name output is only verifiable on hardware and pulls in several new `windows` feature
  gates. Desktop loopback follows the default render endpoint (not per-device in v1, §2.5), so it is
  a plain on/off toggle. A full enumerated picker is a clean fast-follow once the enumeration wrapper
  is written + HW-validated (flagged in HANDOVER).
- **Derived feedback is composed from the SAME spec functions the engine uses, so the numbers are
  honest.** "≈ N Mbps video" = `encoder::video_target_bitrate_bps` at the selected resolution tier
  (native estimated at 1080p, the common beta display). "buffer ≈ N s / X MiB RAM" =
  `ring::byte_cap_bytes` at a nominal 1080p over `buffer_seconds + one GOP` — mirroring the engine's
  actual byte cap exactly (nominal-1080 + the retained-GOP margin), so the estimate matches the real
  reservation rather than under-reporting it.
- **fps picker offers 30/60 only** — 120 stays gated behind M6 (M7-M8-PLAN §1.2), even though
  `Config::validate` permits it for a hand-edited TOML.
- **Invalid edits surface `Config::validate`'s exact error string** (the same text `--check-config`
  prints) and write nothing; the draft is only committed to disk after it validates. Config loads on
  window open (a missing/invalid file falls back to defaults, never silently overwritten).

Release binary **9,199,616 bytes (8.77 MB)** vs the 10 MB budget — **+474 KB from A4's 8.31 MB**
(the config/`toml_edit` write paths + egui ComboBox/Grid/DragValue/TextEdit widget code became
reachable). 216 tests (+8). `just check` + `just test` green. **NOT yet HW-validated** — see the A5
checklist in HANDOVER §5.

### 2026-07-07 — A6 (press-to-bind hotkeys)

Press-to-bind rebinding for the save-clip and record-toggle hotkeys in the settings editor: the user
presses a combo and it's captured, instead of hand-typing an accelerator string. No new dependency,
no engine change; all new logic pure + unit-tested in `ui/settings.rs` (+4 tests). Choices:

- **Capture → canonical accelerator string → `parse_hotkey`-validated.** `accelerator_from` maps an
  egui `Modifiers`+`Key` to a `keyboard-types` `Code` string (`Ctrl+Alt+KeyS`, `Ctrl+F9`) and only
  returns `Some` if `hotkey::parse_hotkey` actually parses it. **Ctrl or Alt is required** (stricter
  than global-hotkey, which accepts a bare `F9`): press-to-bind refuses bare-key / Shift-only combos
  so a global hotkey can't hijack an ordinary keystroke. Bare function keys stay hand-settable in
  TOML. The matched key event is *consumed* so no other focused widget also reacts (rust-reviewer).
- **Restart-noted, NOT live-reregistered.** A rebind writes `[hotkeys]` via `Config::write_atomic`
  and is applied on the next start, where `HotkeyPump::spawn` re-registers and its existing tolerant
  register already logs an OS conflict ("already in use by another app"). **Live re-registration +
  live "combo already taken" detection are deferred:** the `HotkeyPump` lives in `main.rs` on its own
  message-pump thread (separate from the engine's `EngineCommand` channel and `RegisterHotKey` is
  thread-affine), so a live rebind needs a dedicated cross-thread pump-control request/response — real
  plumbing for a marginal v0 gain. Flagged in HANDOVER as a clean fast-follow. The "re-default
  record_toggle on persistent conflict" nicety rides on that same deferred live-detection.
- **Hotkey validation stays UI-side (the editor's `validate_hotkeys`), NOT in `Config::validate`.**
  Both bindings must parse and must differ — compared as PARSED `HotKey` values, not raw strings, so
  `Alt+Ctrl+S` vs `Ctrl+Alt+S` are caught as the same binding (rust-reviewer). It is deliberately not
  folded into `Config::validate` because `Config::load(..).unwrap_or_default()` (main.rs buffer start
  + the editor open) would then silently DISCARD a user's entire config on one bad hotkey — strictly
  worse than the pump's clear fatal-at-startup parse error plus this write-side guard. So the split is:
  the editor guards what it *writes*; the pump enforces on *read* at startup. `--check-config` still
  doesn't flag a hand-edited bad `[hotkeys]` (unchanged pre-existing behavior; documented here).
- **Known v0 limitation:** while the settings window is focused and capturing, the OS-global save/
  record hotkey stays registered, so pressing the *current* combo to rebind it still fires the real
  action (a save/record). Accepted for v0, commented in `draw_hotkeys` — inherent to rebinding
  system-wide hotkeys without live unregister.

Release binary **9,204,736 bytes (8.78 MB)** vs the 10 MB budget — **+5 KB from A5's 8.77 MB**
(pure logic + a few widgets). 220 tests (+4). `just check` + `just test` green. **NOT yet
HW-validated** — see the A6 checklist in HANDOVER §5.

### 2026-07-07 — A7 (recent clips list)

A "Recent clips" list at the bottom of the settings window: the last 20 saved clips, each with
**Open / Folder (reveal) / Copy path**. No editor, no thumbnails (explicit non-goals). No new
dependency, no engine change; new module `src/ui/recent.rs` (+4 tests). Choices:

- **Source of truth = scan the engine's resolved output dir for `{PRODUCT_NAME}_*.mp4`, files only,
  newest-first (mtime), take 20.** No new persisted state, no engine coupling. The dir is the tray's
  `output_dir` (the one `main.rs` resolved and the engine actually saves to) threaded into the window
  — NOT re-derived from `config.output.dir`, so it matches where clips really land even if the editor
  has an unsaved/restart-pending dir change. Filter/sort/take is pure + unit-tested; only `read_dir` +
  the Explorer shell-outs touch the OS. **Directories/symlinks named like a clip are excluded**
  (`metadata().is_file()` — rust-reviewer).
- **Re-scanned on every window re-show, not just the one-time cold open.** The window persists hidden
  across opens (A2 model), so a once-at-construction scan would go stale after the first clip saved
  while hidden — the exact between-sessions case this feature exists for (rust-reviewer). The tray
  sets a `Shared.rescan_recent` flag on the re-show path; the app swaps it and re-scans on the next
  frame. Plus a manual Refresh button. It does not live-watch the filesystem (YAGNI).
- **Actions shell out to Explorer** (consistent with the tray's existing "Open clips folder"): Open =
  `explorer <file>` (default handler), Folder = `explorer /select,<path>` (reveal + select), Copy
  path = egui `ctx.copy_text`. `Command::arg` bypasses the shell, so a path with spaces/specials is
  safe (no injection); a detached child per click is fine on Windows.

Release binary **9,235,456 bytes (8.81 MB)** vs the 10 MB budget — **+30.7 KB from A6's 8.78 MB**.
224 tests (+4). `just check` + `just test` green. **NOT yet HW-validated** — see the A7 checklist in
HANDOVER §5. After A8, Slice A is done → friends-beta v0.

### 2026-07-07 — A8 (friends-beta packaging) — closes Slice A

The lean M10 cut: a `just dist` recipe that packages a portable friends-beta zip. No new
dependency; the only Rust added is one drift-guard test. Choices:

- **New `just dist` recipe** (noted here per devflow — the justfile grows only via a DECISIONS
  entry). It depends on `release` (so the stripped build + 10 MB budget check run first), then stages
  `clipd.exe` + `dist/QUICKSTART.txt` + `dist/config.template.toml` into `target/dist/clipd-v<ver>/`
  and `Compress-Archive`s that FOLDER (so the zip unpacks to one clean `clipd-v<ver>/` directory).
  Version comes from `Cargo.toml`. Output (`target/dist/…`) is gitignored; the sources live in
  committed `dist/`. Verified end-to-end: produces `clipd-v0.0.0.zip` (~3.85 MB compressed) with the
  three files. **No signing / winget / installer** — those are M10.
- **Default-config template = a hand-curated, commented `dist/config.template.toml`**, not a
  generated `Config::default().to_toml()` dump. Rationale: a friends-beta needs the *comments*
  (what each quality/resolution/audio/hotkey key does) far more than it needs machine-generated
  output. Drift risk is closed by a test (`config::tests::shipped_config_template_matches_defaults`)
  that `include_str!`-loads the template, asserts it parses + validates, and asserts it equals
  `Config::default()` — so a changed schema default, a bad value, or a typo fails CI. No
  `--emit-default-config` code helper was needed.
- **One-page `dist/QUICKSTART.txt`** (plain text, opens in Notepad): what clipd is, the **SmartScreen
  "unknown publisher → More info → Run anyway"** note (unsigned build), the tray model, the default
  hotkeys (Ctrl+Alt+S / Ctrl+Alt+F9), where clips land + Recent-clips/Open-folder, the config path
  (`%APPDATA%\clipd\config.toml`) + template, and the log path (`%LOCALAPPDATA%\clipd\logs`).

Release binary unchanged from A7 (**8.81 MB**, 9,235,456 bytes; A8 adds no shipping code). 225 tests
(+1, the template drift guard). `just check` + `just test` green; `just dist` verified. **HW step for
A8:** none beyond "unzip on a clean machine, SmartScreen → Run anyway, it runs" — folded into the
friends-beta rollout. **Slice A (A1–A8) is COMPLETE → friends-beta v0 (2-track, full UI).**

### 2026-07-08 — A6 fast-follow: live "combo already taken" detection (closes half the A6 deferral)

Delivered the first of the two A6-flagged deferrals (the other, live *re-registration* of the working
hotkey, stays deferred — see below). The settings editor now tells the user, at bind time, whether a
freshly-pressed combo is already owned by another application, instead of only finding out from a log
line at the next restart. No new dependency (`crossbeam-channel` already whitelisted), no engine
change, no async. Choices:

- **A cross-thread pump-control channel, not live re-registration.** `HotkeyPump` (in `main.rs`, own
  message-pump thread) gains a `crossbeam_channel` control queue + a cloneable `HotkeyControl` handle
  (`hotkey.rs`). The editor's `HotkeyControl::check(combo)` sends a request and returns a
  `Receiver<Availability>` immediately; the pump thread is woken by a private `WM_APP`
  (`WM_HOTKEY_CONTROL`) thread message posted via `PostThreadMessageW`, drains the queue, and
  **test-registers** the candidate on the thread that owns the (`!Send`) `GlobalHotKeyManager`. A free
  combo registers → we immediately `unregister` it (the probe never holds the binding — the real one
  is still applied from config at restart) → `Available`; an OS conflict → `Taken`.
- **"Our own combo" short-circuits to Available.** The pump keeps the ids it actually registered at
  startup; a probed combo whose `HotKey::id()` matches one we hold returns `Available` without a
  register attempt (else re-probing the current binding would self-conflict and look "taken"). A combo
  that FAILED to register at startup (already owned by another app) is not in that set, so it correctly
  falls through to a real `Taken`.
- **UI never blocks on the pump.** `check` is fire-and-poll: `Editor::poll_availability` does a
  `try_recv` once per frame (the A3 meter cadence already repaints while visible, so the note appears
  within a frame); a disconnected channel (pump gone) resolves to `Unknown` ("couldn't check"), never
  a hang. The probe fires once per bind, not per frame. Direction stays `ui → engine`/pump; the pump
  never references `ui`.
- **Surface, don't block.** A `Taken` combo still saves (the owning app may close later; re-registration
  at restart already logs + tolerates it) — Save is not gated on availability, matching the existing
  tolerant-register behavior. The row shows `⚠ in use by another app` / `✓ available` / `(couldn't
  check)`.
- **Deferred to the post-Slice-B UI pass (orchestrator decision, 2026-07-08):** live re-registration
  of the *working* hotkey without a restart, and — folded into the SAME decision as its dependent —
  "re-default `record_toggle` on persistent conflict". Both need the engine to learn the new
  `HotKey::id()` live (ids are frozen at `BufferEngine::start`); the re-default also auto-mutates user
  config on a persistent startup conflict, which only makes sense if live re-registration exists. **Not
  implemented now and not automatically owed:** the orchestrator will DECIDE whether to build them
  during the planning of the UI pass that precedes the final friend release (see M7-M8-PLAN §7). Until
  then, restart-to-apply for the actual binding is retained; the live bind-time check already steers
  users to a free combo, which is why neither is needed for the betas.
- **rust-reviewer pass (2026-07-08):** approved with fixes applied — control channel made `bounded(8)`
  + `try_send` (no unbounded channels; a full queue drops to `Unknown`, never blocks the UI), the
  probe's `unregister` failure now `warn!`s (was silently discarded — could leak a slot + misreport
  `Taken`), `#[must_use]` on `check`, and the spurious-`WM_HOTKEY` safety property documented on
  `check_availability`. **Accepted (cosmetic, not fixed):** the live probe short-circuits to `Available`
  whenever a candidate equals ANY currently-held id, so rebinding one row to the *other* row's current
  combo shows `✓ available` even though `Editor::validate_hotkeys` blocks that at Save time — a
  save==record cross-conflict is still correctly refused on Save with its clear message; teaching the
  probe per-target ids to catch it live isn't worth the pump↔target coupling.

`hotkey.rs` `unsafe` is unchanged in kind (one more `PostThreadMessageW` cross-thread post, same
`SAFETY:` rationale as `request_quit`); no `unsafe` entered any logic/UI module.

**First-run UI fixes (2026-07-08, same day, after a manual smoke on the settings window):**

- **Bindings now store/show the human token (`Ctrl+Alt+K`, not `Ctrl+Alt+KeyK`).** `key_to_code` →
  `key_to_token` emits `K`/`1`/`F9` instead of `KeyK`/`Digit1`. `global-hotkey`'s parser accepts the
  short and long forms as the SAME `Code` (crate `parse_key`: `"KEYA" | "A"`, `"DIGIT1" | "1"`), so the
  produced `HotKey`/id — and thus `validate_hotkeys` (compares parsed) and the availability id-match —
  are identical; the change is purely cosmetic and now matches the shipped `Ctrl+Alt+S` defaults.
  Guarded by `pretty_and_code_forms_are_the_same_hotkey`.
- **The binding is now an editable text field, not read-only text.** Root cause of "pressing a taken
  combo does nothing / no feedback": a combo already registered as a global hotkey by another app is
  consumed by Windows and delivered as `WM_HOTKEY` to that app — it never reaches the egui window, so
  press-to-bind physically cannot capture it. Fix: each row shows the combo in a monospace `TextEdit`
  (plus the Rebind press-to-bind button, still the quick path for free combos). Typing a parseable
  combo fires the same live availability probe, so an OS-claimed combo now surfaces `⚠ in use by
  another app`. While capturing, the prompt tells the user taken combos must be typed. Bad text is
  still caught on Save by `validate_hotkeys`. No temporary-unregister of our own hotkeys was added
  (the text field already lets the user set any combo, incl. our own) — a possible future nicety.

228 tests (+3 across the fast-follow: `hotkey_target_idx_is_distinct`,
`availability_check_is_a_noop_without_a_pump`, `pretty_and_code_forms_are_the_same_hotkey`; the two
`key_to_token`/`accelerator_from` tests updated to the pretty form). `just check` + `just test` green.
**HW validation is a STANDALONE gate for this fast-follow** (not folded into the batched A4–A8 pass) —
see "A6 FAST-FOLLOW HARDWARE TEST" in HANDOVER §5; the item closes only after it passes on the Nitro.

---

## 2026-07-08 — A5 fast-follow: output-folder verified on save + Videos default (HW batch finding)

**Found in the batched A4–A8 HW pass (Nitro):** the settings editor accepted a bogus output folder
(`ddddddddd`) and wrote it; every later clip save then failed with `mux I/O error: The system cannot
find the path specified. (os error 3)`. It was logged (`clip save FAILED`) and the A4 status strip
showed "failed", but the editor gave no feedback at save-config time — the "why didn't my clip save?"
trust trap this project exists to kill. Root cause: `main.rs` resolved `[output].dir` to a `PathBuf`
and **never created it** (unlike `logging.rs`, which `create_dir_all`s its log dir), and nothing
validated the path on the editor's write path. Also surfaced a **doc-vs-behavior bug**: `OutputConfig::dir`'s
doc said "empty = OS Videos folder" but the empty case actually resolved to `std::env::current_dir()`.

**Fix (branch `a5-ff-output-dir`):**
- **`config.rs` (pure, unit-tested):** `default_output_dir()` = `%USERPROFILE%\Videos\{PRODUCT_NAME}`
  (fallback CWD if `%USERPROFILE%` unset) + `resolve_output_dir(&str)` (empty/whitespace → the Videos
  default; else verbatim, trimmed). This makes the empty-dir default match its long-standing doc
  comment. **Videos folder via the `%USERPROFILE%` env var, NOT a `windows`/Shell known-folder call**
  (`SHGetKnownFolderPath(FOLDERID_Videos)`): the env-var form is the same convention already used for
  `%APPDATA%` (config) and `%LOCALAPPDATA%` (logs), stays pure + testable, adds no `Win32_UI_Shell`
  feature or confined-unsafe COM for one path, and resolves the Videos library correctly in the normal
  case. Tie-break rule applied (simpler + reversible; CLAUDE.md §3). Trade-off documented: a *relocated*
  Videos library isn't followed — acceptable for a friends-beta; flip to the known-folder API later if
  a tester actually needs it.
- **`main.rs` (shell, I/O + logging):** `prepare_output_dir(cfg_dir)` resolves, `create_dir_all`s the
  directory, and on failure **logs + falls back to the Videos default** (also created) so a mistyped
  folder can never silently break every save. Both the `buffer` and `record` resolution sites now call
  it (they were duplicated inline `if empty { current_dir } else { … }` blocks).
- **`ui/settings.rs` (editor write path):** `Editor::validate_output_dir()` runs in `save()` AFTER
  `Config::validate` and BEFORE `write_atomic` — it `create_dir_all`s the resolved dir; on failure it
  surfaces the exact I/O error in red (`output folder: <path> — <err>`) and writes nothing. Per the
  orchestrator's 2026-07-08 call: **create it if missing, reject only if uncreatable**; an empty field
  resolves to (and materialises) the Videos default. **Deliberately NOT added to `Config::validate`** —
  a "dir must exist" check there would make `Config::load(..).unwrap_or_default()` silently discard a
  whole user config when a saved output drive is unplugged (the exact trap A6's hotkey validation is
  kept out of `validate` to avoid).

The out-of-scope half of the same HW note — the mic *device id* isn't checked to exist — is left to
Slice B's `B3.5` (WASAPI `EnumAudioEndpoints` wrapper rides the B2/B7 audio-COM HW cycle), where a real
device list replaces the free-text id field. Not a regression.

231 tests (+3: `resolve_output_dir_empty_follows_videos_default`,
`resolve_output_dir_explicit_is_verbatim`, `validate_output_dir_creates_missing_and_rejects_uncreatable`).
`just check` + `just test` green. No `unsafe` touched; no new dependency. **HW-validated (Nitro,
2026-07-08):** bad folder → red error, nothing written; good/blank folder → clips land (blank →
`…\Videos\clipd`). Item CLOSED.

---

## 2026-07-08 — A6 fast-follow #2: live badge now catches the cross-row hotkey duplicate (HW finding)

**Found in the A6 HW pass (Nitro):** typing `Ctrl+Alt+S` (the Save binding) into the **Record** field
showed a green `✓ available`, and vice-versa — the live badge said "free" for a combo that is exactly
the other row's binding. **Root cause (`hotkey.rs::check_availability`):** the pump answers the probe by
test-registering the combo, but first short-circuits `if registered_ids.contains(&hotkey.id()) →
Available` so re-binding your *own* combo isn't a false "taken". `registered_ids` holds BOTH the save and
record ids (both registered at startup), so the probe cannot distinguish "this row's own binding" from
"the other row's binding" — it returns `Available` for either. The pump structurally can't see a
cross-row conflict; it must be caught UI-side.

**Fix (branch `a6-ff-cross-conflict`, `ui/settings.rs` only):** `Editor::cross_conflict_note(target)`
compares the two rows' **parsed** `HotKey`s (so `Alt+Ctrl+S` == `Ctrl+Alt+S`); on a match it returns
`⚠ same as {other row label}` and the row draws it in red **with precedence over** the probe's
`Availability`. `validate_hotkeys` already refused this on Save (unchanged); this only stops the live
badge from lying before Save. No pump/`hotkey.rs` change — keeping the fix UI-side avoids coupling the
probe to per-target ids (and matches the "hotkey validation is UI-side only" rule; DECISIONS "A6").

232 tests (+1: `cross_conflict_note_catches_duplicate_both_ways`). `just check` + `just test` green. No
`unsafe`, no new dependency. **HW-validated (Nitro, 2026-07-08):** typing one row's combo into the other
now shows `⚠ same as …` in red instead of a false `✓ available`, both directions. Item CLOSED.

---

## 2026-07-08 — Slice B planning: D1/D2 locked (no code)

`SLICE-B-PLAN.md` (repo root) written from a full read of the code + specs — the
working plan for Slice B (B1–B7 + B3.5, 4-track audio). Two decisions locked by the
orchestrator this session; the rest (D3–D6 in the plan) are agent's-call under the
ambiguity rules.

- **D1 — `separate_tracks` semantics change + default flip.** Through Slice A,
  `separate_tracks` defaulted to `true` = {desktop, mic}. Slice B redefines it:
  **`false` = mix + mic (2 tracks), `true` = full 5-track topology** (mix / game /
  voice-chat / other-system / mic per `M7-M8-PLAN §2`), and the **default flips to
  `false`**. So the default saved clip changes from {desktop, mic} to {mix, mic} —
  mix+mic preserves "my voice recoverable in post" while staying CapCut/upload-safe
  (mix is track 1). Migration (pre-1.0 friends-beta, no `config_version` bump): the
  key is honored under the new meaning with the new `false` default; a hand-written
  `separate_tracks = true` from Slice A now yields the full 5-track set (acceptable —
  they asked for separate tracks). B1 updates `AudioConfig::default()`, the config
  template, and the `--check-config` wording; B6 documents it. **Reversible:** it is a
  default + a match on one bool.
- **D2 — B1 track-1 interim = pass-through, real sum in B4.** Between B1 and B4,
  track 1 ("Mix") passes through the raw default-endpoint loopback so B1 is CI-green
  and independently mergeable and the working desktop path never regresses mid-slice;
  the −3 dB soft-clipped sum(endpoint, mic) lands in B4.

No code changed this session. Next session begins at **B1** (`SLICE-B-PLAN.md §3`).

---

## 2026-07-08 — Slice B / B1: N-track model (sources ≠ tracks) implemented

Branch `b1-track-model`, local-green (`just check` + `just test` = **241 tests**, +9;
`just release` **8.85 MB** vs 10 MB budget). No new dependency, no new `unsafe`. Pure-logic
+ wiring; **no HW step** (folds into B7). rust-reviewer'd (5 findings, all addressed;
none blocking).

- **The rename + the split.** `AudioStreamKind{Desktop, Mic}` → **`AudioTrackKind`**
  with the 5-variant container-track model `{Mix, Game, VoiceChat, OtherSystem, Mic}`
  (`COUNT`/`index`/`label`/`title`; Mix=0 … Mic=4, the amended `§2.5` order). New
  **`AudioSource`** enum (`EndpointLoopback` · `MicEndpoint(DeviceSelection)` ·
  `ProcessLoopback{pid, include_tree}`) is the *source* side of the split — B1 wires the
  first two; `ProcessLoopback` is defined for B2 but not opened. The container/save/mux/
  ring were already N-generic (positional `track_index`), so the edit set was narrow, as
  the plan predicted.
- **D-B1 (agent's call, approved) — builder plans all 5, B1 spawns Mix + Mic only.**
  `planned_kinds(TrackModel)` is the pure, exhaustively-tested full-topology builder (Mix
  first, per-source system tracks under `separate_tracks`, Mic last). `b1_spawnable`
  gates what B1 can actually feed (Mix, Mic); Game/VoiceChat/OtherSystem are **planned but
  dropped** until their sources land (B2 process-loopback / B4 mixer), each logged once at
  start (`warn_deferred_tracks`). So the default path is a **pure no-behaviour-change
  refactor** (Mix pass-through ≡ the old Desktop track, D2), zero half-working 5-track
  output, nothing that needs hardware. `spawnable_streams`/`spawnable_kinds` are pure fns
  of the same immutable `BufferParams` → the supervisor's capture list and the shell's
  VU-meter set cannot drift.
- **D1 wired (and a finding).** `separate_tracks` was **schema-only through Slice A** —
  parsed/round-tripped but *never read by the engine* (the `config.rs` doc claiming it
  "toggles whether the mic track is written" was inaccurate). B1 wires it for the first
  time under the new semantics and **flips the default to `false`** (Mix+Mic). The config
  template + the drift test (`shipped_config_template_matches_defaults`) move with it.
- **Dead constant removal.** `spec_constants::audio::TRACK_DESKTOP`/`TRACK_MIC` (unused,
  and encoding the superseded 2-track order) were removed; `AudioTrackKind::index()` is now
  the single source of truth for container-track order (comment left at the old site).
- **Deferred to later B-tasks (unchanged by B1):** the ASC-complete save gate
  (`v.len() == num_audio`, `engine.rs`) still holds because B1's spawned set always equals
  `num_audio` — it only needs relaxing (D4) once conditional/late tracks exist (B2+).

---

## 2026-07-08 — Slice B / B2: process-loopback capture module + `run_capture` reshaped to `AudioSource`

Implements **B2** (`SLICE-B-PLAN.md §3`): a per-application (process-tree) WASAPI
loopback capture source. This is the process-loopback *spine* — it adds the
capability and the dispatch; **B3** binds a live PID (game/VC detection) to actually
spawn it. **No HW validation on this branch** (folds into the batched B7 Nitro cycle);
local-green only. `CLAUDE.md`/devpack normative; amended `§2.2` (QPCPosition master
domain) governs.

### What was built
- **New module `src/audio/process_loopback.rs`** — `run_process_capture(kind, pid,
  include_tree, tx, stop)`, same `AudioPacket`/`stop` contract as the endpoint path.
  Opens via `wasapi::AudioClient::new_application_loopback_client(pid, include_tree)`
  (all COM `unsafe` is inside the crate). The crippled loopback client can't
  `get_mixformat`, so it **requests a fixed 48 kHz f32 stereo format** (autoconvert on);
  the packet carries `sample_rate = 48 kHz` and the downstream resampler runs an
  identity ratio, while the `§2.4` drift controller still corrects off the QPC PTS vs
  sample count (QPC is the real device clock). `QPCPosition` is passed straight into the
  shared `PtsDeriver` (amended `§2.2`).
- **`run_capture` reshaped to consume an `AudioSource` directly** (was `(kind,
  DeviceSelection)`). It now dispatches: endpoint variants → the renamed
  `run_endpoint_capture` (unchanged device-rebuild body); `ProcessLoopback{pid,
  include_tree}` → the new module. The B1 `AudioSource::selection()` bridge shim is
  **retired**. Callers updated: `engine.rs` spawn loop passes `source` by value;
  `main.rs` `run_audio_probe` builds `AudioSource` per kind.
- **PID-liveness watchdog** — process exit ⇒ silence forever with no WASAPI error
  (`§5` research), so `run_process_capture` opens a `SYNCHRONIZE` handle
  (`OpenProcess`) and polls it each loop tick (`WaitForSingleObject(h, 0)`); on exit it
  ends the capture (track → silence, B3 may rebind). The decision latch (`is_dead`) is
  pure + tested; the handle wrapper is confined `unsafe`. Best-effort: if the PID can't
  be opened, capture continues without exit detection (the `stop` flag still ends it).
- **Activation serialization** — a module-level `static ACTIVATION_LOCK: Mutex<()>` is
  held across the `new_application_loopback_client` call only. Parallel
  `ActivateAudioInterfaceAsync` spam is a documented field hazard (froze OBS, `§5`).
- **Runtime floor probe** — `process_loopback_supported()` = build ≥ 19041 (Win10 2004;
  docs *claim* 20348 — the doc is wrong, `M7-M8-PLAN §5`). Uses `RtlGetVersion`
  (manifest-independent; `GetVersionEx` lies without an app manifest we don't ship).
  `build_supports_process_loopback` is pure + tested. `run_process_capture` refuses
  below the floor (track silent); B3's spawn gate will call the same probe to hide the
  per-app tracks. **Exposed `pub` for B3.**

### Decisions taken (CLAUDE.md ambiguity rule: simpler / more-logged / reversible)
- **Reshape `run_capture` rather than add a parallel entry point.** One dispatcher keeps
  the audio-capture thread source-agnostic and matches the plan's "reshape `run_capture`
  to consume the `AudioSource` directly". Reversible.
- **B2 does NOT flip `b1_spawnable`.** Runtime behaviour is unchanged (Mix+Mic
  pass-through) — process-loopback is dispatchable + probe-exercised but not spawned
  until B3 provides a PID binding. Keeps this branch a no-behaviour-change,
  independently-mergeable step (like B1). **D4** (ASC-complete save gate) therefore stays
  untouched — still no conditional/late tracks at runtime.
- **Process-exit / activation-failure / unsupported-OS all return `Ok(())`, never an
  engine error.** The response to "this app's audio is gone" is always the same (track
  silent + possible B3 rebind), exactly like the endpoint path's device-loss rebuild —
  so the audio-capture thread simply ends and the downstream `§2.3` synthesizer fills
  the hole. Logged at each exit.
- **New `windows` feature gates in the same commit that calls them** (07-DEVFLOW):
  `Wdk_System_SystemServices` + `Win32_System_SystemInformation` (RtlGetVersion +
  OSVERSIONINFOW); `Win32_System_Threading` was already enabled (OpenProcess /
  WaitForSingleObject / PROCESS_SYNCHRONIZE). **No new crate dependency** in the core.
- **New standalone tool crate `tools/audio-probe`** (own `[workspace]`, never linked into
  `clipd`; `wasapi` + `hound` dev-deps, free per CLAUDE.md rule 2). It re-implements the
  activation open sequence against `wasapi` directly (like `avrig` re-implements render)
  and is the **manual HW instrument for B2** (the module's pure logic is unit-tested
  in-tree; the COM path is HW-only). `just probe` runs it (self + 440 Hz self-tone). The
  tool's header carries the B7 checklist. Kept in lock-step with the module's open
  sequence by comment.
- **`AacEncoder::new(kind)` param (D6) left as-is** — cosmetic, out of B2 scope.

### Tests / gate (no HW)
- 5 new unit tests in `process_loopback.rs`: build-floor mapping (19040→false /
  19041→true / 22631→true), fixed-format builder (48 kHz f32 stereo, blockalign 8),
  liveness latch (exit → dead, stays dead), wait-failure → dead, buffer = 4 × 10 ms.
- `just check` (fmt + clippy -D warnings) green; `just test` **246** (was 241);
  `tools/audio-probe` compiles + clippy-clean. Release build vs the 10 MB budget checked.
- **Owed to B7 (Nitro):** the process-loopback COM path — QPCPosition epoch vs raw QPC;
  process-exit silence + liveness teardown; dead-PID activation HRESULT; same-PID double
  capture; Discord tray-minimized; serialized-activation no-deadlock. Per the tool header
  checklist.

## 2026-07-08 — Slice B / B3: live game / voice-chat PID binding (per-app tracks turned on)

Implements **B3** (`SLICE-B-PLAN.md §3`): decides *which process* feeds the
`AudioTrackKind::{Game, VoiceChat}` tracks, live, and flips those tracks on — B2 built
"capture one PID's tree"; B3 answers "which PID?" and drives B2's producer with it. This
is the first branch where the per-app process-loopback tracks actually **spawn at
runtime** (under `separate_tracks = true`, above the Win10-2004 floor). **No HW
validation on this branch** — like B1/B2 it merges CI-green with the COM/OS paths owed to
the batched **B7** Nitro cycle (`binding-probe` carries the checklist). `CLAUDE.md`/devpack
normative.

### What was built
- **New module `src/audio/binding.rs`** — the binding brain. Pure, exhaustively
  unit-tested (22 tests) over injected snapshots: `select_vc_pid` (case-insensitive image
  match; **top-most same-name** = a matched process whose parent is not also a match, i.e.
  the Electron main, include-tree; config-order first-app-wins; ties→lowest PID),
  `is_borderless_fullscreen` (window covers `rcMonitor` — distinguishes fullscreen from a
  taskbar-short maximized window), `classify_game` (monitor→foreground-fullscreen /
  window→captured PID, rejects system PIDs < 8), and the `BindingTracker` retarget state
  machine. **Confined-unsafe OS providers** (HW-exercised at B7): `enumerate_processes`
  (Toolhelp `CreateToolhelp32Snapshot`/`Process32*W`), `foreground_window`
  (`GetForegroundWindow`/`GetWindowRect`/`GetWindowThreadProcessId`/`MonitorFromWindow`/
  `GetMonitorInfoW`), `window_pid`. Every `unsafe` has a `// SAFETY:` note; no COM/handle
  crosses a thread.
- **Engine wiring (`engine.rs`)** — the `sources ≠ tracks` split gains a `TrackFeed`
  {`Static(AudioSource)` | `Bound(BoundRole)`} and `BoundRole` {`Game`, `VoiceChat`}.
  `b1_spawnable`/`track_source` retired for `spawnable_feed`/`track_feed` (pure over an
  injected `supported` flag) + the impure `spawnable_streams` that layers the live
  `process_loopback_supported()` gate on top. Game/VoiceChat are now spawnable (above the
  floor); **OtherSystem stays deferred to B4** (its endpoint↔process-exclude switch / D5
  is a B4 concern). A per-epoch **`binding_watcher_thread`** (panic-free; scans every
  600 ms, polls stop every 120 ms) publishes each role's target into a shared
  `BindingState`; each bound track's **`run_bound_capture`** loop runs B2's
  `run_process_capture` on the current PID and rebinds on retarget (generation-guarded
  arm/retarget race; `§2.3` silence-fills the gap). The watcher's liveness is the
  bound-captures' teardown guarantee (it interrupts their in-flight runs on epoch stop).
- **`BufferParams.vc_apps`** threaded from `config.audio.vc_apps` (both constructions in
  `main.rs`). `game_detect_for(CaptureSource)` maps monitor→foreground-fullscreen,
  Window/FocusedWindow→the captured/foreground PID.
- **`binding-probe` subcommand** (`just run -- binding-probe [SECS]`) — the B7 HW
  instrument. Runs the **exact** `binding::` OS providers + selectors (no re-implementation
  to drift, unlike the standalone `audio-probe` crate — chosen because the pure logic is
  in-tree and the OS reads are `windows`-crate, already linked) and prints the detected
  Game/VoiceChat PIDs live. Its doc comment carries the full B7 checklist.

### Decisions taken (CLAUDE.md ambiguity rule: simpler / more-logged / reversible)
- **D4 (ASC-complete save gate) NOT relaxed — the plan's premise doesn't hold under the
  fixed-slot model.** The save gate (`v.len() == num_audio`) waits for every track's ASC.
  The ASC is emitted **eagerly at `audio_process_thread` startup** from the AAC encoder's
  fixed 48 kHz/stereo config — **source-independent**, no audio needed. So every
  *spawnable* track (Game/VoiceChat included) delivers its ASC the moment its consumer
  thread starts, whether or not a PID is ever bound; `num_audio` stays fixed at supervisor
  start and the gate is naturally satisfied. What is conditional at runtime is the **PID
  binding under a fixed track slot**, not the track count — so the ring/save/mux need no
  change and no dynamic-track-count risk is taken. (Reversible; if a future need for truly
  late tracks appears, revisit.)
- **Fixed track slots, live PID rebinding** (the above, stated positively): a Game/VC
  *slot* exists for the whole session (from config); its *source* rebinds live via the
  watcher. Simpler than dynamic tracks and fits the already-N-generic ring/save/mux.
- **One binding watcher per epoch, not per role.** A single Toolhelp scan serves both
  roles (cheap, ~1 ms) and avoids two enumerations per tick. The watcher is deliberately
  panic-free so it can be the bound captures' teardown signal without a dead-thread hazard
  (the failure mode this project exists to kill).
- **Game detection = pure foreground + borderless-fullscreen, no title database** (hard
  non-goal intact). Monitor mode re-evaluates live; window mode binds the captured
  window's PID (fixed). System PIDs (< 8) are never bound (keeps the desktop/Program
  Manager from binding the kernel).
- **VC by process image name, never by window** (tray-minimized Discord has no window);
  **top-most same-name** picks the Electron main over a helper child; include-tree per the
  `VcApp` config.
- **`binding-probe` as a hidden clipd subcommand, not a standalone `/tools` crate.** Unlike
  `audio-probe` (which re-implements the WASAPI activation against `wasapi` directly), the
  binding logic is pure and in-tree and the OS reads use the already-linked `windows`
  crate — so a subcommand exercises the *exact* engine code path with zero drift risk.
- **New `windows` feature gate in the same commit that calls it** (07-DEVFLOW):
  `Win32_System_Diagnostics_ToolHelp` (process enumeration). Foreground/monitor APIs were
  already enabled (`Win32_UI_WindowsAndMessaging` + `Win32_Graphics_Gdi`). **No new crate
  dependency.**

### Tests / gate (no HW)
- **+22 unit tests in `binding.rs`** (VC top-most/case/order/disabled/tie/include-tree;
  fullscreen exact/oversize/maximized/windowed/zero-area; game window/monitor/off/system-
  PID; retarget bind/no-op/pid-change/unbind). **+2 engine tests reshaped** for the new
  `track_feed`/`spawnable` API (OS-support gate both ways; meter-set == spawn-set invariant
  over `supported × desktop × mic × separate`; `game_detect_for` monitor arms). **271
  tests** (was 246), all green; `just check` (fmt + clippy -D warnings) clean. Release
  build **9,337,856 bytes ≈ 8.91 MB** vs the 10 MB budget (+0.04 from B2).
- **rust-reviewer pass — 1 HIGH (fixed) + 1 LOW (fixed).** HIGH: a teardown TOCTOU in
  `run_bound_capture` — the watcher's teardown interrupt is a one-shot sweep, so a
  `run_stop` armed *after* that sweep (which saw `None`) would never be set and
  `run_process_capture` would block forever, hanging the epoch-restart `join()` (the
  live-thread mirror of the dead-thread failure this project exists to kill). **Fixed** by
  rechecking `cap_stop` alongside the existing `generation` guard right after arming — the
  sweep runs only once cap_stop is set, so an arm-before-sweep is caught by the sweep and an
  arm-after-sweep observes `cap_stop = true`; the run never starts unkillable. LOW: replaced
  a wildcard `match` arm in `warn_deferred_tracks` with explicit `Game | VoiceChat` (a future
  deferred variant must not inherit a misleading log reason). The retarget race, the
  `RoleSlot` lock-ordering (no nested locks), watcher panic-freedom, and the OS-provider
  `unsafe`/SAFETY comments were reviewed and confirmed correct.
- **Owed to B7 (Nitro):** the OS providers + live rebind — Discord tray-minimized
  detection; game bind on a borderless title; foreground/maximized false-bind rejection;
  retarget silence gap; the per-app tracks muxing correctly in an N-track clip (the
  empty-VC-track edge is **B5's** N-track/hybrid-moov concern). Per the `binding-probe`
  header checklist.

### Known gaps / follow-ons
- **An unbound-all-session per-app track is an empty audio track** in a `separate_tracks`
  clip (ASC present, zero AUs). Whether that muxes cleanly is **B5** (N-track + hybrid
  `moov`) territory and B7 validation — not exercised on the default (Mix+Mic) path, which
  is unchanged. Flagged for B5.
- **OtherSystem** remains deferred to B4 (source switch / D5).

## 2026-07-08 — Slice B / B4: software mixer for the Mix track (real sum, D2 interim retired)

Implements **B4** (`SLICE-B-PLAN.md §B4`): the always-first **Mix** track
(`AudioTrackKind::Mix`, container track 0) becomes the −3 dB soft-clipped **sum** of the
default-endpoint desktop loopback and the mic — replacing the B1/B2 **D2 interim** where
track 1 passed the raw desktop loopback through. This is the only non-trivial piece of the
default (`separate_tracks = false`) audio path. **No HW validation on this branch** — like
B1/B2/B3 it merges CI-green, its audio-COM behaviour owed to the batched **B7** Nitro
cycle. `CLAUDE.md`/devpack normative. Local-green: **285 tests** (+14), `just release`
**8.96 MB** vs the 10 MB budget (+0.05 from B3). **No new dependency** (the mixer is pure
std f32 math).

### What was built
- **New pure module `src/audio/mixer.rs`** — `TwoSourceMixer`: 100 % safe, deterministic,
  deadline-agnostic logic (14 unit tests incl. spec-shaped edges). Aligns two already-
  resampled, gap-filled, drift-corrected 48 kHz stereo streams on a shared **anchor** (the
  earliest first-chunk PTS) by converting each chunk's PTS to an absolute 48 kHz frame
  index, then sums frame-for-frame. `place` keeps each source contiguous (gap → silence-pad,
  overlap → trim); `drain` emits mixed frames up to `min(blocking-frontier, data-extent)`
  and **only ever advances a monotonic `emitted` cursor** — the emitted sample stream is one
  gap-free run, which is load-bearing because the downstream AAC encoder is a
  **sample-counting clock** (`mft_aac::stamp`: AU PTS = `anchor + au_index·frame_dur`), so
  any hole in the mix input would drift the whole track.
- **Gain stage:** `out = soft_clip((desktop + mic) · HEADROOM)`. `HEADROOM = 0.707_945_78`
  (−3 dB linear). `soft_clip` is exactly **unity for |x| ≤ 0.8** (so a −3 dB full-scale solo
  signal, 0.708, passes clean), a **C¹ cubic-Hermite knee** from 0.8→1.0 (unity slope
  entering, zero slope at the ±1 limit), then a hard ±1. Monotonic, bounded, odd — verified
  across a fine grid. Only genuine overshoot from summing is softened; normal levels are
  pristine.
- **Engine wiring (`engine.rs`):**
  - `TrackFeed` += `Mix { mic_present: bool }` (Mix is no longer `Static(EndpointLoopback)`).
    `track_feed(kind, mic: Option<&DeviceSelection>, supported)` — the mic option carries
    both presence and selection.
  - New **`mix_process_thread`** (track 0): owns the desktop resampler + the Mix AAC encoder
    on its MTA thread. `select!`s over the desktop-endpoint capture packets, the mic's fanned
    resampled chunks, and a one-shot warm-up timer; publishes the Mix VU meter on the **mixed
    output**; sends the Mix ASC **eagerly** before any data (so D4/the save gate stays
    satisfied — unchanged track count). A disconnected input is swapped to `never()` so its
    select arm stops firing.
  - `audio_process_thread` += `chunk_fanout: Option<Sender<ResampledChunk>>` — the Mic track
    forwards each resampled chunk (process + finish flush) to the mixer, then dropping the
    sender at end-of-stream signals the mixer that the mic ended.
  - `spawn_buffer_producers`: a `mic → mixer` channel is created only when both a Mix and a
    Mic track spawn; the Mix track spawns a desktop-loopback capture + the mix thread, the
    Mic track fans out, Game/VoiceChat/other Static tracks are unchanged.

### Decisions taken (CLAUDE.md ambiguity rule: simpler / more-logged / reversible)
- **D3 — fan-out, not double-capture (as the plan recommends).** The desktop loopback is
  captured once (→ mix only); the mic is captured and resampled once (Mic track) and its
  chunks are fanned to the mixer. **Zero double WASAPI clients, one drift domain per
  source.** The Mic → mixer forward is a **non-blocking `try_send`** (`forward_to_mixer`):
  if the mixer falls behind, the chunk is *dropped* rather than blocking the mic's capture
  path, and because the mixer places chunks by absolute frame index a dropped chunk is
  silence at that position with **no cumulative drift** (and the mic's own track still
  encodes every chunk). Dropped-count is logged per track on teardown. This was tightened
  from a blocking send after the B4 review flagged that a slow mixer could transitively
  stall the physical mic-capture callback. Reversible.
- **D6 — kept `AacEncoder::new(kind)`'s cosmetic `kind`** (the mix thread passes
  `AudioTrackKind::Mix`); it only labels logs, no behaviour. Not worth churning the signature.
- **D4 — untouched (already satisfied by B3's eager ASC).** B4 keeps the Mix ASC eager and
  the track count fixed, so the `v.len() == num_audio` save gate needs no change. (The
  handover's speculation that B4 would relax D4 doesn't hold — B3 already made ASC
  source-independent.)
- **Warm-up grace (`MIX_WARMUP_GRACE = 500 ms`).** The mixer blocks emission until both
  expected sources anchor, so the clip's start is correctly mixed even with tens-of-ms
  startup skew. If the mic device **never opens** (no first chunk ever), the thread's 500 ms
  timer calls `release_warmup()` so the desktop plays alone rather than the mix stalling and
  the buffer growing unbounded. A source that shows up after release joins from its own
  anchor (stale pre-emission frames trimmed) — a rare, logged startup edge.
- **Solo desktop mix is 3 dB quieter than the old D2 pass-through.** The −3 dB mix-bus
  headroom is applied whether one or two sources are present (the plan pins "−3 dB then soft
  clip"; the "−3 dB gain exact" test asserts it). Accepted and documented; reversible if a
  later UI pass wants conditional headroom. This is the only behaviour change on the default
  desktop-audio path.

### Scope (flagged per the CLAUDE.md ambiguity rule — reversible, logged)
- **OtherSystem stays deferred; B4 = the mixer only.** OtherSystem's correct source is
  `endpoint-loopback ↔ process-exclude-tree(game)` (D5), which needs exclude-mode process
  loopback bound to the live game PID — HW work, and a half-version would double game audio
  into OtherSystem the moment a game binds. Splitting it out keeps B4 fully correct and
  CI-green. OtherSystem + D5 move to a later task (paired with the exclude-mode process-
  loopback HW cycle). `planned_kinds` still plans it; `track_feed` still returns `None` for
  it; `warn_deferred_tracks` still logs the deferral reason.

### Owed to B7 (Nitro) — no HW on this branch
- Mix plays in a default (Mix+Mic) clip; levels sane; a Discord upload of the 2-track clip
  plays the mix. Long-session crackle/drift watch (the mixer adds a thread + two sums/frame —
  re-baseline CPU ≤ 2 %). Warm-up + late-join behaviour under a real mic that opens slowly.
  The mix's −3 dB solo level is audibly correct.

## 2026-07-08 — Slice B / B5: muxer hybrid-`moov` finalize on save (N-track, amended §4)

Implements **B5** (`SLICE-B-PLAN.md §B5`, amended `§4` finalize): every saved/recorded clip
is finalized as a **progressive** MP4 with a real `moov` (full per-track sample tables +
durations) so non-fragment-aware readers — Explorer duration/thumbnail, some editors, old
WMP seeking — read it cleanly, **without giving up** the `§4.6` crash-safety of the fragment
stream. Entirely inside `src/mux/fmp4.rs`; the ring/save/engine N-track paths were already
generic (B1), so **no engine-logic change** beyond boxing one enum variant. `CLAUDE.md`/
devpack normative. Local-green: **296 tests** (+10), `just release` **8.97 MB** vs the 10 MB
budget. **No new dependency** (hand-rolled boxes). **Validated end-to-end against ffprobe/
libavformat on this box** (see below) — this is a container-correctness smoke check, NOT the
formal B7 HW gate.

### The mechanism — OBS-Hybrid "soft remux"
During recording the file is the frozen-`§4` fragmented layout with **one 16-byte addition**:
a `free` placeholder box (64-bit-largesize form) written between `ftyp` and the fragmented
`moov`. On a clean `finish()`:
1. A **finalized (progressive) `moov`** is appended at EOF: per-track `stts` (one constant-
   delta run), `stsz` (per-sample sizes), `stsc` (run-length samples-per-chunk — one chunk
   per fragment), `co64` (64-bit absolute chunk offsets), `stss` (video IDR positions; omitted
   for all-sync audio), real `mvhd`/`tkhd`/`mdhd` durations, and **no `mvex`**.
2. The head `free` placeholder is overwritten **in place** with an `mdat` header whose 64-bit
   `largesize` spans everything up to that trailing `moov` — swallowing the fragmented `moov`
   + every `moof`/`mdat` into one opaque Media Data box. The placeholder is 16 bytes before
   and after, so **no sample byte moves**; the `co64` offsets point at the untouched bytes.
The finalized file reads as plain progressive `ftyp` · giant `mdat` · `moov`. Two small
writes, no media copy (`§4.7` `.part`→fsync→rename unchanged). A crash **before** finalize
leaves a valid fragmented MP4 — the `free` box is simply skipped — so `§4.6` intent holds.

### Decisions (flagged per the CLAUDE.md ambiguity rule — reversible, logged)
- **D-B5 — a zero-AU audio track is DROPPED from the finalized `moov`,** not emitted as a
  zero-sample track. This is the **B3 gap** the plan called out: with `separate_tracks = true`
  and no VC/game app ever running, that per-app track's ASC is present (D4 eager) but it holds
  zero AUs. Simpler + more compatible than whole-clip silence-fill (a finalized clip carries
  only tracks with content; an editor sees no dead track). Track IDs may then be non-contiguous
  — valid. Not on the default (Mix+Mic) path; exercised by a unit test. Reversible if a later
  need wants the silence-fill variant instead.
- **elst for the audio head offset.** The fragmented file expresses the ≤ 1-AAC-frame head
  slack (`§4.4`) via each audio track's first-fragment `baseMediaDecodeTime`. A progressive
  `moov` has no per-sample base time, so an **empty edit** (`edts`/`elst`: `media_time = -1`,
  duration = the offset in movie ts, then a media edit from 0) re-inserts it — making the
  finalized timeline's A/V alignment **byte-for-byte the fragmented file's**. Emitted only when
  the offset > 0 (video never needs one; it sits at time 0 = the `§4.3` origin). Confirmed:
  `just verify` reports the same "audio head ≤ 21.33 ms, video@0" it does for the fragment path.
- **`co64` (64-bit chunk offsets) + 64-bit `mdat` largesize unconditionally,** so long/high-
  bitrate record-mode clips past 4 GiB stay valid without a size-dependent code path.
- **Known bound (documented, not a bug):** `mvhd`/`tkhd`/`mdhd` keep 32-bit (v0) durations, so
  a single file over ~19.8 h (video timescale `fps·1000`) would overflow the media-duration
  field. Record mode is "next N minutes"; far out of range. Revisit with v1 headers only if
  multi-hour single-file recording is ever in scope.

### Change surface
- `Fmp4Writer` gains `file_pos` (running write cursor = chunk-offset source of truth),
  `placeholder_offset`, a `TrackIndex` per track (`sizes`/`chunks`/`sync`), and stored
  `avcc`/`width`/`height`/`video_timescale` (needed again at finalize). `create()` split into a
  COM read + a COM-free `create_from_parts()` so the whole finalize path is unit-testable
  without an `IMFMediaType`. `flush_*_fragment` record each fragment's chunk; `write_fragment`
  advances `file_pos`. New pure builders: `build_placeholder_box`/`giant_mdat_header`,
  `build_stts`/`stsz`/`stsc`/`co64`/`stss`/`stbl_full`, `build_edts`, `to_movie_ts`,
  `build_final_moov`/`build_final_video_trak`/`build_final_audio_trak`; `mvhd`/`mdhd`/`tkhd`
  gained a `duration` param (fragmented callers pass 0).
- **`engine.rs`: `Rec::Active` boxes the writer** (`Box<Fmp4Writer>`) — the finalize sample
  indexes make `Fmp4Writer` far larger than the other `Rec` variants (clippy
  `large_enum_variant`). Only construction changed (`Box::new(writer)`); all method calls
  auto-deref. No behavioural change.

### Tests (pure box math + COM-free integration)
- `stts`/`stsz`/`stsc`/`co64`/`stss`/`edts`/`to_movie_ts`/placeholder+giant-mdat header byte
  layouts; run-length `stsc` collapse; `stss` omitted when all-sync.
- **`finalize_produces_progressive_moov_over_giant_mdat`**: drives `create_from_parts` →
  packets → `finish`, parses the real output — asserts top-level `ftyp`/giant-`mdat`/`moov`,
  the `mdat` spans to the `moov`, two traks (video + 1 audio, the **empty track dropped**), no
  `mvex`, `co64`/`stss`/`elst` present, and `co64[0]` points at the real first video sample
  bytes inside the `mdat`.
- **`finalize_without_video_stays_fragmented`**: a no-video clip keeps the `free` placeholder
  and a single (fragmented) `moov` — the degenerate branch stays a valid fragmented file.

### ffprobe/libavformat validation on this box (container smoke check, not the B7 gate)
A real `record --seconds 6` on the Nitro (which happens to have `separate_tracks = true` +
Discord + a game bound → a **5-stream** clip: video + Mix/Game/VoiceChat/Mic) produced a file
that ffprobe reads as **`ftyp` · `mdat` · `moov`** with a **real 5.842 s duration** (the exact
fragmented-file-shows-0 quirk B5 fixes), all 5 streams with proper `stbl`. `just verify` →
**PASS on every §4/§5 assertion**: stream shape, monotonic PTS (all tracks), video CFR, track-
end alignment ≤ 1 AAC frame, rebase origin (video@0, audio head ≤ 21.33 ms), full-decode
validity (`§4.6`). This confirms the byte-level container against a true demuxer; the formal
AV-1..AV-5 rig + CapCut/Discord/Explorer/WMP compat checks remain **owed to B7**.

### Owed to B7 (Nitro) — no formal HW on this branch
- A 5-track clip → **CapCut** import reads all enabled tracks + plays the mix; **Explorer**
  shows the correct duration; **WMP** seeks; a **Discord** upload plays (flattens to track 1).
- **Crash-safety**: kill mid-record, confirm the `.part` is a valid fragmented MP4 (the `free`
  box skipped, fragments play to the last complete one) — the unfinalized side of the hybrid.
- The **empty-per-app-track drop** on a real clip: `separate_tracks = true` with **no** VC/game
  app running → the finalized `moov` omits that track (unit-tested here; confirm on HW).

## 2026-07-08 — Slice B / OtherSystem + D5 (the last deferred track) + B6 (LIMITATIONS)

Implements the OtherSystem track (`SLICE-B-PLAN §0` track 4, decision **D5**) — the last
planned track still deferred after B1–B5 — and **B6** (the multi-track honesty docs). With
this the 5-track topology is feature-complete: under `separate_tracks = true` above the
Win10-2004 floor the runtime spawns **Mix · Game · VoiceChat · OtherSystem · Mic**. Pure-
logic + narrow wiring on the proven B3 binding machinery; **no HW on this branch** (the
exclude-mode process-loopback path folds into the B7 Nitro gate). `CLAUDE.md`/devpack
normative. Local-green: **297 tests** (+1), `just check` clean, `just release` within the
10 MB budget. **No new dependency.**

### The mechanism — OtherSystem reuses the game binding, as an *exclude*
"Other system" = all system audio **except the game**. It must exclude the *same* game the
Game track includes, so it consumes the binding watcher's game publication rather than
detecting its own PID (which could race to a different game). New `TrackFeed::OtherSystem`
(not a `BoundRole` — endpoint-or-exclude, not include-tree). Its capture loop
`run_other_system_capture` reads a live game `Binding` from a new `BindingState.other_system`
slot and picks its source with the pure `other_system_source`:
- **no game bound** → `AudioSource::EndpointLoopback` (the full default-endpoint loopback);
- **a game bound** → `AudioSource::ProcessLoopback { pid, include_tree: false }` (exclude the
  game's tree → everything else).

### Decisions (flagged per the CLAUDE.md ambiguity rule — reversible, logged)
- **D5 — the endpoint↔exclude switch is a within-epoch source swap, not an epoch.** When a
  game binds/unbinds, `run_other_system_capture` ends the current `run_capture` and starts a
  fresh one on the new source. Both derive PTS from the QPC master domain (`§2.2`), so PTS
  stays absolute/monotonic across the swap; the gap between the two runs is silence-filled by
  the `§2.3` synthesizer downstream — exactly the device-rebuild path. The ring/encoder/video
  are untouched (confirmed: no epoch bump, no `size_rx` re-init). This mirrors B3's game A→B
  retarget, which already crosses two `run_process_capture` calls on one downstream resampler.
- **A dedicated `other_system` slot mirrors the game binding, rather than sharing
  `state.game`.** A `RoleSlot` holds exactly one in-flight run's stop flag; if OtherSystem and
  the Game track both armed `state.game`, their run-stops would clobber. The watcher publishes
  each game retarget to both `state.game` (include, for the Game track) and `state.other_system`
  (exclude, for OtherSystem) with the same generation — each consumer only compares its own
  slot. So `track_game = off` + `track_other_system = on` still excludes the game from
  OtherSystem (the watcher runs game detection when *either* track needs it).
- **OtherSystem is gated on the Win10-2004 process-loopback floor,** like Game/VoiceChat. Below
  it, exclude-mode process loopback is unavailable, so an OtherSystem track could never exclude
  anything — it would just duplicate the Mix's desktop content. Hiding it there keeps the whole
  per-app track family behind one coherent capability gate (`track_feed` returns `None`;
  `warn_deferred_tracks` logs the floor reason; the three per-app reasons merged into one).
- **Endpoint-mode OtherSystem opens its own loopback client** (a second default-render loopback
  alongside the Mix's desktop capture). WASAPI allows multiple loopback clients on one endpoint;
  a dedicated client is simpler + more reversible than plumbing a conditional fan-out that would
  have to detach/attach on every game bind. Trivial CPU (one extra 48 kHz sum-free capture).

### Teardown safety (mirrors `run_bound_capture`)
`run_other_system_capture` arms its run-stop on `state.other_system` before running, then
re-checks `cap_stop` + the slot generation to close the same teardown/retarget TOCTOU B3 fixed.
The watcher's teardown sweep now also interrupts `state.other_system`, and the watcher is spawned
whenever OtherSystem is present — so watcher-exists ⟺ OtherSystem-spawned, and a run armed after
the sweep still observes `cap_stop` and never starts unkillable (no hung epoch-restart join).

### D4 (ASC-complete save gate) untouched
OtherSystem runs the standard `audio_process_thread`, which emits its ASC eagerly at startup
(source-independent), so the `v.len() == num_audio` save gate is satisfied whether or not a game
is ever bound. No D4 change (as B3 already established). If a save lands while OtherSystem has
produced zero AUs (never happens — endpoint mode always produces), B5's D-B5 drop handles it.

### B6 — LIMITATIONS.md + README
The multi-track honesty list (`SLICE-B-PLAN §B6`): in-game voice is inseparable (renders inside
the game process → the Game track); the **Other-system track double-counts a detected voice app**
(the API can't express system − game − VC, so VC bleeds into Other-system as well as its own
track — editors keeping both play it twice); voice-chat = the whole app (pings/soundboard/Go-Live,
not just speech); detection is by process name, browser VC is out of scope; the game bind is a live
foreground-fullscreen guess and retargeting leaves a silence gap; per-app tracks need Win10 2004+;
uploads/players hear only the Mix (track 1). Added a README audio bullet pointing at the full list.

### Change surface
- `src/engine.rs`: `TrackFeed::OtherSystem`; `track_feed` OtherSystem arm gated on `supported`;
  pure `other_system_source(Option<Binding>) -> AudioSource`; `BindingState.other_system` slot;
  `binding_watcher_thread` gains an `other_system` param (game detection when Game **or**
  OtherSystem needs it; dual publish; teardown sweep); new `run_other_system_capture`; the spawn
  loop's `TrackFeed::OtherSystem` arm + watcher spawn condition (`|| other_system_present`).
- Doc-rot fixes in `src/audio/wasapi_stream.rs` (the `AudioTrackKind` enum comment).
- Tests: new `other_system_source_switches_on_the_game_binding`; updated
  `track_feed_spawnable_set_depends_on_os_support` + `spawnable_is_planned_intersect_feed` (all
  five spawn above the floor with `separate_tracks`).

### Owed to B7 (Nitro) — no HW on this branch
- OtherSystem carries the correct content in both modes: **no game** → full system audio; **game
  bound** → everything *but* the game (play a game + music + Discord, confirm the game is absent
  from Other-system and present on Game). The **endpoint↔exclude swap** on a game launch/exit
  leaves a clean silence gap, no desync, no epoch (video uninterrupted). The **double-counted VC**
  is audible on Other-system+VoiceChat together (documented). CPU ≤ 2 % at the full 5 sources.

## 2026-07-08 — Slice B / B3.5: enumerated mic-device dropdown (Slice-A fast-follow closed)

Replaces A5's free-text pinned-id field with a populated capture-device dropdown
(`SLICE-B-PLAN §B3.5`) — the last owed Slice-A fast-follow. Fixes the A5 HW finding
"a bad pinned id just fails to open" (`HANDOVER §5` A5 finding #2): the normal path now
offers only real, enumerated devices. Pure-logic + narrow UI wiring; **no config schema
change** (a device is still stored as its endpoint id, so `config.rs`, the shipped
template, and the drift test are untouched). Local-green: **299 tests** (+2 pure-mapping
tests), `just check` clean, `just release` within the 10 MB budget. **No new dependency,
no new `unsafe`, no new `windows` feature gate.** No HW *validation* on this branch (the
formal pick/restart/unplug cycle folds into the B7 Nitro gate); the `list-audio-devices`
subcommand was smoke-run on the Nitro and returned the real capture endpoints (FIFINE +
others) with friendly names and the exact `{0.0.1…}` ids capture pins.

### Decisions (flagged per the CLAUDE.md ambiguity rule — reversible, logged)
- **Built on the whitelisted `wasapi` crate's `EnumAudioEndpoints` wrapper, NOT hand-rolled
  COM** — a deliberate deviation from the `SLICE-B-PLAN §B3.5` sketch ("confined unsafe
  COM"). `wasapi::DeviceEnumerator::get_device_collection(&Direction::Capture)` already wraps
  `EnumAudioEndpoints(eCapture, DEVICE_STATE_ACTIVE)`, and `Device::get_id()` /
  `get_friendlyname()` give the id + name — the same crate + types the capture path already
  uses to open a pinned mic (`wasapi_stream::open_endpoint`), so the enumerated id is exactly
  what `get_device(id)` re-opens. Result: zero new `unsafe`, zero new `windows` feature gates,
  zero new deps (CLAUDE.md YAGNI + "use existing libraries where you can"). The plan's sketch
  predated noticing the crate already exposed it.
- **The COM read runs on a short-lived MTA thread** (`enumerate_capture_devices` spawns a
  thread holding a `ComMta` guard, enumerates, joins). Apartment-independent of the caller, so
  the settings-window / tray threads need not be (and are not) forced into a particular COM
  apartment. Any failure (COM unavailable, an endpoint with no id) yields an **empty list** —
  the picker degrades to Default/Off and still preserves a hand-set pin; it never blocks
  (beyond the fast join) or panics. Called only on window open/re-show (user-initiated,
  infrequent), so the synchronous join is a non-issue.
- **A pinned id that is not currently enumerable is PRESERVED, not dropped or substituted.**
  If `[audio].mic` holds an id that is unplugged now or was hand-set in TOML, the dropdown
  shows a trailing `Unavailable: <id>` entry (selected) and the label reads the same — so
  merely opening Settings never silently changes a saved pin, and the user sees their device
  is missing (`02-AV-SYNC-SPEC §7`: never pretend a gone device is fine). Picking a live
  device or a policy is the only way the pin changes.
- **Re-enumerate on each open/re-show.** The window persists hidden across opens (A2), so the
  device list is filled in `Editor::load` and re-filled on re-show via a new
  `Shared.rescan_devices` flag the tray sets and the app consumes in `logic()` — the same
  swap-to-consume pattern as A7's `rescan_recent`. A mic hot-plugged while the window was
  hidden appears on the next open. (No in-window "Refresh" button — reopen is the refresh, and
  it matches the B7 HW checklist wording.)
- **`list-audio-devices` subcommand** added as the B3.5 HW instrument (the exact
  `enumerate_capture_devices` code path, no drift) — prints `id <TAB> name`; its doc comment
  carries the B7 checklist. Also genuinely useful for a user hand-pinning a device in TOML.

### Owed HW (folds into the B7 Nitro gate)
- The Settings dropdown lists the real capture devices with friendly names; picking one +
  Save + restart makes the mic track open that endpoint (log / VU meter confirm).
- Unplug the pinned device → reopen Settings → it shows `Unavailable: <id>` and is NOT
  replaced by another device; the list otherwise updates (device dropped). Replug → it returns.
- The `list-audio-devices` id matches what `[audio].mic` accepts (round-trip a pin).

## 2026-07-08 — Slice B / B7: HW gate in progress — two code fixes surfaced by the pass

The batched Nitro HW gate is underway (`B7-CHECKLIST.md` tracks per-phase results). Phases 1–4
are substantially green (audio-COM instruments, B3.5 mic dropdown incl. unplug/replug, the
5-track container via `just verify` + ffprobe 5 streams + VLC/Explorer/WMP + crash-safe `.part`,
OtherSystem content routing). Two issues found during the pass were fixed **directly** (both
narrow, both re-verified local-green: `just check` + 299 tests + probe crate build + release
budget); everything else remains owed to the batched HW re-check.

### Fix 1 — audio-probe PID-liveness watchdog mirrored from the core (doc-drift closed)
- **Finding:** killing the target of `just probe --pid <PID>` mid-run did NOT end the capture or
  log "target process exited" — it silence-filled to the full duration (no crash, valid WAV).
- **Cause:** the probe (`tools/audio-probe`) never implemented a liveness watchdog; its header
  item 3 *claimed* it did. The CORE `src/audio/process_loopback.rs` DOES have it (unit-tested),
  and the probe is a standalone instrument kept "in lock-step by comment" — the watchdog was the
  one piece never mirrored.
- **Decision:** mirror the core's watchdog into the probe (the SAME `OpenProcess(PROCESS_SYNCHRONIZE)`
  + zero-timeout `WaitForSingleObject` + exit-latch `is_dead`), polled at the top of the capture
  loop; on exit it logs "target process exited — ending process-loopback capture" and stops. Added
  `windows = "0.62.2"` (features `Win32_Foundation`, `Win32_System_Threading`) to the probe's OWN
  Cargo.toml — the crate is standalone (own `[workspace]`, never linked into `clipd`), so the core
  dependency whitelist does not apply (CLAUDE.md rule 2, "Dev-deps are free" for tools). Self-capture
  (own PID) never signals, so the happy path is unchanged (smoke-verified: full 3 s, no early exit).
  Also corrected the header usage examples (`just probe <ARGS>`, NOT `just probe -- <ARGS>` — the
  recipe already injects `--`, so a leading `--` reaches the tool as a bad arg).
- **Core watchdog still owed on HW:** the probe fix validates the probe; the *shipping* watchdog is
  exercised by closing a **clean-exit** bound game during a live 5-track run and grepping the log for
  `target process exited`. The 2026-07-08 attempt used Roblox, which keeps helper processes alive, so
  the bound PID may not have died — inconclusive, retry with a game that exits its PID cleanly.

### Fix 2 — per-track names in the finalized MP4 (flagged per the CLAUDE.md ambiguity rule — reversible)
- **Motivation (orchestrator-directed):** a 5-track clip previously wrote the same `hdlr` name
  (`PRODUCT_NAME` = "clipd") for every audio track, so editors/ffprobe showed five indistinguishable
  "Audio" streams. For a clipper whose value is separable tracks, that is a usability gap, not a new
  feature — this labels an existing deliverable rather than adding scope. Done on explicit instruction.
- **Decision:** `build_hdlr` now takes the track name; each audio track's `soun` `hdlr` name is its
  `AudioTrackKind::title()` ("Mix" / "Game" / "Voice chat" / "Other system" / "Microphone"), threaded
  via a new `AudioTrackConfig.name` field set at the two engine construction sites. The video track
  keeps `PRODUCT_NAME` (it is unambiguous as the sole video stream, and this keeps the branding + the
  import). ffprobe surfaces the names as `handler_name`; most NLEs/VLC show them as the track label.
  Config encoding, the container box layout, `co64`/`mdat` math, and the hybrid-`moov` finalize are all
  untouched — this is a presentation-only string change. Unit-tested (the `soun`-handler test now also
  asserts the name round-trips into the hdlr box). Fully reversible (revert the string source).
- **Owed HW (folds into B7):** `ffprobe -show_entries stream_tags=handler_name` on a real 5-track clip
  shows the five names; spot-check that a target editor (CapCut) displays them.

### B7 progress + scope decisions (2026-07-08)
The HW pass validated Phases 1–4 (audio-COM instruments; B3.5 mic dropdown incl. unplug/replug;
5-track container via `just verify` + ffprobe 5 streams + VLC/Explorer/WMP + crash-safe `.part`;
OtherSystem content routing). Two fixes above were HW-confirmed: **track names** show in ffprobe
`handler_name` as `clipd` (video) + Mix/Game/Voice chat/Other system/Microphone; the **core
PID-liveness watchdog** logged `target process exited` for BOTH `game` and `other-system` tracks
when a clean-exit game (Incredibox) closed — which also evidences the **D5 endpoint↔exclude swap
on game-exit** (Roblox had been inconclusive because it keeps helper processes alive).

Orchestrator scoping decisions (what remains before the UI rework + friend distribution):
- **Phase 5 (AV-1..AV-5) is the ONLY remaining gate.** Everything else is cleared, accepted, or moved.
- **Phase 6 (endurance/perf: ≥1 h crackle, CPU ≤2 % at 5 sources, 2 h UI soak) → folded into the
  friends-beta multi-device test** — several testers on iGPU / AMD / Win10 AMD+Nvidia clipping
  full-time for days is a stronger endurance + cross-hardware signal than one Nitro session.
- **Phase 7 (A6 hotkeys) → cleared** (cross-row conflict was HW-validated; the rest is revisited in
  the UI rework, where live hotkey re-registration is decided).
- **P4 items** (`game=false + other_system=true` still excludes the game; double-counted VC audible;
  D5 swap on game-LAUNCH via `just verify`) **→ deferred to after the UI pass** — the config UI to
  toggle these tracks does not exist yet; covered by unit tests + the QPC master domain meanwhile.
- **P1/P3 leftovers** (probe `--exclude` single-PID; CapCut/Discord specifics; empty-per-app-track
  drop on HW) **→ accepted** — covered by two-probe output, VLC/VS-Code substitutes, and unit tests.

### AV-sync gate (Phase 5) — scoped/skipped this session (orchestrator 2026-07-08)
Grounded in the M2 record above (2026-07-04): **AV-2 (drift, the only trustworthy gate) PASSED at
−1.92 ms/10 min**; AV-3/AV-4 PASSED; **AV-1/AV-5 were never clean gates** (uncalibrated rig adds a
run-to-run WASAPI-render-latency constant, +47 vs +60 ms — the §5 AV-1 constant cancels in AV-2's
drift fit anyway). That pass was on the M2/M3 2-track pipeline.

Decision: **no AV re-run this session.** AV-3/AV-4's "audio within 1 AAC frame" bound is re-confirmed
green on real 5-track clips by `just verify` (B5), and they exercise the §2.3 gap-synthesis / §7
device-loss paths Slice B did not change; AV-1/AV-5 remain rig-limited (not worth a number). The one
piece NOT re-confirmed on the Slice-B pipeline is **AV-2 drift now running through B4's software mixer**
(the Mix track feeds the AAC sample-clock; B4's review caught + fixed a HIGH anchor av-sync bug, so it
is a real regression surface). `just verify` (green on 5-track) checks static timeline consistency
(CFR, end-alignment ≤ 1 AAC frame) but is looser than AV-2's 5 ms minute-1/10 metric.

**Owed before final Slice-B sign-off (not blocking UI rework / friend distribution):** one AV-2 drift
re-confirm on the mixer-in-path build — either a one-off 10-min rig run or, preferably, folded into the
friends-beta (days of multi-device clipping is a stronger drift signal than one rig run). Recorded so
the reasoning is on the record and the gap is not silently dropped.

### B7 net state at session end (2026-07-08)
Phases 1–4 GREEN; Phase 7 cleared; the two fast-follows (named MP4 tracks, audio-probe watchdog)
merged + HW-confirmed. Phase 6 (endurance) → friends-beta; P4 → post-UI; Phase 5 AV gate → the single
deferred AV-2 re-confirm above. **B7 is effectively closed pending that AV-2 re-confirm.** `main` is
3 commits ahead of `origin/main` (2 code + 1 docs, this session's doc commit makes it 4) — local-only,
push when ready. NEXT = UI rework + friend distribution.

---

## 2026-07-08 — UI/brand cleanup pass (pre-friends-beta)

The final UI cleanup pass before the friends build: a calculated accent colour, a real
tray glyph, the CI license gate, and a written UI plan. No engine code changed. Full plan:
`UI-PASS-PLAN.md`; rendered brand reference is the session artifact.

### License gate fixed (committed)
`cargo deny check` (CI's licenses+advisories step) was **red on every push** — three
*permissive* licenses pulled in transitively by eframe/egui 0.35 were not on the
`deny.toml` allow-list:
- **`BSL-1.0`** (Boost) — the arboard clipboard stack (`clipboard-win`, `error-code`),
  used by `egui-winit` for the settings-window text fields.
- **`OFL-1.1`** (SIL Open Font License) and **`Ubuntu-font-1.0`** — `epaint_default_fonts`
  (egui's bundled UI + emoji fonts).

All three are OSI-approved / FSF Free-Libre and impose no copyleft on our binary, so they
are GPL-3.0-compatible and safe to redistribute. **Decision:** added them to the `allow`
list with provenance comments (not dropping clipboard/fonts — both are wanted). Verified
locally: `cargo deny check licenses` → `ok`. Reversible (remove the three lines). The
pre-existing `windows-sys` triple-version *warning* (via glutin/winit) is left as `warn`
— cosmetic, and the binary is still 9.0 MB < 10 MB.

### Lavender accent — calculated, not picked (D-U1, D-U2)
The M7 language is "egui default dark + **one** accent"; the window ships egui default dark
and never sets an accent, so a status-green did double duty. **Decision:** one lavender
accent, chosen by computing WCAG 2.1 contrast against egui 0.35's real dark surfaces
(`panel_fill #1B1B1B`, `extreme_bg_color #0A0A0A`):
- `ACCENT #A78BFA` (6.3:1 / 7.3:1 — AA text + graphical), `ACCENT_HOVER #C4B5FD` (9.3:1),
  `ACCENT_FILL #5B4B9E` (selection background, text-on 4.8:1 AA — **fill only**).
- Classic lavender `#E6E6FA` (14:1) reads as off-white — rejected (no brand). Deep violet
  fails as foreground text — fill only.
- **Semantic traffic-lights are unchanged** (VU meter green/amber/red, save OK green /
  fail red, warning orange, error red) — they encode state, not brand.
- **D-U2:** these UI colours live in a new `src/ui/theme.rs`, **not** `spec_constants.rs`
  (that file is `02-AV-SYNC-SPEC.md`-only, per its doc mandate). Applied via
  `set_visuals(Visuals::dark() + accent overrides)` — **D-U1** also forces dark theme
  (M7 "dark, dense, quiet"). Both reversible.

### Tray glyph — brand-forward, procedural (D-U3, D-U4)
The solid-colour square is replaced by a hand-rasterized "last-slice" glyph (rolling
timeline, kept tail lit, live-edge playhead), keeping `tray.rs`'s zero-dependency
constraint (no image decoder → binary budget safe) and its one-function `icon_for` seam.
- **D-U4 brand-forward:** healthy/buffering = `ACCENT` lavender; amber/orange/red reserved
  for attention states (fits the trust model — calm normally, colour shift *means* "look").
- **D-U3:** procedural glyph is **placeholder art for the friends beta** — at 16 px the
  playhead knob is barely legible, accepted for beta. The official **SVG logo + embedded
  `.exe` `.ico`** (needs a build-dependency) is **M10 / official release**, not now.

### UX findings → `UI-PASS-PLAN.md`
Audit of the settings window produced 2 P1 fixes (VU meters lifted above the fold; "needs
restart" shown inline as a field changes, not only after Save) + P2 polish (section cards,
a primary lavender Save button, a first-run orientation line). These are *planned*, not yet
implemented — the plan is the deliverable of this pass; implementation is the next task.

### Distribution / M10 pre-decisions (decide now, build at M10)
Not implemented this pass (orchestrator scoped M10 signing/distribution out), but positioned:
- **License stays GPL-3.0-only**; add a `TRADEMARK.md` (code free, name/logo identify
  official builds — the Firefox/Rust model; `09-NAME-AND-GTM.md` §1.3). Use ™ from day one.
- **Code signing:** unsigned for friends-beta (document the SmartScreen "More info → Run
  anyway" line in the quick-start); budget a cert (Azure Trusted Signing / EV) before public
  launch — the deferrable M10 item.
- **Distribution shape:** `just dist` portable zip is the friends channel now; winget +
  optional installer + Steam convenience build are M10 / monetization phase.

## 2026-07-08 — Name deferred to M10 (research recorded)

**Decision:** the product name stays the working `clipd` through the friends beta; the final
naming decision is **deferred to M10**. Rationale + findings, recorded for the future pass:

- **`clipd` is retained as the beta name** — orchestrator reads it as leaning "get *clip'd*"
  (gamer slang, cf. "get rekt"), which is on-tone for the audience; it is zero-collision and
  every namespace we need is free. `PRODUCT_NAME`/`BINARY_NAME` (`spec_constants.rs`) stay
  `clipd`; a rename remains a one-constant + Cargo-package change.
- **Naming criterion for the M10 pass:** the name must **read as gaming software** to a gamer.
  The candidates researched this session were rejected on that axis (below), not on availability.
- **`Kiroku` (the devpack's `09-NAME-AND-GTM.md §1.1` "cleanest" pick) is now TAKEN** on both
  crates.io and the GitHub namespace — scratched.
- **Six names were live-verified free across crates.io + GitHub namespace + a strong domain**
  (RDAP-checked): `lastclip` (.dev+.app), `gotclip` (.dev+.gg), `clipkeep` (.dev), `clipjet`
  (.dev), `nabclip` (.dev), `clipwell` (.io). **All rejected by the orchestrator as not reading
  as gamer software** — `lastclip`/`gotclip` felt un-gamer-y; the rest likewise.
- **Availability-check method (reuse at M10):** crates.io `GET /api/v1/crates/<name>` (404 =
  free) with a UA header; `github.com/<name>` (404 = free namespace); `rdap.org/domain/<d>` with
  redirect-follow (404 = available). Also do the `09-NAME-AND-GTM.md §1.2` personal checks
  (USPTO/IP-India TM search, Steam/SteamDB, winget id, say-it-aloud) before claiming.
- **Next-research direction (M10):** explicitly gamer-toned coinages (clutch/ace/frag/clip
  culture, short punchy invented words), verified free in all namespaces AND checked on
  Steam/SteamDB (a clip tool may want a Steam presence), then run the `09` claim checklist
  within a day of the pick (GitHub org, crates.io stub, handles).
- **Domains are not being pursued now** (orchestrator) — the availability data above is kept
  only as reference for whenever a domain is wanted.

## 2026-07-08 — UI pass Branch 1 (U1–U4 implemented): theme, palette, glyph, layout

Implements `UI-PASS-PLAN.md` U1–U4 (the low-risk visual bundle) on `ui-u1-u4-theme-glyph`.
No engine code touched; all changes confined to `src/ui/*`. Local-green: `just check` clean,
**306 tests** (+7 over 299), `just release` **9.03 MB** (< 10 MB). No new dep.

- **New `src/ui/theme.rs`** (D-U2) — the single home for UI colours + the procedural glyph.
  Exports `ACCENT #A78BFA` / `ACCENT_HOVER #C4B5FD` / `ACCENT_FILL #5B4B9E`, the
  value-harmonised semantic palette, `ON_FILL`, `configure_visuals()`, `glyph_rgba()`,
  `window_icon()`. `settings.rs` + `tray.rs` reference it; the inline `0x3f_b9_50`-style
  literals are retired.
- **Value-harmonised palette locked (D-U11):** `GOOD #7DFA8F`, `AMBER #FAD67D`, `WARN #FAC87D`,
  `BAD #FA6D5F`. Chosen from the plan's §1.1 candidates and **contrast-validated in a
  `theme.rs` unit test** (not eyeballed): each ≥ 3:1 graphical on `#0A0A0A` (all ≥ 7:1); GOOD
  & BAD ≥ 4.5:1 text on `#1B1B1B` (13.1 / 6.1); all share HSV value ~0.98 (max channel ≥ 0xF0);
  BAD keeps the highest saturation (S 0.62) so it still reads as danger; AMBER (H 43°) vs
  WARN (H 36°) stay 7° apart. The WCAG assertions live in the test suite so a future retune
  can't silently regress AA.
- **D-U1 applied:** `cc.egui_ctx.set_visuals(theme::configure_visuals())` at window creation
  (dark + accent: hyperlink, selection bg/stroke, hovered/active bg_stroke). The buffer-fill
  bar is the one hand-painted accent (was green); the state dot / VU bands / save line stay
  **semantic** (green/amber/red), only recoloured to the harmonised set. Peak tick → `ACCENT_HOVER`.
- **D-U3/D-U4 tray glyph:** `tray.rs::icon_rgba` now calls `theme::glyph_rgba(state_color, 32)`
  — a rounded chip with a horizontal track carved out, the kept tail (right ~40%) painted back,
  and a bright playhead; supersampled 4× + alpha-weighted box-downsampled (no dark halo).
  Buffering = lavender (brand-forward); paused/warning/error = amber/orange/red. The same
  rasteriser feeds the window icon (`ViewportBuilder::with_icon`, U1). The old "solid fill" tray
  test is replaced by a "glyph, not a solid fill" test. SVG + embedded `.ico` remains **M10**.
- **U4 layout (pure presentation):** VU meters moved above Status (highest-value answer first);
  each section wrapped in a quiet full-width `Frame::group` card; Save promoted to a filled
  `ACCENT_FILL` button with `ON_FILL` text; a first-run orientation line (`{name} is buffering.
  Press <hotkey> to save the last <len>.`); `on_hover_text` tooltips on every editor row + Rebind;
  friendly relative-time recent-clip labels (reusing `status::format_elapsed`) with the raw
  epoch-ms name kept as weak secondary text + a hover tooltip; the frames counters de-emphasised
  (`weak()`). New pure helpers `format_buffer_len`/`first_run_line`/`relative_time` are unit-tested.
- **Deferred to Branch 2/3:** U5 (inline restart chips), U6 (window min-size + responsive
  widths), U7 (auto-restart banner + relaunch); U8–U10 (recording feedback, save tray balloon,
  folder picker). Those cross into `engine.rs`/`main.rs`/new `unsafe` and are rust-reviewer'd.

## 2026-07-08 — UI pass Branch 2 (U5–U7 implemented): restart chips, responsive, auto-restart

Implements `UI-PASS-PLAN.md` U5–U7 on `ui-u5-u7-restart-responsive`. **rust-reviewer'd —
Approve, no CRITICAL/HIGH**; the one LOW (hotkey rows lacked the inline restart chip) was
fixed. Local-green: `just check` clean, **307 tests** (+1), `just release` **9.05 MB**.
No new dep, **no new `unsafe`** (this branch is pure-Rust wiring — the `unsafe` Win32 lands
in Branch 3). `ui` still depends on engine types only; the engine is untouched.

- **D-U5 — `applied` snapshot + inline restart chips.** `Editor` gains `applied: Config`,
  seeded at window load and never mutated during a session (the config the running engine
  started from). `restart_required_fields` is refactored into a pure `restart_fields(a, b)`;
  every restart-bearing editor row (quality/resolution/fps/buffer/output/desktop/mic **and**
  the two hotkey rows) shows a small lavender "⟳ restart" chip when its draft value differs
  from `applied` — the mic row compares `self.mic.to_cfg()`, the hotkey rows compare the raw
  combo strings (so chip ⇄ banner ⇄ save-note all funnel through `restart_fields` and can't
  drift). `clear_after_save` hot-applies → never chipped.
- **D-U6 — window min-size + responsive content.** `with_min_inner_size([440, 340])`
  (`MIN_WINDOW_SIZE`, floor set by the widest fixed row — the hotkey row). `draw_status_bar`
  flexes `clamp(80, 640)` instead of the old `clamp(80, 320)`; `hotkey_row` uses
  `horizontal_wrapped` so the availability note drops below the field on a narrow window;
  the section cards are full-width. Reversible (drop the min-size call).
- **D-U7 — auto-restart via signal → teardown → relaunch.** The banner's **Restart now**
  sets a new `Shared.restart: AtomicBool`; the tray polls `SettingsHandle::restart_requested()`
  each loop and, when set, tears down as for Quit but returns a new `ui::ShellOutcome::Restart`
  (defined in `ui`, so **no engine→ui dependency**). `main.rs::run_buffer` captures the outcome
  and calls `relaunch_self()` **only after** `engine.stop_and_join()` (devices released) +
  `pump.join()` (hotkeys released), spawning `current_exe` with the same argv via
  `creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)`. The reviewer confirmed the
  release-before-spawn ordering (traced `hotkey.rs`'s synchronous manager drop in `pump.join`),
  that no automatic path can set `restart` (no runaway loop — it's a human click only), and
  that the headless-only hooks (`--autosave`/`--record-secs`/`--simulate-device-loss`) can never
  reach a restart, so the replayed argv always comes back up in the same tray mode. The process
  spawn lives in `main.rs`, not `ui` (satellite law). **HW-owed → the §10 U7 manual pass** (the
  reviewer flagged the `DETACHED_PROCESS` child-stdio edge as worth one machine-side confirm;
  it passed a standalone 3× repro on this box).
- **D-U7 banner mechanics.** The banner is an `egui::Panel::bottom(...).show_collapsible(...)`
  pinned **outside** a `CentralPanel::default().show(...)` scroll (egui 0.35 unified the panel
  types into `egui::Panel` and renamed `show_inside`→`show`, `show_animated_inside`→
  `show_collapsible`). It appears when `pending_restart_fields()` (`applied` vs committed `base`)
  is non-empty and names the accumulated set; **Later** dismisses until the set changes
  (`restart_banner_dismissed`), **Restart now** signals the relaunch.
- **D-U7 limitation (accepted, per plan §7.4):** `applied` is seeded at window creation, so a
  save made in a *prior* window session without a restart under-reports the pending set. Accepted
  for the beta; a fully-correct `applied` would need the engine to publish its started-from
  config — not worth the coupling now.

## 2026-07-08 — UI pass Branch 3 (U8–U10 implemented): recording feedback, save balloon, folder picker

Implements `UI-PASS-PLAN.md` U8–U10 on `ui-u8-u10-trust-feedback` — the trust-feedback trio.
**rust-reviewer'd — Approve, no CRITICAL/HIGH** (both new `unsafe` surfaces verified: correct
zeroing/`cbSize`, buffer bounds, HWND conversion, COM free-once, graceful degradation, no
panics); the two MEDIUM + one LOW findings were fixed (below). Local-green: `just check` clean,
**309 tests** (+6), `just release` **9.06 MB** (< 10 MB). **No new crate** — two `windows`
feature gates only (`Win32_UI_Shell`, `Win32_System_Com`), added in this commit; `Cargo.lock`
unchanged. Two confined-`unsafe` Win32 surfaces, each with a `// SAFETY:` note.

- **D-U8 — recording feedback.** `EngineStatus` gains `recording: AtomicBool` +
  `record_started_unix_ms: AtomicU64` (`set_recording`, same lock-free engine→UI seam as A3/A4).
  The ring thread publishes at **one** point after its `select!` (diffing `matches!(rec, RingRec::On)`
  against a `recording_published` bool) — a single site that catches every `RingRec` transition
  (start / toggle-stop / pause-drain / `--record-secs`-stop / keep-up-failure / drain-complete),
  reviewer-traced to miss none. `Draining` reads as *not recording*. The tray flips the menu label
  ("Start recording" ⇄ "Stop recording") + appends a "· recording" **tooltip suffix** (the plan's
  sanctioned alternative to a glyph mark — recording is orthogonal to the four state colours, so
  it is NOT a fifth state); the status strip shows a red "● Recording — MM:SS". Our analogue of
  ShadowPlay's persistent Instant-Replay icon.
- **D-U9 — save-complete/-failed tray balloon.** **Deviation from plan §8.2 (logged):** instead
  of a new `ShellSignal::Saved { ok, seconds }` (which would thread `signal_tx` through the mux
  worker), the tray **polls the existing `EngineStatus`** each loop (it already reads it for U8)
  and toasts once per changed `last_save_unix_ms` — simpler, touches **zero** engine save-path
  code, same satellite-safe engine→UI direction. Chosen per CLAUDE.md ambiguity rule (simpler +
  reversible). The balloon text drops the clip-length "— N s" (not in `EngineStatus`) to avoid
  showing a wrong number: "Clip saved" / "Clip didn't save — check the log" (the failure toast is
  the priority — the whole "why didn't my clip save" trust model made visible). **Mechanism:** a
  new `Notifier` owns its **own** HIDDEN notification-area entry (`NIS_HIDDEN`, `uID = 0xC1D0` we
  control) on the tray-icon crate's message window (`TrayIcon::window_handle()`), raising balloons
  via `Shell_NotifyIcon(NIM_MODIFY, NIF_INFO)`. Registering our own entry (not reusing tray-icon's
  private `internal_id`) means **no coupling** to the crate's internal counter. Overlays stay a
  permanent non-goal; the OS balloon is the honest no-overlay analogue.
- **D-U10 — native Browse… folder picker.** New `src/ui/folder_dialog.rs` — a confined-`unsafe`
  COM wrapper over `IFileOpenDialog` (`FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM`) — **no `rfd`**. Runs
  on the settings-UI thread (winit already put it in an STA); the `PWSTR` from `GetDisplayName` is
  freed once with `CoTaskMemFree`; cancel / any HRESULT → `None` (never panics). The Browse… button
  fills the draft; the Save-time `validate_output_dir` stays the backstop for typed/TOML paths.
- **Review fixes applied:** (1) MEDIUM — reordered `Shell`'s fields so `notify` drops **before**
  `tray` (fields drop in declaration order; `Notifier`'s `NIM_DELETE` must run before
  `TrayIcon::drop` destroys the window, else the cleanup was a no-op). (2) MEDIUM — added exhaustive
  `fill_wide` unit tests (truncation, exact-fit, overflow, empty, mid-surrogate-pair — the
  buffer-safety helper behind the balloon). (3) LOW — log a `SetOptions` failure in the folder
  picker for consistency.
- **HW-owed → the §10 U8/U9/U10 manual pass** (the reviewer flagged the `NIM_ADD`-without-`NIF_ICON`
  hidden-entry registration as worth one machine-side confirm on the target Windows build; it already
  degrades gracefully — `active=false`, balloons silently disabled + logged — if it doesn't take).

## 2026-07-08 — Settings-redesign batch: scope amendments A1–A4 (recorded before coding)

Orchestrator scope amendments for the post-research settings redesign (branch
`ui-redesign-research`). Recorded verbatim before implementation per instruction; each is a
deliberate reopening/ pull-forward and is normative for the T1–T8 task batch that follows.

- **A1 — Apply-on-change; the "Save settings" button is REMOVED.** Settings write through to
  the versioned TOML the moment a control changes (no explicit Save). Consequence: the
  unknown-key / comment-preservation-on-rewrite work previously deferred to the M7 settings
  pen is **pulled forward into this batch** — the UI now rewrites `config.toml` routinely, so
  the "config is never silently rewritten" guarantee (comments + unknown keys survive) must
  hold from that commit on (`toml_edit`, already whitelisted).
- **A2 — Save-complete/-failed notification (U9) pulled forward from M10 into M7.** It is the
  *visual half of the save feature*, not polish. A real native notification (tray balloon /
  WinRT toast — never an egui window) that survives the settings window being closed and
  renders over borderless-fullscreen games.
- **A3 — Clips organized per-app at save time.** Foreground process → exe version-resource
  `FileDescription`/`ProductName`, fallback exe stem, sanitized, used as a subfolder under the
  clips dir; unknown/failure → **"Other"**. Explicitly **NOT** a game-detection database (the
  REJECTED list stands). Label resolution must **never be able to fail or delay a save**.
  Filename tokens remain M10; this is **folders only**.
- **A4 — Window geometry persistence is UI state, not config.** Size/position do **not** live
  in the user-facing `config.toml`.

## 2026-07-08 — T1 save-toast mechanism (self-owned notification window)

T1 toast mechanism: self-owned hidden window + Shell_NotifyIcon balloon with
NIN_BALLOONUSERCLICK handling. WinRT toast + AUMID rejected for now solely because
unpackaged activation requires registry writes beyond the Run key (CLAUDE.md #5 / 06-SAFETY).
Revisit at M10: the installer may legitimately register AUMID + COM activator at install
time, making WinRT the packaged-build upgrade path. Not a runtime concession ever.

Binding implementation notes: a real hidden **top-level** window (NOT a message-only
HWND_MESSAGE window — Shell_NotifyIcon callback delivery to message-only windows is
historically unreliable), never shown, WS_EX_TOOLWINDOW so no taskbar presence; the
notification-area entry carries **NIS_HIDDEN** (never a second visible clipd icon). Rich
content in the same commit: success "Clip saved · <N> s" (pts-delta length), failure distinct
+ reason; click → success opens the clip's containing folder (the per-app subfolder once T5
lands), failure opens the log folder. Toast text + the save-outcome log line are generated
from the SAME data so they can never disagree. Click handling tolerates the balloon timing
out unclicked and a newer save arriving before dismissal (latest-wins, no crash, no stuck
icon). Unsafe confined to `src/ui/notify.rs`; no new dependency. **HW-owed:** confirm on the
Nitro (Win11) that a NIS_HIDDEN entry's balloon still displays; if suppressed, STOP and flag
(migrate to a single self-owned tray icon rather than hack around it).

## 2026-07-08 — T2: apply-on-change; output folder made live; restart-bar finding

Kills the Save button (A1): all settings write-through on change via the existing
`toml_edit` preserving path (comments + unknown keys survive — proven by a new
`config::write_atomic_preserves_comments_and_unknown_keys` round-trip test). The per-field
"restart" chips and the "Saved…" line are gone — the bottom banner is the only
pending-restart voice. Apply-on-change commits per completed interaction (combo/checkbox
`.changed()`, DragValue `drag_stopped`/`lost_focus`, folder text field on focus-loss via a
separate `folder_text` buffer so a partial path never writes/creates a folder, hotkey field
on focus-loss + rebind), validated before each write; invalid input shows an inline error
and writes nothing.

**Restart-bar investigation (reported):** the **output folder no longer needs a restart** —
the save/record path resolves per-save from the ring thread's `output_dir`, so a new
`EngineCommand::SetOutputDir` hot-applies it live (mirrors `SetClearAfterSave`). But the bar
does **NOT** have zero customers: **quality, resolution, frame rate, replay length, desktop
audio, microphone, and hotkeys still require a restart** — each rebuilds the encoder / ring /
audio pipeline or re-registers the global hotkeys, which today only happens on a process
restart (an in-process epoch-reconfigure for those is a larger, later change). So the bar
mechanism stays and keeps those customers; only the folder was removed. Because customers
remain, **"Restart now" relaunches AND re-opens the settings window** (`main.rs` appends a
deduped `--reopen-settings` to the relaunch argv; `run_buffer` honors it via
`Shell::open_settings_on_start`, which re-opens the window and fires a transient "clipd
restarted — your new settings are now active" tray confirmation) — a window that vanished
after a restart reads as a crash. (Simplification: the confirmation is generic, not
per-setting-named; carrying the exact changed set across the process boundary is deferred.)
