# Session Handover — A3 (VU meters) DONE + HW-verified; A4 (status strip) is next

> Onboarding note for the next session. `CLAUDE.md` and the `clipper-devpack/devpack/`
> docs are normative and override anything here. `02-AV-SYNC-SPEC.md` (frozen) overrides
> everything EXCEPT the dated `DECISIONS.md` amendments: the two M4-2 entries, the three
> **2026-07-07** entries (§2.2 process-loopback QPC, §2.5 track layout, §4 hybrid-moov),
> the **"T0 resolution"** entry (§6.1 CQP → bitrate-target VBR), the **"A1"** entry (config
> schema v2 / quality tiers / `toml_edit`), the **"A2"** entry (eframe/egui settings window /
> satellite thread / `winit` dep), and now the **"A3"** entry (lock-free `AudioLevels` /
> VU-meter seam). Read **`M7-M8-PLAN.md`** (repo root) — it is the working plan for this whole
> phase; you are at Slice A task **A4**.

**Written:** 2026-07-07, after **A3 was implemented, self-reviewed, rust-reviewer'd, merged to
`main`, and HW-validated on the Nitro.** This session added the first live UI data path: the VU
meters that render audio levels in the settings window.

---

## 1. Code state

- **M0–M5 + T0 + A1 + A2 + A3 merged on `main`.** Working tree clean. **197 tests** (nextest;
  +11 from A1/A2's 186 — all in the new pure-logic `audio/levels.rs`). `just check` (fmt +
  clippy -D warnings + check) green. Release build **8.30 MB** (8,703,488 bytes) vs the 10 MB
  budget — **+19 KB from A2's 8.28 MB** (the meter code is tiny; the eframe/egui/winit/glow
  stack was already linked in A2). ~1.7 MB headroom left.
- **A3 is HARDWARE-VERIFIED on the Nitro** (2026-07-07): both meters track their stream
  (desktop follows system audio, mic follows speech) and decay to silence when quiet. A3
  acceptance met (see §5 + DECISIONS "A3").
- Last commits: `e8ce8e5` Merge a3-vu-meters → `daf4c59` the A3 feat commit (+ this doc commit
  on `main`).
- **`main` is ahead of `origin/main`** (A1/A2/A3 feat+merge + handover/DECISIONS docs).
  `origin/main` = `5ac1040`. **Not pushed** (orchestrator chose leave-local through A1–A3).
  Push when ready (`git push`; remote HTTPS `github.com/ImTani/clipd`, gh authed `ImTani`).
- **Still owed (M7 acceptance, not A3-specific):** the **2 h open-window soak** — zero engine
  stalls attributable to the UI thread. Not yet run; do it during a longer session before M6
  sign-off.

---

## 2. What A3 changed + the seam (READ before touching audio levels / the meters)

**New pure-logic module `src/audio/levels.rs`** — the level-publishing type + the meter math.
Full rationale: `DECISIONS.md` "2026-07-07 — A3". The load-bearing facts:

