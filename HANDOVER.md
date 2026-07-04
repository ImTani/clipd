# Session Handover — M2 merged; M3 built to the HW gate; next up: validate on the Nitro

> Onboarding note for the next session. `CLAUDE.md` and the
> `clipper-devpack/devpack/` docs are normative and override anything here.
> `02-AV-SYNC-SPEC.md` (frozen) overrides everything. `DECISIONS.md` is the
> append-only rationale log — read its 2026-07-04 entries (M3 Task 1–4) for the
> whole M3 story. `M3-PLAN.md` (repo root) is the M3 design + the two
> devpack-resolved decisions (`Arc<[u8]>` packet bytes; ring is the pipeline spine).

**Written:** 2026-07-04, end of the M3 build session. **M2 was merged into `main`**
(`--no-ff`, commit `940d0ef`) and re-confirmed green. Then **all four M3 sub-tasks
were built**: three are fully CI-green + committed, and the fourth (the live buffer
engine + hotkey) **compiles green but is NOT hardware-validated** — that is the next
action. **M1 + M2 are on `main`.**

M3 work is a **stack of four unmerged branches off `main`**, in order:
`m3-verify` → `m3-ring` → `m3-save` → `m3-buffer` (each stacked on the previous;
`m3-buffer` is checked out and contains all of M3).

| Branch | Task | State |
|---|---|---|
| `m3-verify` | M3-4 `just verify` ffprobe assertion script (`tools/verify`) | ✅ CI-green (21 tests) + smoke-tested on synthetic clips |
| `m3-ring` | M3-1 `ring.rs` packet ring (`§3`/§6.2) | ✅ CI-green (10 tests) |
| `m3-save` | M3-2 `save.rs` the `§4` rebasing | ✅ CI-green (9 tests) |
| `m3-buffer` | M3-3 hotkey + `BufferEngine` + `buffer` cmd | ⚠️ **compiles green; awaits Nitro validation** |

> **Tree is clean and green.** Root `clipd`: `just check` + `just test` =
> **130 tests**, clippy `-D warnings` + fmt clean. `tools/verify`: **21 tests**.
> `tools/avrig`: **7 tests**. Release binary **1.94 MB** (was 1.70; `global-hotkey`
> +~0.24 MB; budget 10 MB).

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

## 2. DO THIS NEXT

### 2a. Validate M3 on the Nitro (THE next action — this is the HW gate)

M3-1/2/4 are CI-green; **M3-3 (`m3-buffer`) compiles green but is unproven on
hardware**. Run the full DECISIONS.md "M3 Task 3 → TEST-MACHINE step" procedure:
```
$env:Path = "X:\cargo\bin;$env:Path"
just run -- buffer --seconds 15         # expect the "buffering … press [Ctrl+Alt+S]" banner
#   … let it run >15 s with motion + audio, then press Ctrl+Alt+S …
#   expect `save triggered` then `clip saved … <path>` in < 1 s; Enter to quit
just verify <saved-clip>.mp4            # expect ALL checks PASS
```
The whole buffer path (the global-hotkey message pump, the ring→save→mux flow) is
**unvalidated** — this run is where it's proven. First-run risks to watch:
- `WM_HOTKEY` actually firing through the pump (the finicky part — see `hotkey.rs`).
- `Ctrl+Alt+S` being free; if `could not register hotkey`, pick another in
  `[hotkeys].save_clip`.
- The `§4` rebase: `just verify`'s `save rebase origin` check asserts video@0 — the
  one thing the plan flagged to confirm on a real arbitrary-IDR window.

Then accumulate **50 consecutive saves** and `just verify clip1 … clip50` green to
close the M3 ffprobe exit criterion.

### 2b. M3-5 — the 24-hour soak (the incumbent-killer)

Once saves are proven: run `buffer` for 24 h, sample RSS + handles, assert RAM flat
(ring is byte+duration bounded) and the hour-24 clip is `just verify`-clean.

### 2c. Merge the M3 stack → `main`

After the Nitro closes M3-1/2/3/4 (+ M3-5): merge `m3-buffer` (it contains the whole
stack) into `main` — `git checkout main && git merge --no-ff m3-buffer`, re-run
`just check && just test`. The four intermediate branches can be parked/deleted.

### 2d. M3 follow-ups deferred this session (flagged in DECISIONS "M3 Task 3")

- **Buffer-mode epoch restart (`§7`)** — a mid-buffer device loss currently ends the
  session; the record path has the restart, fold it into buffer mode (ring spanning
  epochs, save picking the newest epoch per `§4.2`).
- **`auto_qp_relief` QP bump (`§6.2`)** — the ring exposes the fill signal; wire the
  60 s-sustain tracking + the live-encoder QP bump (needs on-HW tuning).
- **Byte cap uses a nominal 1080p tier** — thread the real frame size (known at the
  first frame) through to `est_bitrate_bps` for the exact `§6.2` tier.

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
