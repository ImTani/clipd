# Session Handover — M2 DONE & validated; next up: merge, then M3 (the ring buffer)

> Onboarding note for the next session. `CLAUDE.md` and the
> `clipper-devpack/devpack/` docs are normative and override anything here.
> `02-AV-SYNC-SPEC.md` (frozen) overrides everything. `DECISIONS.md` is the
> append-only rationale log — read its 2026-07-04 entries for the whole M2 story.

**Written:** 2026-07-04, after **Milestone 2 was completed and hardware-validated**
end to end. The audio path, the engine integration, the `§7` device-change state
machine, and the `§5` sync rig are all built, green, and proven on the Nitro. All
M2 work is on branch **`m2-audio`** (17 commits off `main`), **not yet merged**.
**M1 is merged into `main`.**

> **Tree is clean and green.** Root `clipd`: `just check` + `just test` =
> **107 tests**, clippy `-D warnings` + fmt clean. Rig `tools/avrig`: **7 tests**,
> clippy clean. Release binary **1.70 MB** (budget 10 MB).

---

## 1. Where things stand

M0 (spikes) ✅ · M1 (dumb recorder) ✅ merged · **M2 (audio) ✅ validated, unmerged**.

**M2 is complete.** `clipd record` produces video + desktop-loopback + mic, the
audio stays sample-accurate over 10 minutes, and it survives device changes. The
four M2 exit criteria are all checked off in `05-MILESTONE-TRACKER.md` with the
Nitro numbers (2026-07-04):

| Criterion | Result |
|---|---|
| Two tracks (48 kHz AAC, muxed) | ✅ ffprobe: 1 h264 + 2 aac, both audible |
| Silence-gap ≠ desync (AV-3) | ✅ 60 s silence filled, no offset jump |
| Device-change (AV-4) | ✅ mic unplug/replug: no crash, gap is silence, in sync after |
| **10-min drift (AV-2)** | ✅ **−1.92 ms** (minute-1 vs minute-10, ≤ 5 ms) |

**AV-1 / AV-5 are rig-limited, not gates** — the rig's absolute offset carries a
WASAPI-render-latency constant that varies run-to-run, so its absolute number
isn't trustworthy (AV-2's *drift*, which cancels any constant, is). See
DECISIONS.md "M2 COMPLETE".

**M2 code map** (all on `m2-audio`):
- `audio/{gaps,drift}.rs` — pure silence-synth + drift controller.
- `audio/wasapi_stream.rs` — WASAPI capture with the `§7` in-place rebuild loop.
- `audio/resample.rs` — native→48 kHz + drift correction + `switch_native_rate` + gap cap.
- `audio/devices.rs` — `§7` device-change (`IMMNotificationClient`, debounce, `DeviceSelection`).
- `encode/mft_aac.rs` — AAC-LC encoder. `mux/fmp4.rs` — video + 2 AAC tracks.
- `engine.rs` — audio capture/process threads + the merged `MuxItem` mux channel.
- `tools/avrig/` — the `§5` click/flash sync rig (standalone crate; `just rig`).

**Deps added across M2** (all whitelisted or justified): `wasapi`, `rubato`,
`windows-core` (named for the `#[implement]` macro — DECISIONS "M2 Task 6").
Cargo.lock committed.

## 2. DO THIS NEXT

### 2a. Merge `m2-audio` → `main` (first action)

M2 is validated; land it. Nothing depends on holding the branch open.
```
git checkout main
git merge --no-ff m2-audio        # a merge commit keeps the milestone legible
# (or open a PR to github.com/ImTani/clipd if you prefer review-then-merge)
just check && just test           # re-confirm green on main
```
Then delete/park `m2-audio`. Update this handover's "M1 is merged" line to include M2.

### 2b. Start Milestone 3 — the ring buffer (THE product)

This is the milestone that makes clipd *clipd*: continuous capture → a compressed
in-memory ring → a **hotkey saves the last N seconds** as a clean fMP4. M1/M2
built the "record to disk" mode; M3 builds the replay-buffer mode on top of the
same pipeline. Spec: **`02-AV-SYNC-SPEC.md §3` (ring timestamps + eviction) and
`§4` (save-path rebasing — the mux contract)**. Both are frozen; implement literally.

M3 exit criteria (`05-MILESTONE-TRACKER.md`):
- [ ] Compressed-packet ring with duration + byte caps, whole-GOP eviction (`§3`, `§6.2`).
- [ ] Global hotkey save: keyframe walk-back, timestamp rebase, atomic write-then-rename, < 1 s.
- [ ] Re-entrant / debounced saves; optional buffer clear after save.
- [ ] ffprobe assertion script green on 50 consecutive saved clips.
- [ ] 24-hour soak: RAM flat, no handle leaks, clip saved at hour 24 is perfect.

Suggested task breakdown (name branches after each):
- **M3-1 `ring.rs`** — the packet ring: hold `EncodedPacket` (video) + `EncodedAudioPacket`
  (audio) with dual caps (duration + bytes per `§6.2`) and **whole-GOP eviction**
  (never evict a partial GOP — a save needs a leading IDR). Pure, safe,
  exhaustively unit-tested (byte-cap pressure, eviction across a GOP boundary —
  CLAUDE.md testing rules). Insert it between the encode/audio threads and the sink.