- **The level path is engine PUBLISHES → UI READS, lock-free, one-directional.** `AudioLevels`
  is an `Arc`-shared struct of an `AtomicU32` peak/rms pair (f32 stored as bit patterns,
  `Relaxed`) **per `AudioStreamKind`** — keyed by *kind*, not index, so there is zero
  producer/consumer index coupling. The engine's `audio_process_thread`s write; the settings
  window reads. It deliberately does **NOT** route through `ShellSignal` (that channel is the
  tray's single, state-only consumer). Keep this direction: `ui` only holds a clone of the
  `Arc` and reads — `AudioLevels` lives in `audio`, nothing in `engine` references `ui`.
- **Levels are computed on the raw captured `AudioPacket`** (native f32), once per packet,
  before resample — the packet is already in hand (no copy), and resampling barely moves
  amplitude. Silence-flagged packets skip the scan and publish zero. A stream that stops
  (device loss / epoch rebuild / shutdown) **publishes silence on exit** so its meter decays
  instead of freezing at the last level (the "live indicator, dead thread" lie this project
  exists to kill — do not regress it).
- **`Arc<AudioLevels>` is created in `BufferEngine::start`** (main thread, before the
  supervisor spawns) so the shell can clone it synchronously via `engine.audio_levels()`, and
  is cloned into every producer set — it **survives §7 epoch rebuilds**. `engine.audio_streams()`
  returns the enabled kinds (the meter set); `enabled_audio_kinds(params)` is the single source
  of truth for both that list and the supervisor's capture list, so they can't drift.
- **Store-latest, not a peak-hold.** A VU meter tolerates missing a sub-33 ms transient between
  the ~100 Hz publish and the 30 fps read; store-latest avoids reader/writer coupling and a
  stale-peak spike on reopen. The "fast tip" comes from the UI animation (instant attack, slow
  release via the pure `release_toward`), not the publish side.
- **Meter animation is repaint-gated on `Shared.visible`** (settings.rs): the app clears it on
  the close-intercept (hide-to-tray), the tray sets it on re-show. A hidden window idles at zero
  CPU; a stale post-hide repaint sees `false` and lets egui idle rather than spinning a hidden
  window at 30 fps. This flag — not an inferred per-frame heuristic — is the source of truth for
  "should animate." The A2 one-loop-per-process reopen model is unchanged.

### A2 pain points that still bite (carried forward — the meters live on the A2 window)

1. **eframe 0.35 has the REDESIGNED `App` trait**: `logic(&mut self, ctx, frame)` (non-drawing
   per-frame work — close-intercept, context publish, repaint scheduling) + `ui(&mut self, ui,
   frame)` (drawing). The handed `Ui` has **no margin/background** — wrap in
   `egui::Frame::central_panel(ui.style()).show(ui, …)`. Any egui snippet from pre-0.32 docs/LLM
   memory is wrong; translate against the pinned source.
2. **The crate source cache is under `C:\Users\tanis\.cargo\registry\src\index.crates.io-*`**,
   NOT `X:\cargo` (that holds `bin/` only). Grep crate internals there (this is how A3 confirmed
   the egui 0.35 painter/`Visuals` API before writing `draw_meter`).
3. **`winit = "=0.30.13"` is a direct dep** (A2) pinned to what eframe 0.35 resolves, for
   `with_any_thread(true)` (off-main-thread event loop). eframe uses
   `default-features = false, features = ["glow","default_fonts"]` (no wgpu/persistence).
4. **Cross-thread `egui::Context` is sound** (reviewer-verified against egui 0.35):
   `send_viewport_cmd`/`request_repaint` queue into an internally-locked buffer, never touch a
   winit `Window`/HWND from the calling thread. This is how the tray drives the window and how
   A3 gates animation.
5. **The first eframe build is SLOW** (~6 min release cold). It's built now, so incremental
   `check`/`test` are seconds; a cold `cargo check` still needs a backgrounded run or a long
   timeout (the 2-min default Bash timeout kills it).

---

## 3. DO THIS NEXT — A4 (status strip)

Full task text in `M7-M8-PLAN.md` §3. Order within Slice A = devpack priority (meters were the
highest-value element and shipped first in A3; status strip is next, editor after). Branch per
task (`a4-status-strip`).

- **A4 — status strip** in the settings window: engine state, buffer fill (seconds held vs
  configured), capture target, res/fps/codec/vendor, last save result + time, dropped-frame +
  watchdog counters.
- **The satellite-law seam to solve first (same shape A3 just established):** the status fields
  live on the engine (`PipelineStats` counters, `ShellSignal::State`, the ring's buffer fill,
  save results). You need a **UI-readable status path the engine PUBLISHES and the window READS**.
  Two publish mechanisms already exist to reuse rather than reinvent: (a) `PipelineStats` is a
  set of `Arc<Atomic*>` counters (`captured`/`encoded`/… — the same pattern `AudioLevels` used),
  cloneable to the window; (b) `ShellSignal` already carries `TrayState` to the tray. Decide
  whether the status strip reads the stats `Arc`s directly (lock-free, like the meters) or gets a
  richer snapshot pushed — prefer the former where the data is already an atomic. Do NOT make the
  engine depend on the window. Anything derived (e.g. "N s / X MB buffered") is pure — unit-test
  the mapping like `levels.rs`.
- Then **A5** settings editor (writes via A1 `Config::write_atomic`; shows derived Mbps/RAM from
  `video_target_bitrate_bps × quality.multiplier()`) · **A6** press-to-bind hotkeys · **A7**
  recent-clips list · **A8** `just dist` beta zip.
- After A8: friends-beta v0 (2-track, full UI), then Slice B (B1–B7, 4-track audio), then M6
  closes on beta evidence.

`M7 acceptance` (from 08): cold-open < 300 ms (A2: measured 385 ms, **accepted** — driver-bound,
first-open-only); 2 h open-window soak, zero engine stalls attributable to UI (**still owed**).

---

## 4. Research facts the next session must not re-derive (sourced in M7-M8-PLAN §5)

Carried forward — all still relevant for A4–A8 / Slice B:

- **Process loopback** (`ActivateAudioInterfaceAsync` + PROCESS_LOOPBACK): Win10 19041+
  (docs claim 20348 — runtime-probe, hide below floor), anti-cheat-safe. Client is crippled
  (GetMixFormat/IAudioClock/GetStreamLatency E_NOTIMPL) BUT `GetBuffer.QPCPosition` is valid
  and IS our tick master domain (OBS 28+ trusts it). Request 48 kHz f32 (honored). Silence =
  SILENT-flagged packets (keep gap synthesis armed). Process exit ⇒ silence forever, no
  error — needs our own PID-liveness watchdog. Serialize activations. No new dep — whitelisted
  `wasapi` has `new_application_loopback_client` (its `include_tree:false` doc comment is
  WRONG — code does EXCLUDE mode).
- **VC detection:** by process enumeration, NEVER by window (tray-minimized Discord breaks
  window pickers). Discord = top-most `Discord.exe` (parent not same-name) + include-tree
  (audio in an Electron child). Ships as TOML table: Discord/PTB/Canary (P0 — **A1 seeded
  this as the default `vc_apps` entry already**), Vesktop/Legcord/TS3/TS6/Mumble (P1), Steam
  voice + Game Bar (P2). Skype + Guilded are DEAD — never add. In-game voice
  (Vivox/EOS/Steamworks: Valorant/Fortnite/Apex/LoL) renders INSIDE the game process — never
  separable → LIMITATIONS.md. Only Medal auto-detects Discord today (a differentiator).
- **4-track layout (Slice B):** mix FIRST (track 1; one-track players/CapCut/Discord/YouTube
  use exactly it), then game / voice-chat / other-system / mic when `separate_tracks=true`;
  mix+mic when false. All tracks flagged enabled. "Other system" contains VC too (API can't
  express system−game−VC) — accepted, documented.
- **Container:** MKV folklore doesn't apply; fMP4-on-disk quirks solved by the approved
  OBS-Hybrid appended-`moov`-on-save (§4 amendment). Uploads flatten to one track; editors
  read all enabled tracks.
- **Competitor defaults:** Steam 12 Mbps default tier / NVIDIA ~20–50 computed / Medal 3–100
  slider; only OBS exposes CQP. Resolution UX: "Source (recommended)" + downscale tiers, hide
  options above source (rides our `encode.resolution`/`effective_max_height` canvas).

---

## 5. Environment facts (this machine = the Nitro V15 test box)

| Thing | Value |
|---|---|
| Repo root | `X:\Projects_X\clipd` |
| Rust | stable **1.95.0**, `x86_64-pc-windows-msvc` (pinned) |
| `CARGO_HOME` | `X:\cargo` (`X:\cargo\bin` NOT on PATH — prepend `$env:Path = "X:\cargo\bin;$env:Path"`; in the Bash tool: `export PATH="/x/cargo/bin:$PATH"`) |
| Crate **source cache** | `C:\Users\tanis\.cargo\registry\src\index.crates.io-*` (NOT `X:\cargo`; this is where you grep crate internals — e.g. the egui 0.35 painter API for A3) |
| GPU | RTX 4050 Laptop (Ada NVENC) + Intel iGPU; Optimus. Primary **1080p** on the dGPU |
| Default audio | Realtek Headphones (render) + FIFINE mic (capture), both 48 kHz |
| ffmpeg/ffplay/ffprobe | 7.0.1 on PATH (ffplay is a **chocolatey shim** — see gotchas) |
| Config file | none by default — `%APPDATA%\clipd\config.toml` by hand. Hotkeys: save `Ctrl+Alt+S`, record `Ctrl+Alt+F9` |
| Log file | `%LOCALAPPDATA%\clipd\logs\clipd.log.<date>` |
| Git remote | `origin` HTTPS (`github.com/ImTani/clipd`), gh authed `ImTani`. `origin/main` = `5ac1040`; local `main` ahead (A1+A2+A3+docs) — push when ready |
| Zombie procs | `Get-Process clipd,ffplay -EA SilentlyContinue \| Stop-Process -Force` between runs |
| Local cruft (gitignored) | `ram.csv` (M5 RAM-budget log — delete if unneeded) |

### A3 HARDWARE TEST — DONE (Nitro V15, release binary, 2026-07-07)

- ✅ Tray **Settings…** → the window shows an "Audio levels" section with a meter per enabled
  stream (Desktop + Microphone).
- ✅ **Desktop meter tracks system audio** (play something → bar rises, peak tick leads the RMS
  body); **mic meter tracks speech**.
- ✅ Both meters **decay to silence** when their source goes quiet (VU release).
- ✅ Meters animate only while the window is visible; close-to-tray → reopen resumes cleanly
  (visibility-gated repaint, no hidden-window spin).
- ⏳ **Still owed (M7 acceptance, not A3-specific):** the **2 h open-window soak** — zero engine
  stalls attributable to the UI thread. Run during a longer session before M6 sign-off.

### A2 HARDWARE TEST — DONE (Nitro V15, release binary, 2026-07-07)

- ✅ Window opens on the dGPU (glow/WGL, RTX 4050, GL 3.3); close (X) → hides; re-click → re-shown,
  **no panic**; save with the window open unaffected; tray **Quit** clean teardown, no hang.
- ⚠️ **Cold-open 385 ms** (release) vs the < 300 ms target → **accepted + documented** (DECISIONS
  "A2 HW validation"): driver-bound (WGL context on the Optimus dGPU), first-open-only.

---

## 6. Gotchas carried forward (+ new A3 ones)

**New from A3:**
- **Level path is `Arc<AudioLevels>` (atomics), NOT `ShellSignal`.** Keep any new UI-data seam
  (A4 status!) the same shape: engine publishes to a lock-free `Arc`, UI reads a clone; never the
  reverse. Publish silence/zeroed state when a producer stops so the UI decays instead of lying.
- **`AudioStreamKind::COUNT` is a manual literal** guarded by a `const _` assert in `levels.rs`.
  When Slice B (B1) adds a stream variant, bump `COUNT` and extend both the assert and the
  meter-color/label paths — the exhaustive `index()` match will force the arm.
- Meter animation runs ~30 fps **only while visible**; do not add always-on repaints (a hidden
  window must idle). `stable_dt` from `ui.input` drives the decay; the meter chrome reads
  `ui.visuals()` so it adapts to a system light theme.

**Carried from A2:**
- eframe 0.35 App trait = `logic()` + `ui()` (NOT `update()`); handed `Ui` has no bg — wrap in
  `egui::Frame::central_panel`. Crate source cache is under `C:\Users\tanis\.cargo`. `winit`
  is a direct dep (=0.30.13) for `with_any_thread`. Settings window is a satellite on its own
  thread; keep `ui → engine` one-directional.
- **Cold-open ~385 ms (release), over the 300 ms target but ACCEPTED** (driver-bound WGL context
  init on the Optimus dGPU, first-open-only). Do NOT "fix" it by pre-warming a hidden context at
  startup unless the orchestrator flips the decision (rejected — holds VRAM all session for a
  maybe-never-opened window). See DECISIONS "A2 HW validation".

**Carried from A1:**
- `toml_edit` is a SEPARATE crate from `toml` 1.x; added explicitly, no `serde` feature.
- Config **writes go through `Config::write_atomic` only**; use `effective_max_height()`, not
  `max_height`. Quality tiers = bitrate multipliers (never CQ). `[audio.tracks]`/`vc_apps`
  are schema-only until Slice B.

**Carried from T0:**
- **Exclusive fullscreen starves WGC monitor capture** → no frames → encode thread blocks on
  `size_rx.recv()` → `stop_and_join` hangs forever. Drive on-screen test content with a
  **borderless window**, never `ffplay -fs`.
- **Chocolatey `ffplay` is a shim** that spawns real ffplay and exits — kill ffplay **by
  name**, not by the `Start-Process -PassThru` PID.
- **`--encode-*` hooks contaminate "no bitrate target" tests** (`EncoderOverrides::is_default()`
  gates the shipping PCVBR default). PCVBR peak cap (1.5× avg) was never approached even by
  mandelbrot — pure byte-cap safety.

**Carried earlier:**
- **Run `buffer` via `just run buffer`** (NOT `just run -- buffer`). Tray mode: Enter does
  not quit — use tray Quit. New icons hide in the Win11 "^" overflow flyout.
- **`common-controls-v6` breaks binary load** (DECISIONS "M5 T2 fixup") — keep `tray-icon`
  default-features off; `tests/smoke.rs` guards it. eframe + the A3 meters did NOT reintroduce
  this (smoke `version_loads_and_runs` passes with the full UI stack linked).
- `--simulate-device-loss` is headless by design. `clip shorter than requested (§4.2)` on a
  young buffer is EXPECTED.
- Carried M1–M4: `Closed` doesn't fire on window close → `IsWindow` poll; fixed canvas
  letterboxes odd aspects; `windows` 0.62 COM interfaces `!Send`/`!Sync`; only the `Win32_*`
  features actually called; `unsafe` confined to COM/D3D/MF/OS wrappers; **never claim a HW
  path works until the machine says so.**

---

## 7. Quick command reference

```
$env:Path = "X:\cargo\bin;$env:Path"          # first, always (PowerShell)
export PATH="/x/cargo/bin:$PATH"              # first, always (Bash tool)
just check            # fmt + clippy -D warnings + cargo check
just test             # nextest, 197 tests (incl. smoke.rs loading the real exe)
just release          # stripped release vs 10 MB budget (8.30 MB with the UI stack)
just run buffer                               # tray shell → "Settings…" → live VU meters (A3)
just run -- buffer --record-secs 8            # headless auto-record self-test
just run -- record --seconds 15               # timed record (headless)
just run -- --check-config [PATH]             # print effective config (schema v2)
just verify clip.mp4                          # ffprobe assertion script

# A3 meter HW check (see §5): open Settings, play audio / speak, watch the two meters.
# Cold-open latency still logged per open:
Select-String cold_open_ms "$env:LOCALAPPDATA\clipd\logs\clipd.log.*"   # A2: ~385 ms first open

# Quality-tier spot check (A1): a High-tier clip ~24 Mbps @ 1080p60 vs Default's ~16.
# Set [encode] quality = "high" in %APPDATA%\clipd\config.toml, then:
just run -- record --seconds 15 --out c.mp4
ffprobe -v error -select_streams v:0 -show_entries stream=bit_rate <clip>
```
