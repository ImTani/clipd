# Session Handover — next up: M2 Task 6 (device-change) + Task 8 (avrig)

> Onboarding note for the next session. `CLAUDE.md` and the
> `clipper-devpack/devpack/` docs are normative and override anything here.
> `02-AV-SYNC-SPEC.md` (frozen) overrides everything. `DECISIONS.md` is the
> append-only rationale log — read its 2026-07-04 entries for the M2 choices.

**Written:** 2026-07-04, after **Milestone 2 Tasks 1–5 + 7 built** — the whole
audio processing chain *plus* the engine integration that wires it into
`clipd record`. All M2 work is on branch **`m2-audio`** (off `main`). **M1 is
merged into `main`.**

> ⚠ **UNCOMMITTED WORK IN THE TREE (read first).** Two logically separate,
> green units are sitting uncommitted on `m2-audio`, in this order:
> 1. **The M2 quality-audit fixes** (`src/audio/resample.rs` + the DECISIONS /
>    HANDOVER audit sections) — the two sync-math fixes + 2 regression tests
>    (98→100). Committed as its own unit is cleanest.
> 2. **Task 7 engine integration** (`src/engine.rs`, `src/main.rs`, the
>    DECISIONS "M2 Task 7" entry, this file). Depends on nothing in #1's code.
>
> Suggested split: commit #1 (`M2 quality audit: drift-pairing + group-delay`),
> then #2 (`M2 Task 7: engine integration (audio threads + merged mux)`).
> `git add -p` cleanly separates them (they touch disjoint code files; only
> DECISIONS.md/HANDOVER.md overlap). Last **committed** M2 commit is
> `b62c76f` (the pre-Task-7 HANDOVER rewrite).

---

## 1. Where things stand

- **M2 is half-built: the audio *data path* is complete as tested modules; the
  *wiring* (Task 7) is not done.** capture → resample → AAC → multi-track mux all
  exist and unit-test, but nothing spawns the audio threads yet, so `clipd record`
  still produces **video-only** MP4s (unchanged from M1).
- **`just check` / `just test` green: 100 tests**, clippy `-D warnings` + fmt clean.
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
| 7 | engine integration | `engine.rs`, `main.rs` | *(uncommitted)* | ✅ built + CI-green; **HW-unvalidated** |
| **6** | device-change state machine | `audio/devices.rs` (new) | — | ⬜ **DO THIS NEXT** |
| **8** | click/flash sync rig | `tools/avrig` (new) | — | ⬜ TODO |

(Task 7 was done before Task 6 — the handover's recommended order — so a real
3-track artifact exists to test the device-change work against.)

### What's hardware-validated vs not
- **Validated (Nitro, 2026-07-04):** `audio-probe 8` — both streams capture
  cleanly, **native rate 48000 Hz** (Realtek Headphones loopback + FIFINE mic;
  mic mono→stereo autoconvert works), **480 frames/packet**, `bad_qpc=0`,
  `ts_violations=0`, `sample_counting=false`, sub-ms jitter. The loopback-silence
  gap (§2.3) was **not** exercised (audio stayed active the whole run) — the fill
  path is unit-tested but unseen on HW; AV-3 covers it later.
- **Binary size re-checked (2026-07-04, post-Task-7):** `just release` →
  **1.70 MB** (M1 was 1.5 MB), well under the 10 MB budget. Deferred item cleared.
- **Not yet run:** `aac-probe` (expect ASC `11 90`, ~94 AUs/2 s), **Task 7's
  3-track `record`** (the first real A/V artifact — procedure in §2 below),
  ffprobe on a 3-track file, and all A/V sync measurements.

## ⚠ 2026-07-04 quality-audit pass (pre-integration) — PRIORITY

A **dedicated audit pass** (spec-vs-code review of Tasks 1–5, all six M2
modules) ran on 2026-07-04 before Task 7. Two sync-math bugs were found and
**fixed** on `m2-audio` (DECISIONS.md "M2 quality audit" entry); two design
gaps remain **OPEN and take priority** — treat them as requirements folded
into Tasks 6/7, not suggestions.

**Fixed in this pass (+2 regression tests, 98 → 100):**
1. `audio/resample.rs` — the drift window paired each QPC span with the
   **wrong packet's frame count** (current instead of previous). Invisible with
   constant 480-frame packets (why the Nitro probe looked clean), but variable
   packet sizes (WASAPI double/triple periods after scheduling hiccups) injected
   up to ~330 ppm of phantom drift — larger than the 20–200 ppm signal AV-2
   measures.
