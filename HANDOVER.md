# Session Handover — A2 (egui settings-window skeleton) DONE; A3 (VU meters) is next

> Onboarding note for the next session. `CLAUDE.md` and the `clipper-devpack/devpack/`
> docs are normative and override anything here. `02-AV-SYNC-SPEC.md` (frozen) overrides
> everything EXCEPT the dated `DECISIONS.md` amendments: the two M4-2 entries, the three
> **2026-07-07** entries (§2.2 process-loopback QPC, §2.5 track layout, §4 hybrid-moov),
> the **"T0 resolution"** entry (§6.1 CQP → bitrate-target VBR), the **"A1"** entry (config
> schema v2 / quality tiers / `toml_edit`), and now the **"A2"** entry (eframe/egui settings
> window / satellite thread / `winit` dep). Read **`M7-M8-PLAN.md`** (repo root) — it is the
> working plan for this whole phase; you are at Slice A task **A3**.

**Written:** 2026-07-07, after **A2 was implemented, self-reviewed, rust-reviewer'd, and
merged to `main`.** This session added the first UI-module code: the settings window
skeleton the meters (A3), status strip (A4), and editor (A5) hang off.

---

## 1. Code state

- **M0–M5 + T0 + A1 + A2 merged on `main`.** Working tree clean. **186 tests** (nextest;
  count unchanged from A1 — the window is GUI/thread code, covered by `tests/smoke.rs`
  loading the real exe, not new unit tests). `just check` (fmt + clippy -D warnings + check)
  green. Release build **8.28 MB** (8,681,984 bytes) vs the 10 MB budget — **+6.1 MB from
  A1's 2.57 MB, all from the eframe/egui/winit/glow UI stack.** ~1.3 MB headroom left.
- Last commits: `4349a42` Merge a2-settings-window → `339314e` the A2 feat commit.
- **`main` is 5 ahead of `origin/main`** (A1 feat+merge, the post-A1 handover doc, A2
  feat+merge). `origin/main` = `5ac1040`. **Not pushed** (orchestrator chose leave-local
  through A1/A2). Push when ready (`git push`; remote HTTPS `github.com/ImTani/clipd`, gh
  authed `ImTani`).

---

## 2. What A2 changed + the pain points (READ before touching the UI)

**`ui.rs` is now a directory module: `src/ui/{mod,tray,settings}.rs`** (matches
`capture/`, `encode/`, etc.; `lib.rs` `pub mod ui` and the `ui::Shell` public surface are
unchanged — git tracked `ui.rs → ui/tray.rs` as a rename). Full rationale: `DECISIONS.md`
"2026-07-07 — A2". The load-bearing facts:

- **The settings window is a SATELLITE on its own thread.** The tray "Settings…" item lazily
  spawns a `settings-ui` thread running `eframe::run_native`; the tray keeps the main-thread
  Win32 pump (per-thread message queues keep them apart). The window's ONLY engine coupling
  is a clone of `Sender<EngineCommand>` (held in `SettingsApp._cmd_tx`, **unused by A2** —
  wired for A5's editor / A6's rebinds). Direction is strictly `ui → engine`: `settings` is a
  PRIVATE submodule of `ui`; nothing in `engine` references it. **Keep it that way** — the
  engine must run fully if the window never opens (satellite law, `CLAUDE.md` "UI rules").
- **Reopen model (winit allows ONE event loop per process):** you CANNOT re-run `run_native`
  after a close. So the window's close (X) is intercepted in `SettingsApp::logic` →
  `CancelClose` + `Visible(false)` (hides); the tray re-shows it (`Visible(true)` + `Focus` +
  `request_repaint`) via an `egui::Context` clone the app publishes **synchronously from the
  `CreationContext`** into `Shared.ctx`. The thread lives until tray Quit.
- **Shutdown is a quit-flag + bounded join.** `SettingsHandle::shutdown` sets `Shared.quit`,
  sends `Close` + repaint, then joins within `SHUTDOWN_JOIN_TIMEOUT` (500 ms) and **detaches**
  on timeout (a window wedged in a native modal loop — mid drag/resize — must not stall
  process exit). `open()` also detects a dead UI thread (`is_finished()`, e.g. `run_native`
  failing on a VM/RDP) and disables Settings for the session with a logged reason — no respawn.
