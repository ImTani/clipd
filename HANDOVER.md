# Session Handover — M3 built & hardware-validated; next up: two duration runs, then merge

> Onboarding note for the next session. `CLAUDE.md` and the
> `clipper-devpack/devpack/` docs are normative and override anything here.
> `02-AV-SYNC-SPEC.md` (frozen) overrides everything. `DECISIONS.md` is the
> append-only rationale log — read its 2026-07-04 entries (M3 Task 1–4, the M3
> first/second-HW-run fixes, and the ~12 h soak) for the whole M3 story. `M3-PLAN.md`
> (repo root) is the M3 design + the two devpack-resolved decisions (`Arc<[u8]>`
> packet bytes; ring is the pipeline spine).

**Written:** 2026-07-04, end of the M3 build + validation session. **M2 was merged
into `main`** (`--no-ff`, `940d0ef`). Then **all four M3 sub-tasks were built AND the
live buffer path was validated on the Nitro** — hotkey save works, clips pass every
`just verify` check across multiple device configs, and a ~12 h soak showed no leak.
**Two duration runs + the merge remain.** **M1 + M2 are on `main`.**

M3 work is a **stack of four unmerged branches off `main`**:
`m3-verify` → `m3-ring` → `m3-save` → `m3-buffer` (each stacked on the previous;
**`m3-buffer` is checked out and contains ALL of M3** — the four tasks + the HW fixes).
Merging `m3-buffer` alone lands the whole milestone.

| Task / branch | State |
|---|---|
| M3-4 `tools/verify` (`m3-verify`) | ✅ CI-green (21 tests) + proven on real clips |
| M3-1 `ring.rs` (`m3-ring`) | ✅ CI-green (10 tests) |
| M3-2 `save.rs` §4 rebasing (`m3-save`) | ✅ CI-green (9 tests) |
| M3-3 hotkey + `BufferEngine` + `buffer` cmd (`m3-buffer`) | ✅ **HW-validated** (saves land in 64–67 ms, all verify checks PASS) |

**M3 exit criteria (`05-MILESTONE-TRACKER.md`) — validation status:**

| Criterion | Status |
|---|---|
| Ring: dual caps + whole-GOP eviction | ✅ unit-tested + live |
| Hotkey save: walk-back, rebase, atomic, < 1 s | ✅ 64–67 ms saves; video@0, CFR exact |
| Re-entrant/debounced + clear-after-save | ✅ debounce + clear seen live (soak dips) |
| ffprobe green on 50 consecutive | 🟡 **13/13 so far** — ~37 more to go |
| 24 h soak: RAM flat, hour-N clip perfect | 🟡 **~12 h clean** (no leak, 13/13 clips) — full 24 h to go |

> **Tree is clean and green.** Root `clipd`: `just check` + `just test` =
> **131 tests**, clippy `-D warnings` + fmt clean. `tools/verify`: **21 tests**.
> `tools/avrig`: **7 tests**. Release binary **1.94 MB** (budget 10 MB).
>
> **HW-validation highlights (this session):** buffer save works end-to-end on the
> Nitro; a `--seconds 30` save yields a ~30.7 s clip (§4.2 pre-roll), all 8 verify
> checks PASS across two audio device configs (Realtek+FIFINE, then Realtek+NVIDIA
> Broadcast). ~12 h soak (`ram.csv`): RAM trend **+0.22 MB/h** (flat), 30–66 MB band,
> all 13 accumulated clips pass. Three HW-found fixes landed — see "M3 first/second-HW-run
> fixes" in DECISIONS.md.

---

## 1. Where things stand

M0 (spikes) ✅ · M1 ✅ merged · M2 (audio) ✅ **merged** · **M3 (ring buffer) ✅ built
& HW-validated, unmerged** (two duration runs remain — see §2).

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

**M3 code map** (all on `m3-buffer`, which stacks the four branches):
- `tools/verify/` — the ffprobe assertion script (standalone crate; `just verify`).
- `ring.rs` — the packet ring (`§3`/§6.2): dual caps, whole-GOP eviction, audio slack.
- `save.rs` — `§4` save path: pure `select_window` (IDR walk-back, epoch clamp,
  trailing audio) + safe `save_clip` driving the **reused** `Fmp4Writer`.
- `hotkey.rs` — the Win32 message-pump wrapper for `global-hotkey` RegisterHotKey.
- `engine.rs` — `BufferEngine` (ring thread + save worker) reusing the record spawn
  helpers; `main.rs` — the `buffer` subcommand.
