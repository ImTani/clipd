# Session Handover — next up: Milestone 2 Task 7 (engine integration)

> Onboarding note for the next session. `CLAUDE.md` and the
> `clipper-devpack/devpack/` docs are normative and override anything here.
> `02-AV-SYNC-SPEC.md` (frozen) overrides everything. `DECISIONS.md` is the
> append-only rationale log — read its 2026-07-04 entries for the M2 choices.

**Written:** 2026-07-04, after **Milestone 2 Tasks 1–5 built** (the whole audio
processing chain, as isolated + unit-tested modules). All M2 work is on branch
**`m2-audio`** (stacks Tasks 1–5 off `main`), **not yet merged** — 5 commits
(`fffbe92`..`3ae9928`). **M1 is merged into `main`.**

---

## 1. Where things stand

- **M2 is half-built: the audio *data path* is complete as tested modules; the
  *wiring* (Task 7) is not done.** capture → resample → AAC → multi-track mux all
  exist and unit-test, but nothing spawns the audio threads yet, so `clipd record`
  still produces **video-only** MP4s (unchanged from M1).
- **`just check` / `just test` green: 98 tests**, clippy `-D warnings` + fmt clean.
- **Deps added (both whitelisted):** `wasapi = "0.23.0"`, `rubato = "0.16.2"`
  (rubato pulls num-traits/num-integer/autocfg transitively). Cargo.lock committed.
- **Binary-size budget NOT re-checked since adding the two deps** — run
  `just release` at integration and confirm still < 10 MB (M1 was 1.5 MB; both
  new crates are small, so expect ample headroom, but verify).

### M2 tasks — status

| # | Task | Module(s) | Commit | State |
|---|---|---|---|---|
| 1 | gap-synth + drift controller (pure) | `audio/gaps.rs`, `audio/drift.rs` | `fffbe92` | ✅ done, CI-tested |
| 2 | WASAPI capture (loopback + mic) | `audio/wasapi_stream.rs` | `19186da` | ✅ done, **HW-validated** |
| 3 | native→48 kHz resampler + drift | `audio/resample.rs` | `4818f28` | ✅ done, DSP CI-tested |
| 4 | AAC-LC encoder | `encode/mft_aac.rs` | `7b1e16d` | ✅ built; `aac-probe` unrun |
| 5 | multi-track fMP4 (video + 2 AAC) | `mux/fmp4.rs` | `3ae9928` | ✅ done, box logic CI-tested |
| **6** | device-change state machine | `audio/devices.rs` (new) | — | ⬜ TODO |
| **7** | **engine integration** | `engine.rs`, `main.rs` | — | ⬜ **DO THIS NEXT** |
| **8** | click/flash sync rig | `tools/avrig` (new) | — | ⬜ TODO |

### What's hardware-validated vs not
- **Validated (Nitro, 2026-07-04):** `audio-probe 8` — both streams capture
  cleanly, **native rate 48000 Hz** (Realtek Headphones loopback + FIFINE mic;
  mic mono→stereo autoconvert works), **480 frames/packet**, `bad_qpc=0`,
  `ts_violations=0`, `sample_counting=false`, sub-ms jitter. The loopback-silence
  gap (§2.3) was **not** exercised (audio stayed active the whole run) — the fill
  path is unit-tested but unseen on HW; AV-3 covers it later.
- **Not yet run:** `aac-probe` (expect ASC `11 90`, ~94 AUs/2 s), any recording
  with audio, ffprobe on a 3-track file, and all A/V sync measurements.

## 2. DO THIS NEXT — Task 7: engine integration (design worked out)

Goal: `clipd record` produces an MP4 with **video + desktop-loopback + mic**
tracks, `[audio]`-config driven. The pieces all exist; this is threading + wiring.

**Thread topology to add** (per enabled stream — desktop always if
`cfg.audio.desktop`; mic if `cfg.audio.mic != "off"`):
- **audio-capture thread** — runs `wasapi_stream::run_capture(kind, tx, stop)`,
  emits `AudioPacket` (already built).