- **Cold-open < 300 ms (M7 acceptance) is a HARDWARE measurement.** Instrumented via a
  `cold_open_ms` field on the `settings window first frame` log event. NOT claimed from a
  build — see §5 for the on-Nitro procedure.

### Pain points I hit (so you don't re-derive them)

1. **eframe 0.35 has the REDESIGNED `App` trait — NOT the historical `update(&Context)`.** It
   is split: `fn logic(&mut self, ctx: &egui::Context, frame)` for non-drawing per-frame work
   (close-intercept, context publish live here) + `fn ui(&mut self, ui: &mut egui::Ui, frame)`
   for drawing. The handed `Ui` has **no margin/background** — wrap in
   `egui::Frame::central_panel(ui.style()).show(ui, |ui| …)`. Also `CentralPanel::show` now
   takes `&mut Ui` (old `show_inside` was renamed to `show`). **Any egui snippet you paste
   from pre-0.32 docs/LLM memory will be wrong — translate it against the pinned source.**
2. **The crate registry is under `C:\Users\tanis\.cargo\registry`, NOT `X:\cargo\registry`.**
   `CARGO_HOME=X:\cargo` holds `bin/`, but the source cache the compiler cites (and that you
   grep to read crate internals) is in the default user profile. This cost me two failed
   `find` passes — go straight to `C:\Users\tanis\.cargo\registry\src\index.crates.io-*`.