2. `audio/resample.rs` — output PTS ignored rubato's sinc **group delay**
   (`output_delay()` ≈ 64 frames ≈ 1.33 ms): the whole audio signal was stamped
   one group delay early, a constant offset the §5 budget never accounted for.
   PTS now subtracts it (the first chunk legitimately starts ~13,333 ticks
   before the anchor; the muxer's pre-origin handling absorbs that).

**OPEN — must be designed into Tasks 6/7 (priority over new feature work):**
3. **Gap-fill is unbounded** (`gaps.rs` → `resample.rs::push_silence`). QPC
   keeps counting through suspend, so a sleep/resume can demand *hours* of
   synthesized silence → GB-scale allocation ground through rubato + AAC (and
   past ~24.8 h the `u32` frame cast truncates). The spec has no cap; decide one
   (e.g. gap > buffer_seconds → re-anchor / audio epoch restart) and record it
   in DECISIONS.md. The 60 s AV-3 case is fine (~23 MB, one burst).
4. **Device rebuild must preserve the contiguity chain.** The muxer places only
   the FIRST AU by PTS and butt-joins the rest; the AAC encoder and resampler
   both count from a single anchor. §7's "silence synthesis needs no special
   case" holds **only if `StreamResampler`/`AacEncoder` survive the rebuild** —
   Task 6 must recreate the WASAPI client *below* them, not the processing
   chain. And a replacement device at a different native rate has no re-anchor
   path today: `StreamResampler` needs a rate-switch, or a rate change must be
   an explicit epoch restart. Decide in Task 6.

**Minor open (audit, non-blocking):** cap the drift controller's `dt` at the
10 s interval (an update after a long silent span may currently step the ratio
to the full ±300 ppm in one go — the audible step §2.4 warns about);
`fmp4.rs` `initial_offset` floors where §4.5/DECISIONS say round (≤ 20.8 µs,
once); the muxer silently drops pre-origin AUs and never-aligned prebuffer at
`finish()` — add logs (the "why didn't my clip save" trust model);
`annexb_nals` trims trailing zeros that could be legal `cabac_zero_words`
(note-only); in §2.2 sample-counting mode drift measurement degenerates to
0 ppm by construction (physically inevitable — document, don't "fix").

## 2. Task 7 — engine integration (BUILT, needs HW validation)

`clipd record` now spawns the audio pipeline and writes **video + desktop +
mic**, `[audio]`-config driven. Wiring only; no spec change, no new deps. Green
on `just check` + `just test` (100 tests — unchanged; this is thread wiring,
validated by the on-machine `record`, not a unit test). **Design + decisions:
DECISIONS.md "M2 Task 7".** What landed:

- **`engine.rs`**: a `MuxItem { Video(EncodedPacket), Audio(track_index,
  EncodedAudioPacket) }` merged channel; one `audio-capture` + one
  `audio-process` thread per enabled stream inside the `RecordingEngine`
  lifecycle (so they tear down + rebuild per epoch with the video pipeline);
  `mux_thread` collects the video type + N ASCs, `create`s the multi-track
  writer, then dispatches merged items. Audio-stage failures are **non-fatal to
  the video clip** (logged; the clip finalizes with the AUs it got).
- **`main.rs`**: `cfg.audio` → `RecordParams` (`desktop_audio`, `mic_audio =
  mic != "off"`, `audio_bitrate_bps`); banner prints the active audio set.
- **One refinement over the pre-Task-7 design:** the `StreamResampler` needs the
  device native rate, which only arrives on the first `AudioPacket`, so it is
  built **lazily on packet 1**; the `AacEncoder` (and its ASC) has no such
  dependency and is built at thread start, so the ASC handoff still happens
  before any data (moov is correct). See the DECISIONS entry.

**DO THIS NEXT — validate Task 7 on the Nitro (the first real A/V artifact):**
```
$env:Path = "X:\cargo\bin;$env:Path"
just run -- record --seconds 15      # while playing desktop audio AND talking
```
Expected: exit 0; console says `audio: desktop+mic`. Then on the output file:
```
ffprobe -hide_banner <file>.mp4      # expect 3 streams: 1 h264 + 2 aac (48 kHz stereo)
```
- Plays with **both** desktop sound and mic audible; A/V looks in sync by ear.
- Each audio track's duration is within ~1 AAC frame (~21 ms) of the ~15 s video.
- Log shows two `audio capture started` lines + one `recording finalized`.
- The precise click/flash offset is **Task 8's** job; this run only proves the
  three tracks mux, decode, and play.

If the mux errors with `ChannelClosed`, an audio-process thread died before its
ASC handoff (e.g. AAC activate failed) — check the `audio-process` worker log.

## 3. Remaining after Task 7

- **Task 6 — device-change** (`audio/devices.rs`): `IMMNotificationClient`,
  250 ms debounce, 500 ms rebuild, RUNNING→DRAINING→REBUILDING→RUNNING (§7). The
  gap during rebuild is filled by the existing §2.3 silence synthesis (no special
  case). `AudioError` currently stringifies `wasapi` errors — this task adds proper
  `AUDCLNT_E_DEVICE_INVALIDATED` classification. Target: AV-4 (unplug mic
  mid-record, recovery gap ≤ 750 ms, no desync, no crash). **Audit items 3 & 4
  above are requirements for this task**: rebuild below a surviving
  `StreamResampler`/`AacEncoder`, decide the native-rate-change policy, and cap
  the gap fill.
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
- **M2 audit item #3 (unbounded gap fill)** — reassigned to **Task 6** (with item
  #4); see the DECISIONS "M2 Task 7" entry for why it is not a one-liner and why
  Task 7 doesn't trigger it. Still open, now scoped.
- ~~Binary-size re-check~~ — **done 2026-07-04: 1.70 MB, within budget.**

## 7. Quick command reference

```
$env:Path = "X:\cargo\bin;$env:Path"   # prepend cargo to PATH first
just check          # fmt + clippy -D warnings + cargo check
just test           # nextest (100 tests)
just run -- audio-probe 8   # capture both streams, per-stream stats  [validated]
just run -- aac-probe 2     # AAC encoder + ASC (expect "11 90")      [unrun]
just run -- record --seconds 15   # NOW: video + desktop + mic (Task 7); HW-unvalidated
```
