# Session Handover — next up: Milestone 1 (dumb recorder)

> Onboarding note for the next session. `CLAUDE.md` and the
> `clipper-devpack/devpack/` docs are normative and override anything here.
> `02-AV-SYNC-SPEC.md` (frozen) overrides everything.

**Written:** 2026-07-03, after **Milestone 0 (spikes) completed and merged to
`main`**.

---

## 1. Where things stand

- **Milestone 0 is DONE and on `main`** (CI green, `windows-latest`). All four
  spikes were built, run on the Nitro V15 / RTX 4050, validated, and merged
  (commits `c2e2cfe`..`0c468f4`). The mux decision is recorded.
- Orchestration is now at **Milestone 1 — the "dumb recorder"** (01-PLAN §6;
  tracker M1). This is the FIRST real `src/` engine code — spikes were throwaway.

### M0 findings that shape M1 (read these — they're load-bearing)
- **Encoder (spike #1):** the `NVIDIA H.264 Encoder MFT` async state machine
  (`METransformNeedInput`/`HaveOutput`/`DrainComplete`) + `IMFDXGIDeviceManager`
  + GPU-resident NV12 texture input all work. The spike used **average-bitrate**
  rate control; **M1 must switch to CQP** (spec §6.1: NVENC CQ 23 @ 1080p60) via
  `ICodecAPI` / `CODECAPI_AVEncCommonRateControlMode = Quality`.
- **Capture (spike #2):** WGC works. **Hybrid-graphics reality:** a default
  `D3D_DRIVER_TYPE_HARDWARE` device landed on the **dGPU (RTX 4050)** and WGC
  still delivered BGRA8 for the iGPU-driven 1080p panel via a cross-adapter copy
  (pitfall 14). **M1 must deliberately enumerate adapters and co-locate the
  encoder with the capture texture's adapter** (04-TEST-MACHINE topology task) —
  don't trust the default pick. SDR texture = `DXGI_FORMAT_B8G8R8A8_UNORM` (87).
- **Audio (spike #3):** the `wasapi` 0.23 crate gives per-packet QPC timestamps
  (`BufferInfo.timestamp`, 100 ns ticks = spec §2.2). Loopback = default **Render**
  device initialized with `Direction::Capture`. Not needed until M2, but proven.
- **Mux decision (spike #4): hand-rolled fragmented MP4 (`mux/fmp4.rs`).** The MF
  Sink Writer was *proven viable* (passthrough of pre-encoded H.264, honors our
  timestamps, no re-encode) and is kept as a documented fallback — but frozen
  spec §4 (crash-safe moof/mdat + atomic rename + rebasing control) decides for
  fMP4. See DECISIONS.md.

### What exists in the repo now
- `src/spec_constants.rs`, `src/clock.rs` (QPC↔ticks + MonotonicGuard),
  `src/config.rs` (versioned TOML + `--check-config`), `src/{lib,main}.rs` (thin
  shell — **engine still not wired**). 32 unit tests, `just check`/`just test`
  green, release exe ~0.45 MB.
- `spikes/` — four **standalone throwaway crates** (`mf_h264_encoder`,
  `wgc_capture_spike`, `wasapi_audio_spike`, `sinkwriter_mux_spike`), each with a
  `README.md` (repro steps + expected numbers). Never linked into `clipd`; run
  with `just spike <name>`.
- Tooling: `justfile` (`just spike NAME` now works), `.cargo/config.toml`,
  `rust-toolchain.toml` (1.95.0), `deny.toml`, CI, `DECISIONS.md` (M0 decisions +
  findings appended).

## 2. Environment facts (this machine = the Nitro V15 test box)

| Thing | Value |
|---|---|
| Repo root | `X:\Projects_X\clipd` |
| Rust | stable **1.95.0**, `x86_64-pc-windows-msvc` (pinned) |
| `CARGO_HOME` | `X:\cargo` (User env; `X:\cargo\bin` on PATH) |
| GPU | RTX 4050 Laptop (Ada NVENC) + Intel iGPU (QSV); Optimus hybrid |
| Display | 1920×1080 SDR panel — **not HDR-capable** |
| Default audio | Realtek Headphones (render), FIFINE mic (capture) |
| Git remote | `origin` HTTPS (`github.com/ImTani/clipd`), gh authed `ImTani` |

**M0 hardware/debug tools — now ALL INSTALLED:**
- GPUView + xperf (Windows ADK **10.1.26100.2454** WPT), RenderDoc
  (`C:\Program Files\RenderDoc\`), MediaInfo (**GUI build — no stdout; use
  ffprobe for assertions**), PresentMon 2.5.1 (`tools/presentmon/`, gitignored),
  `mftrace.exe` (Windows SDK 26100 bin). `ffprobe`/`ffmpeg` 7.x on PATH.

## 3. Gotchas (learned across M0 — don't retrip)

- **Spikes are standalone crates** with an empty `[workspace]` table so cargo/CI
  at the root never build or feature-unify against their heavy MF/D3D `windows`
  feature sets. Each has its own `target/` (gitignored via `/spikes/*/target/`).
  Reuse their code freely into M1 `src/` — that's what they're for.
- **`windows` 0.62.2 quirks:** the MF C-header helpers `MFSetAttributeSize` /
  `MFSetAttributeRatio` are **not exposed** — pack the u64 by hand
  (`(w<<32)|h`). `MFTEnumEx` returns a CoTaskMem array to free. Async MFTs need
  `MF_TRANSFORM_ASYNC_UNLOCK=1` + `MFT_MESSAGE_SET_D3D_MANAGER` before use.
- **`tracing` macro name collision:** a local variable named `display` breaks
  `info!(...)` (collides with the macro's internal `display` helper). Name locals
  something else.
- **Sink Writer passthrough:** `AddStream(h264_type)` + `SetInputMediaType(idx,
  h264_type)` (input == output) = no re-encode; fetch the type via
  `GetOutputCurrentType(0)` *after* streaming begins so it carries
  `MF_MT_MPEG_SEQUENCE_HEADER` for the MP4 `avcC` box.
- **Device unplug** surfaces as `AUDCLNT_E_DEVICE_INVALIDATED` (0x88890004) and
  can hand back a non-monotonic/garbage timestamp — do arithmetic in i128 /
  guard monotonicity (the M0 audio spike panicked on this before the fix).
- **MediaInfo GUI build doesn't pipe to stdout** — use `ffprobe`/`ffmpeg -f null`
  for scripted assertions (that's the M3 assertion-script path anyway).
- **`just` runs recipes under PowerShell; CI calls cargo directly** (no `just`,
  no `bc` — use `awk`). Push uses HTTPS + gh token (has `workflow` scope).

## 4. Do this next: Milestone 1 — dumb recorder (01-PLAN §6; tracker M1)

The first real engine code. No ring buffer yet — just a straight pipeline to
disk. Build under `src/capture/` and `src/encode/` (per CLAUDE.md layout), wiring
into `src/main.rs`. Checklist (tracker M1):
- [ ] Monitor → **BGRA→NV12 via `ID3D11VideoProcessor`** (pixels stay on GPU) →
      **H.264 CQP** (spec §6.1) → **MP4 on disk** (use the fMP4 writer per the M0
      decision; a first cut may use the proven Sink Writer fallback to unblock,
      then swap — note it in DECISIONS.md).
- [ ] **Correct colours: BT.709 limited range**, verified vs a reference
      screenshot (pitfall: BT.601-vs-709 + limited-vs-full is the guaranteed
      first-week bug — RenderDoc is installed for exactly this).
- [ ] **CFR maintained when the screen is static** (resubmit last frame on the
      grid; spec §1.2 — `clock.rs` already has the slot math).
- [ ] **GPUView trace** proving encode rides the Video-Encode engine, not 3D;
      **PresentMon** before/after game-frametime numbers.
- [ ] Survives monitor sleep / lock screen / sleep-resume (pipeline rebuild path
      — the epoch-restart subsystem, spec §7).

Rules for real code (vs spikes): add ONLY the specific `Win32_*` feature gates
per commit; `unsafe` confined to COM/D3D/MF wrappers with `// SAFETY:`; pure
logic (pacing grid, rebasing) 100% safe + unit-tested; `just check`/`just test`
green; branch per tracker item.

## 5. Landmines (from CLAUDE.md — still binding)

- **`windows` features:** ONLY the specific `Win32_*` gates for APIs actually
  called, same commit. Blanket features = review rejection.
- **Dependency whitelist is closed** (`tracing-subscriber` is on it but is NOT
  yet in the *core* `Cargo.toml` — add it when the engine first installs a
  subscriber; the spikes pull it independently). No async runtime, no FFmpeg, no
  vendor SDKs. Anything else → DECISIONS.md + task-summary callout.
- **No scope additions** (non-goals = the business model). **No UI before M7.**
- **Never claim a hardware path "works"** — claim it "builds and is ready for
  04-TEST-MACHINE procedure X." Only the Nitro says it works (this session's
  mic-unplug crash, caught only by the human running it, is why).

## 6. Pending / deferred (not blocking M1)

- **HDR capture path** (spike #2) is code-correct but **untestable on this
  machine** (no HDR panel). Re-verify on an HDR display someday; expect
  `DXGI_FORMAT_R16G16B16A16_FLOAT` (10).
- **Audio device rebuild on reconnect** (unplug → auto-recover) is **Milestone
  2** (§7 IMMNotificationClient). The M0 spike only proves unplug is survivable.
- **Loopback silence gap** did not reproduce on this Win11/Realtek box (engine
  stays warm, delivers continuous unflagged PCM). M2 keeps the defensive
  silence-synthesis path for hardware/OS where it does occur.
- The four `spikes/` stay in-tree as reference (never linked). Cannibalize their
  code into `src/` for M1/M2.