3. **eframe re-exports `egui`/`egui_glow`/`glow`/`egui_wgpu` but NOT `winit`.** To call
   `EventLoopBuilderExtWindows::with_any_thread(true)` (needed for the off-main-thread event
   loop) the platform ext trait must come from `winit` itself → **`winit = "=0.30.13"` is a
   NEW direct dep** (pinned to the exact winit eframe 0.35 resolves so cargo unifies to one
   winit and the trait applies to eframe's `EventLoopBuilder`). UI-module-only; documented.
4. **`eframe`/`egui` are CLAUDE.md-sanctioned for the UI module; `winit` is the newly-added
   one** — flagged in the task summary + DECISIONS (never bury a dep). eframe added
   `default-features = false, features = ["glow","default_fonts"]` (drops wgpu, Linux
   backends, accesskit, and eframe's persistence storage — config goes ONLY through A1's
   `Config::write_atomic`, never eframe storage).
5. **The first eframe build is SLOW** (~6 min release, compiling winit/glow/egui). Budget for
   it; the 2-minute default Bash timeout will kill a cold `cargo check` — run it backgrounded.
6. **Cross-thread `egui::Context` is sound** (reviewer-verified against egui 0.35 source):
   `send_viewport_cmd`/`request_repaint` queue into an internally-locked command buffer and
   never touch a winit `Window`/HWND from the calling thread — the owning thread drains them
   next frame. This is the intended way to drive eframe from the tray thread.

---

## 3. DO THIS NEXT — A3 (VU meters)

Full task text in `M7-M8-PLAN.md` §3. Order within Slice A = devpack priority (meters before
cosmetics), branch per task (`a3-vu-meters`).

- **A3 — VU meters for both current streams** (desktop-loopback + mic; grows to N tracks in
  Slice B). Highest-value UI element, ships before anything cosmetic.
- **The satellite-law design problem to solve first:** the meters live on the `settings-ui`
  thread; the audio levels are computed on the engine's audio threads. You need a
  **UI-readable level path that the engine PUBLISHES and the window READS — never the reverse.**
  Recommended shape: an `Arc<[AtomicU32]>` (or a small `Arc<struct>` of per-stream peak/RMS as
  bit-cast f32) that the audio-process threads write and the `SettingsHandle` gets a clone of
  to hand the `SettingsApp`. Lock-free, one-directional, satellite-clean. Do NOT route levels
  through `ShellSignal` (that channel is the tray's single consumer and is state-only).
  Compute the level where the PCM already is — `audio/resample.rs`'s 48 kHz chunks in
  `engine::audio_process_thread`, or the raw `AudioPacket` in `wasapi_stream` — pick the one
  that doesn't copy pixels/samples needlessly and keeps the math in a safe, unit-testable spot
  (the level→dB mapping is pure — unit-test it like the other logic modules).
- Then **A4** status strip · **A5** settings editor (writes via A1 `Config::write_atomic`;
  shows derived Mbps/RAM from `video_target_bitrate_bps × quality.multiplier()`) · **A6**
  press-to-bind hotkeys · **A7** recent-clips list · **A8** `just dist` beta zip.
- After A8: friends-beta v0 (2-track, full UI), then Slice B (B1–B7, 4-track audio), then M6
  closes on beta evidence.

`M7 acceptance` (from 08): cold-open < 300 ms; 2 h open-window soak, zero engine stalls
attributable to UI.

---

## 4. Research facts the next session must not re-derive (sourced in M7-M8-PLAN §5)

Carried forward — all still relevant for A3–A8 / Slice B:

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
| Crate **source cache** | `C:\Users\tanis\.cargo\registry\src\index.crates.io-*` (NOT `X:\cargo` — see A2 pain point 2; this is where you grep crate internals) |
| GPU | RTX 4050 Laptop (Ada NVENC) + Intel iGPU; Optimus. Primary **1080p** on the dGPU |
| Default audio | Realtek Headphones (render) + FIFINE mic (capture), both 48 kHz |
| ffmpeg/ffplay/ffprobe | 7.0.1 on PATH (ffplay is a **chocolatey shim** — see gotchas) |
| Config file | none by default — `%APPDATA%\clipd\config.toml` by hand. Hotkeys: save `Ctrl+Alt+S`, record `Ctrl+Alt+F9` |
| Log file | `%LOCALAPPDATA%\clipd\logs\clipd.log.<date>` |
| Git remote | `origin` HTTPS (`github.com/ImTani/clipd`), gh authed `ImTani`. `origin/main` = `5ac1040`; local `main` **5 ahead** (A1+A2) — push when ready |
| Zombie procs | `Get-Process clipd,ffplay -EA SilentlyContinue \| Stop-Process -Force` between runs |
| Local cruft (gitignored) | `ram.csv` (M5 RAM-budget log — delete if unneeded) |

### A2 HARDWARE TEST — still owed (I could not drive an interactive GUI headlessly)

Run these on the Nitro; the code only *builds + loads* clean (smoke test) so far:
1. `just run buffer` → tray → **Settings…** → window opens. Log shows
   `settings window first frame` with **`cold_open_ms` < 300** (M7 budget). Grep:
   `Select-String cold_open_ms "$env:LOCALAPPDATA\clipd\logs\clipd.log.*"`.
2. Close with **X** → hides (no error logged); **Settings…** again → `settings window re-shown`;
   repeat ~5× → NO panic (proves the one-event-loop-per-process reopen model).
3. Tray **Quit** with the window open → clean exit, log `settings window closed`, no hang.
4. **2 h open-window soak** (M7 acceptance): buffer + saves keep working with the window open;
   zero engine stalls attributable to the UI thread.

---

## 6. Gotchas carried forward (+ new A2 ones)

**New from A2** (details in §2 pain points):
- eframe 0.35 App trait = `logic()` + `ui()` (NOT `update()`); handed `Ui` has no bg — wrap in
  `egui::Frame::central_panel`. Crate source cache is under `C:\Users\tanis\.cargo`. `winit`
  is a NEW direct dep (=0.30.13) for `with_any_thread`. Settings window is a satellite on its
  own thread; keep `ui → engine` one-directional. Binary is now 8.28 MB (1.3 MB headroom).

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
  default-features off; `tests/smoke.rs` guards it. The A2 eframe window did NOT reintroduce
  this (smoke `version_loads_and_runs` passes with eframe linked). Themed classic-menu styling
  later = a manifest via build script, NOT the feature flag.
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
just test             # nextest, 186 tests (incl. smoke.rs loading the real exe)
just release          # stripped release vs 10 MB budget (8.28 MB with the UI stack)
just run buffer                               # tray shell (M5) → "Settings…" opens the A2 window
just run -- buffer --record-secs 8            # headless auto-record self-test
just run -- record --seconds 15               # timed record (headless)
just run -- --check-config [PATH]             # print effective config (schema v2)
just verify clip.mp4                          # ffprobe assertion script

# A2 settings-window HW check (see §5): open Settings from the tray, then
Select-String cold_open_ms "$env:LOCALAPPDATA\clipd\logs\clipd.log.*"   # expect < 300 ms

# Quality-tier spot check (A1): a High-tier clip ~24 Mbps @ 1080p60 vs Default's ~16.
# Set [encode] quality = "high" in %APPDATA%\clipd\config.toml, then:
just run -- record --seconds 15 --out c.mp4
ffprobe -v error -select_streams v:0 -show_entries stream=bit_rate <clip>
```