- **audio-process thread** — owns a `resample::StreamResampler` + an
  `encode::mft_aac::AacEncoder`; on start it **sends its `AudioSpecificConfig` to
  the mux setup channel** (like the encode thread sends `SendMediaType`), then per
  `AudioPacket`: `resampler.process()` → for each `ResampledChunk`,
  `f32_to_i16(&chunk.samples)` → `encoder.encode(&pcm, chunk.pts)` → send each
  `EncodedAudioPacket` to the mux. Call `resampler.finish()` + `encoder.finish()`
  on stop and flush the tail.

**Mux thread changes** (`engine.rs::mux_thread`):
- Gather the **video `SendMediaType` AND each audio track's ASC** before creating
  the writer. Simplest: a small setup phase that receives the video type + N ASCs
  (N known from config), builds `Vec<AudioTrackConfig>` (asc, channels=2,
  sample_rate=48000), then `Fmp4Writer::create(&video_type, &audio_cfgs, &path)`.
- Multiplex packet streams. Recommended: **one merged channel** carrying an enum
  `MuxItem { Video(EncodedPacket), Audio(usize /*track_index*/, EncodedAudioPacket) }`;
  every encode/process thread sends into it; the mux thread dispatches to
  `write_video_packet` / `write_audio_packet(idx, pkt)`. (Avoids `crossbeam::select!`
  over a variable number of channels.) Track index 0 = desktop, 1 = mic, matching
  §2.5 and the `AudioTrackConfig` order passed to `create`.

**Gotchas for the integration:**
- **COM `!Send`:** `AacEncoder`/`StreamResampler` must be **created and used on the
  same MTA thread** (the audio-process thread). Do NOT create the encoder on main
  and move it — mirror how the H.264 encoder lives entirely on the encode thread.
  Each audio-process thread calls `ComMta::initialize()` at entry (MTA COM rule).
  `MFStartup` is already once-per-process in `main` (record path).
- **ASC-before-container:** the mux must not `create()` until it has every ASC, or
  the moov is wrong. The audio-process thread produces the ASC at
  `AacEncoder::new()` — so send it immediately on thread start, before the capture
  packets flow.
- **Origin coupling:** `Fmp4Writer` already handles A/V alignment (first video PTS
  = origin; audio prebuffered until then, aligned by `initial_offset`). So audio
  packets can safely arrive before the first video packet — the writer buffers
  them. No extra coordination needed.
- **Config plumbing:** `RecordParams` currently carries no audio settings. Add the
  `AudioConfig` (or the derived booleans + bitrate) so `run_record` in `main.rs`
  passes `cfg.audio` through. `bitrate_bps` → `AacEncoder::new(kind, bitrate)`.
- **Shutdown:** the new threads join like the others; a merged channel closes when
  all senders drop. Keep the panic-isolation (`spawn`/`catch_unwind`) pattern.
- **Epoch restart:** audio threads should tear down + rebuild per epoch like the
  video pipeline (a clip must not span epochs, §0). Simplest: start/stop the audio
  threads inside the same `RecordingEngine` lifecycle as the video ones.

**Then validate on the Nitro:** `clipd record --seconds 15` while playing audio +
talking → `ffprobe` shows 3 streams (1 h264 + 2 aac), plays with sound, audio
duration within ~1 AAC frame of video. That is the first real A/V artifact; the
precise offset is Task 8's job.

## 3. Remaining after Task 7

- **Task 6 — device-change** (`audio/devices.rs`): `IMMNotificationClient`,
  250 ms debounce, 500 ms rebuild, RUNNING→DRAINING→REBUILDING→RUNNING (§7). The
  gap during rebuild is filled by the existing §2.3 silence synthesis (no special
  case). `AudioError` currently stringifies `wasapi` errors — this task adds proper
  `AUDCLNT_E_DEVICE_INVALIDATED` classification. Target: AV-4 (unplug mic
  mid-record, recovery gap ≤ 750 ms, no desync, no crash).
- **Task 8 — `tools/avrig`**: the click/flash rig for AV-1..AV-5 (§5). Plays an
  audible click on a full-screen white flash; measures click-vs-flash offset.
  Wire the `just rig` recipe (currently a stub). AV-2 (10-min drift ≤ 5 ms) is THE
  incumbent-killer test; AV-3 exercises the loopback-silence fill on HW.