- **M3-2 `save.rs`** — the save path: on hotkey, walk back to the IDR at/before the
  save start, **rebase timestamps per `§4`** (chosen IDR = origin, trailing audio,
  `§4.4`/`§4.5` slack/rounding), drive `fmp4.rs` over that window, **atomic
  write-then-rename**, < 1 s. NOTE: the M2 muxer does *origin-based* A/V alignment
  for the record path — that is NOT the `§4` save contract. M3 implements the real
  rebase; don't conflate them (see M2 gotchas below).
- **M3-3 hotkey + engine wiring** — `global-hotkey` (whitelisted) `RegisterHotKey`;
  re-entrant/debounced saves; optional buffer-clear-after-save. The buffer mode is
  a new engine lifecycle (continuous capture into the ring; save is triggered, not
  duration-bound).
- **M3-4 `just verify` (the ffprobe assertion script)** — currently a stub. Track
  durations within 1 AAC frame, monotonic PTS, CFR deltas, fragment validity;
  green on 50 consecutive saves. This is the natural companion to the rig
  (`tools/avrig`) and the first thing to build so every later save is checked.
- **M3-5 24-hour soak** — RAM flat, no handle leaks (the incumbent failure mode).

## 3. Environment facts (this machine = the Nitro V15 test box)

| Thing | Value |
|---|---|
| Repo root | `X:\Projects_X\clipd` |
| Rust | stable **1.95.0**, `x86_64-pc-windows-msvc` (pinned) |
| `CARGO_HOME` | `X:\cargo` (`X:\cargo\bin` **not** on the agent's default shell PATH — prepend it: `$env:Path = "X:\cargo\bin;$env:Path"`) |
| Shell for cargo/just | PowerShell (the Bash tool lacks cargo on PATH) |
| GPU | RTX 4050 Laptop (Ada NVENC) + Intel iGPU; Optimus. Primary 1080p on the dGPU |
| Default audio | **Realtek Headphones (render) + FIFINE mic (capture), both native 48 kHz** |
| ffprobe/ffmpeg | **7.0.1** on PATH (NB: ffmpeg 7 dropped `pkt_pts_time` — use `pts_time`) |
| Git remote | `origin` HTTPS (`github.com/ImTani/clipd`), gh authed `ImTani` |

## 4. Gotchas carried forward (M1 + M2)

Binding from M1: `windows` 0.62 interfaces are `!Send + !Sync` (COM crosses MTA
threads via per-type `unsafe impl Send` + SAFETY note); add ONLY the specific
`Win32_*` features for APIs actually called, same commit; `unsafe` confined to
COM/D3D/MF wrapper modules; pure logic stays 100 % safe + unit-tested; never claim
a HW path "works" — claim it "builds and is ready for procedure X".

New / important for M3:
- **The M2 muxer alignment is origin-based, NOT `§4` rebasing.** `fmp4.rs` aligns
  audio to the first video PTS for the record path. The `§4` save contract (chosen
  IDR origin, trailing-audio handling, head/tail slack) is an M3 deliverable in
  `save.rs`. Don't mistake one for the other.
- **Capture is at the device NATIVE rate**, resampled to 48 kHz by `rubato` so
  device-crystal drift stays measurable (`§2.4`). On the Nitro native == 48 kHz.
- **The merged mux channel** (`engine.rs` `MuxItem`) carries video + AAC AUs to one
  mux thread; track index 0 = desktop, 1 = mic (`§2.5`). The ring will sit in
  front of this.
- **`avrig` measures `pts_time`** (ffmpeg 7); the rig's absolute offset is
  latency-limited — trust its *drift*, not its absolute number.

## 5. Still-deferred (flagged, not fixed)

- **M1: real sleep/resume device-loss rebuild** — logic validated via injection,
  but an actual GPU suspend/resume recovery is unverified on HW. Still open.
- **M2: AAC priming impulse measurement (`§2.6`)** — fallback 1024 (≈ 21 ms) in
  use; shows up as part of the rig's AV-1 constant. Measurable once the rig's own
  render latency is characterized/reduced. Not blocking (AV-2 drift is the gate).
- **Rig polish (`tools/avrig`)** — reduce/calibrate the WASAPI-render click latency
  so AV-1's absolute offset becomes meaningful; a longer default flash for
  under-load runs. Optional; AV-2 doesn't need it.
- **AV-5 / load matrix** — full multi-GPU encoder-contention validation is an
  **M6** deliverable; M2's AV-5 confirmed robustness (no crash under 100 % GPU) only.

## 6. Quick command reference

```
$env:Path = "X:\cargo\bin;$env:Path"   # prepend cargo to PATH first
just check          # fmt + clippy -D warnings + cargo check   (root clipd)
just test           # nextest, 107 tests                       (root clipd)
just release        # stripped release + size vs 10 MB budget  (1.70 MB)
just run -- record --seconds 15         # video + desktop + mic (M2)
just rig flash --seconds 35             # §5 flash+click generator (Task 8)
just rig measure clip.mp4               # §5 offset + drift report
just verify clip.mp4                    # ffprobe assertion script — STUB, M3-4
cargo test --manifest-path tools/avrig/Cargo.toml   # the rig's 7 tests
```

Full M2 hardware procedures (for re-runs / regressions): **`M2-HARDWARE-TESTS.md`**.