- `EncodedPacket`/`EncodedAudioPacket` bytes are now `Arc<[u8]>` (ring/save zero-copy).

**Deps added in M3** (whitelisted): `global-hotkey = "0.7.0"` + windows features
`Win32_UI_WindowsAndMessaging`, `Win32_System_Threading`. Cargo.lock committed.

**Fixes found on hardware this session** (all on `m3-buffer`, in DECISIONS
"M3 first/second-HW-run fixes"): (1) clip end = `min(video_end, each audio end)` so
tracks align (audio lags video ~85 ms at save time, no flush → was failing §5 AV-3);
(2) ring thread counts consumed video into `muxed` (killed a spurious "mux falling
behind" WARN); (3) retain **one GOP of pre-roll margin** beyond `buffer_seconds` so a
full-length save doesn't clamp at the eviction boundary. Plus a hidden **`--autosave N`**
test hook (fires the same §4 save on a timer) for the 50-save + soak runs.

## 2. DO THIS NEXT — two duration runs, then merge

M3 is built and HW-validated; only the two *duration* exit criteria remain (both are
just "let it run"), then land the milestone.

### 2a. Finish the 50 consecutive saves (13/13 so far)

```
$env:Path = "X:\cargo\bin;$env:Path"
just run -- buffer --seconds 5 --autosave 6      # ~40 clips in ~4 min, then Enter
just verify (Get-ChildItem X:\Projects_X\clipd\clipd_*.mp4)   # expect ~53/53 passed
```
`--autosave N` is the hidden hook (fires the real save path every N s). 13 clips from
the soak already pass; this tops up past 50. Green closes the ffprobe criterion.

### 2b. Full 24-hour soak (~12 h already clean)

Same run, but sample **Private Bytes + HandleCount** (WorkingSet is Windows-trimmed;
these are the true leak metrics):
```
# Terminal 1
just run -- buffer --seconds 30 --autosave 3600
# Terminal 2 (leave 24 h)
while ($true) {
  $p = Get-Process clipd -ErrorAction SilentlyContinue
  if ($p) { "{0},{1:N1},{2}" -f (Get-Date -Format o), ($p.PrivateMemorySize64/1MB), $p.HandleCount | Add-Content soak24.csv }
  Start-Sleep 60
}
```
Pass = Private Bytes flat + HandleCount flat over 24 h + the last clip verifies. The
~12 h WorkingSet run (`ram.csv`) already showed **+0.22 MB/h** (flat) and 13/13 clips
clean — strong preliminary evidence; this formalizes it.

### 2c. Merge the M3 stack → `main`

Once 2a + 2b are green and the tracker's M3 items are checked off (they close on the
measurement, not the build):
```
git checkout main
git merge --no-ff m3-buffer      # contains ALL of M3-1…M3-4 + the HW fixes
just check && just test          # re-confirm on main
git tag m3
```
Then park/delete the four intermediate branches.

### 2d. Deferred follow-ups (KEPT DEFERRED — do NOT start without an explicit ask)

Flagged in DECISIONS "M3 Task 3"; none block M3's exit criteria. Recommended next
pick-up is the first (real robustness gap on a laptop; also closes M1's long-open
sleep/resume validation):
- **Buffer-mode epoch restart (`§7`)** — a mid-buffer device loss (sleep/resume, TDR,
  res change) currently *ends* the session. The record path segments-and-rebuilds;
  fold that into buffer mode: keep the ring alive across the restart (it may span
  epochs — §7 "older epochs remain saveable"), rebuild capture/encode/audio into a new
  epoch feeding the same ring, and have the save worker hold a per-epoch output type
  so a save picks the newest epoch (`§4.2`) with the matching SPS/PPS.
- **`auto_qp_relief` QP bump (`§6.2`)** — the ring exposes the fill signal
  (`duration_ticks`/`caps`); wire the 60 s-sustain tracking + a live-encoder QP bump
  via `ICodecAPI::SetValue` (needs on-HW tuning — NVENC rejects some runtime props).
- **Byte cap uses a nominal 1080p tier** — the frame size isn't known at ring
  construction; thread the real `(w,h)` (the encode thread has it via `size_rx`)
  through to `est_bitrate_bps` for the exact `§6.2` tier. Small; harmless on 1080p.

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