- **The ffprobe assertion script** (track durations within 1 AAC frame, monotonic
  PTS, CFR deltas, fragment validity) is an **M3** deliverable (`just verify`
  stub) but is the natural companion to Task 8.

## 4. Environment facts (this machine = the Nitro V15 test box)

| Thing | Value |
|---|---|
| Repo root | `X:\Projects_X\clipd` |
| Rust | stable **1.95.0**, `x86_64-pc-windows-msvc` (pinned) |
| `CARGO_HOME` | `X:\cargo` (`X:\cargo\bin` **not** on the agent's default shell PATH — prepend it: `$env:Path = "X:\cargo\bin;$env:Path"`) |
| Shell for cargo/just | PowerShell (the Bash tool lacks cargo on PATH) |
| GPU | RTX 4050 Laptop (Ada NVENC) + Intel iGPU; Optimus. Primary 1080p on the dGPU |
| Default audio | **Realtek Headphones (render) + FIFINE mic (capture), both native 48 kHz** |
| ffprobe/ffmpeg | 7.x on PATH |
| Git remote | `origin` HTTPS (`github.com/ImTani/clipd`), gh authed `ImTani` |

## 5. Gotchas carried forward + new in M2

Carried from M1 (still binding): `windows` 0.62 interfaces are `!Send + !Sync`
(COM crosses MTA threads via per-type `unsafe impl Send` + SAFETY note); add ONLY
the specific `Win32_*` features for APIs actually called, same commit; `unsafe`
confined to COM/D3D/MF wrapper modules; pure logic stays 100% safe + unit-tested;
never claim a HW path "works" — claim it "builds and is ready for the procedure."

New in M2:
- **Capture is at the device's NATIVE rate**, not 48 kHz (autoconvert does
  format+channels only). This is deliberate (§2.4): rubato does native→48 kHz so
  the device-crystal drift stays measurable. On the Nitro native == 48 kHz, so the
  resampler runs near-identity — a 44.1 kHz device would exercise real resampling.
- **Drift is feed-forward on the native clock** over *contiguous* audio (gap spans
  excluded). `gaps.rs`/`drift.rs` were parameterized by rate (Task 3) — identical
  to the spec's literal `48_000` at 48 kHz, correct for other rates.
- **Output PTS after resample = anchored sample count** (`anchor + out_frames·ticks/48000`),
  legitimate because the stream is gap-filled + drift-locked. The AAC encoder does
  the same by AU index.
- **AAC priming = the §2.6 fallback constant 1024**; the exact impulse measurement
  is DEFERRED (needs Nitro + ffmpeg) — an error here is a constant offset AV-1
  catches. This is the M2 analogue of M1's deferred device-loss test.
- **The MS AAC encoder is a *synchronous* MFT** (not async like NVENC H.264) and
  wants **16-bit PCM in** (not float) → `f32_to_i16`. ASC is in the output type's
  `MF_MT_USER_DATA` after a 12-byte HEAACWAVEINFO prefix.
- **Muxer A/V alignment is origin-based, not full §4 rebasing.** The M2 record path
  aligns audio to the first video PTS; the proper save-time rebase (chosen IDR
  origin, trailing audio) is an M3 deliverable. Don't mistake the M2 alignment for
  the §4 save contract.

## 6. Still-deferred (flagged, not fixed)

- **M1: real sleep/resume device-loss rebuild** — logic validated via injection,
  but an actual GPU suspend/resume recovery is unverified on HW (see prior
  handover / DECISIONS). Still open.
- **M2: AAC priming impulse measurement** (§2.6) — fallback 1024 in use.
- **Binary-size re-check** after the 2 new deps (run `just release`).

## 7. Quick command reference

```
$env:Path = "X:\cargo\bin;$env:Path"   # prepend cargo to PATH first
just check          # fmt + clippy -D warnings + cargo check
just test           # nextest (98 tests)
just run -- audio-probe 8   # capture both streams, per-stream stats  [validated]
just run -- aac-probe 2     # AAC encoder + ASC (expect "11 90")      [unrun]
just run -- record --seconds 15   # video-only until Task 7 wires audio
```
