# Session Handover — next up: Milestone 2 (audio)

> Onboarding note for the next session. `CLAUDE.md` and the
> `clipper-devpack/devpack/` docs are normative and override anything here.
> `02-AV-SYNC-SPEC.md` (frozen) overrides everything.

**Written:** 2026-07-03, after **Milestone 1 (dumb recorder) built + validated on
the Nitro V15**. All M1 work is on branch **`m1-epoch-restart`** (stacks Tasks A–G
off `main`), **not yet merged** — the orchestrator reviews and merges the 8
commits (`7e13082`..`f551b99`) into `main`.

---

## 1. Where things stand

- **Milestone 1 is code-complete and hardware-validated.** All 8 tasks (A–G) are
  built and committed one-per-branch (stacked). `just check` / `just test` green
  (**50 tests**), clippy `-D warnings` + fmt clean, release binary **1.5 MB**
  (< 10 MB budget).
- **One M1 item is DEFERRED (user's call, 2026-07-03):** the *real* **sleep/resume
  device-loss rebuild** (§7 epoch restart). The rebuild code exists and its
  happy-path + Win+L (lock) survival are validated, but an actual device-loss has
  not been triggered on hardware. See §4 — this is the one open M1 checkbox.

### What ships — `clipd record [--seconds N] [--out PATH]`
Monitor → WGC → D3D11 `VideoProcessor` BGRA→NV12 → async H.264 MFT (CQP) →
**crash-safe fragmented MP4**. Three worker threads (capture · encode · mux) over
`crossbeam` bounded channels, driven by the CFR pacing grid, all-MTA COM. No
`--seconds` → records until Enter. On device loss it segments into `<name>-N.mp4`.
Diagnostics also shipped: `probe-gpu`, `capture-probe`, `convert-probe`,
`encode-probe`.

### M1 validation numbers (Nitro V15 / RTX 4050, 2026-07-03)
- **probe-gpu:** RTX 4050 drives the primary `DISPLAY5` 1080p; `Auto` co-locates
  the device there (same-adapter WGC copy → NVENC).
- **Pipeline:** playable MP4, **r_frame_rate = avg_frame_rate = 60/1**, CFR PTS
  deltas all exactly **1/60**; h264 / Main / avc1 / 1080p / yuv420p,
  **color_range=tv**, **bt709** primaries/transfer/matrix, `has_b_frames=0`.
  Pixel colour confirmed correct by eye.
- **fMP4:** one `moof`/`mdat` per second (5 for a 5 s clip, 60 for 60 s).
  **Crash test:** killed mid-record → orphaned `.part` plays to the last complete
  fragment (exactly 2.000 s). §4.6 crash-safety verified.
- **GPU engines** (perf counters, attributable to clipd): encode on
  **Video-Encode 37.6 %**, **3D 1.4 %** (< 3 % budget); **CPU 0.61 %** (< 2 %);
  **RAM 66.5 MB** (< 75 MB).
- **Real game (Roblox):** recorded at strict 60/1 CFR, ~6.7–7.2 Mbps under motion
  (vs ~2.7 static — CQP is content-adaptive). PresentMon before/after impact came
  out **within gameplay noise** (negative delta; Roblox scene variance ±25 %
  dwarfs clipd's overhead, consistent with the separate-engine numbers). Budget met.
- **Win+L lock:** survived, continuous 59.6 s clip, no crash, no device loss.

### Load-bearing decision: CQP vendor quirk (matters for all future encode work)
The RTX 4050 `NVIDIA H.264 Encoder MFT` **rejects** `CODECAPI_AVEncVideoEncodeQP`
and `AVEncMPVDefaultBPictureCount` (E_INVALIDARG); it **accepts**
`AVEncCommonRateControlMode = Quality`, `AVEncCommonQuality` (0-100), and
`AVEncMPVGOPSize`. So the spec's CQ is applied via `AVEncCommonQuality`, mapped
`quality = 100 − cq·100/51`. No-B-frames is left to the NVENC default (verified
`has_b_frames=0`). All `ICodecAPI::SetValue`s are best-effort (log + continue).

### Repo layout now (`src/`)
Pre-existing pure logic: `clock`, `config`, `spec_constants`. New in M1:
`gpu` (shared D3D11 device + DXGI adapter co-location), `com` (MTA + Media
Foundation RAII guards), `capture/{wgc,convert,pacing}`, `encode/mft_h264`,
`mux/{mod,sinkwriter,fmp4}` (Sink Writer kept as documented fallback), `engine`
(3-thread orchestration + epoch loop), `watchdog` (minimal §6.3 subset). Deps
added (whitelisted): `crossbeam-channel`, `tracing-subscriber`.

## 2. Environment facts (this machine = the Nitro V15 test box)

| Thing | Value |
|---|---|
| Repo root | `X:\Projects_X\clipd` |
| Rust | stable **1.95.0**, `x86_64-pc-windows-msvc` (pinned) |
| `CARGO_HOME` | `X:\cargo` (User env; `X:\cargo\bin` on PATH — **not** on the agent's default shell PATH; prepend it) |
| GPU | RTX 4050 Laptop (Ada NVENC) + Intel iGPU (QSV); Optimus hybrid. **Primary 1080p output currently on the dGPU** |
| Display | 1920×1080 SDR panel — **not HDR-capable** |
| Default audio | Realtek Headphones (render), FIFINE mic (capture) |
| Git remote | `origin` HTTPS (`github.com/ImTani/clipd`), gh authed `ImTani` |

**Debug/measure tools (installed):** GPUView + xperf (Windows ADK WPT), RenderDoc,
MediaInfo (**GUI build — no stdout; use ffprobe**), **PresentMon 2.5.1**
(`tools/presentmon/PresentMon-2.5.1-x64.exe`, gitignored; **needs an elevated
terminal** for ETW), `mftrace.exe`. `ffprobe`/`ffmpeg` 7.x on PATH. GPU
engine/CPU/RAM budgets are scriptable via `Get-Counter "\GPU Engine(*)"` /
`\Process(clipd)\*` — no screenshots needed.

## 3. Gotchas (carried forward + new in M1)

- **windows 0.62 interfaces are `!Send + !Sync`** (each wraps a bare `NonNull`).
  The engine is **all-MTA**; COM crosses threads via per-type `unsafe impl Send`
  with a `SAFETY` note (`CapturedFrame`, `InputFrame`, `SendMediaType`,
  `GpuContext`). `TypedEventHandler::new` requires a `Send` callback.
- **Feature-gate surprises:** `IDXGIOutput6::GetDesc1` needs `Win32_Graphics_Gdi`;
  `VARIANT` (for `ICodecAPI::SetValue`) needs `Win32_System_Ole` **and**
  `Win32_System_Com` (plus `Win32_System_Variant` for `VT_*`); `windows` has no
  `From<u32>` for `VARIANT` (hand-build the union).
- `FrameArrived` returns a bare `i64` token in 0.62 (no `EventRegistrationToken`).
- **NV12 output pool must exceed the input-channel depth** (pool 8 > cap 4) or a
  queued frame's texture gets recycled under it. No GPU fence yet (deferred).
- **The MF H.264 encoder emits Annex-B**; the fMP4 writer converts each sample to
  length-prefixed AVCC and strips SPS/PPS/AUD (they go in `avcC`, read from
  `MF_MT_MPEG_SEQUENCE_HEADER`).
- `std::fs::rename` on Windows replaces atomically (MoveFileEx) — no delete-then-
  rename window needed for the `.part` → final swap.
- `just`/cargo run under PowerShell; the agent's Bash shell lacks cargo on PATH.

## 4. DEFERRED M1 item — real device-loss / sleep-resume rebuild (Task G)

The epoch-restart path is built AND its **software logic is validated**:
`EngineError::is_device_lost` classifies `DXGI_ERROR_DEVICE_REMOVED/_RESET`;
`record` is an epoch loop that finalizes the segment and rebuilds a fresh
`GpuContext`+pipeline within the 2 s budget, segmenting into `<name>-N.mp4` (a
clip must not span epochs). The hidden `--simulate-device-loss <secs>` test hook
injects a synthetic device loss; a run proved **finalize → detect → rebuild →
new segment → continue**, producing two playable segments. That validation also
**caught a real bug** (the engine had shared the record loop's stop flag, so
finalizing epoch 0 tripped the loop's stop — the engine now owns its own internal
stop flag). Happy path + Win+L lock survival also validated.

**Still NOT validated:** that real WGC/D3D objects *recover* after an ACTUAL
sleep/resume (the injection uses a fresh device on rebuild, which trivially
succeeds; a real resume must re-init WGC + the video processor + the MFT after
the OS suspended the GPU). To close it on hardware:
`clipd record --seconds 90`, then **Start → Power → Sleep** and wake (lid sleep is
disabled on this box), or force a driver TDR, mid-record. Expect: no crash, a
`device lost … segment saved` line, a fresh `<name>-1.mp4`, both segments playable.

## 5. Do this next: Milestone 2 — audio (01-PLAN §6; tracker M2)

Add the audio path and A/V sync. Checklist:
- [ ] Desktop **WASAPI loopback** + **mic** capture, resampled to 48 kHz
      (`rubato`), **AAC**-encoded (MF AAC MFT), muxed as **two separate tracks**.
- [ ] **Silence-gap synthesis** — loopback goes quiet ≠ desync (spec §2; pitfall 2).
- [ ] **Device-change handling** — unplug mic / switch default output mid-record;
      recording continues, gap is silence, log lines written (§7 `IMMNotificationClient`,
      250 ms debounce, 500 ms rebuild budget; pitfall 3).
- [ ] **A/V offset within ±1 frame @ 60 fps over a 10-minute recording** measured
      with a click/flash tool (proves no drift; §5).

Cannibalize `spikes/wasapi_audio_spike/` (per-packet QPC timestamps via
`wasapi` 0.23 `BufferInfo.timestamp` = spec §2.2; loopback = default Render device
opened `Direction::Capture`). The audio thread joins the engine as a 4th worker;
audio packets carry QPC-derived PTS in the same master clock domain, and the
muxer gains a second (AAC) track. `spec_constants::audio` + `::drift` + `::aac`
already hold every constant. Mind pitfall 7 (AAC priming delay ~1024 samples →
~21 ms lead if ignored) and pitfall 4 (virtual audio devices).

## 6. Landmines (from CLAUDE.md — still binding)

- **windows features:** only the specific `Win32_*` gates for APIs actually
  called, same commit. Blanket features = review rejection.
- **Dependency whitelist is closed.** No async runtime, no FFmpeg, no vendor SDKs.
  Anything else → DECISIONS.md + task-summary callout.
- **No scope additions** (non-goals = the business model). **No UI before M7.**
- **Never claim a hardware path "works"** — claim it "builds and is ready for the
  04-TEST-MACHINE procedure." Only the Nitro says it works.
- `unsafe` confined to COM/D3D/MF wrapper modules with `// SAFETY:`; pure logic
  (pacing, fMP4 box math, drift) stays 100 % safe + unit-tested.
