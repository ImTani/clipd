# Session Handover — M5 COMPLETE (shell & trust), merged to `main`

> Onboarding note for the next session. `CLAUDE.md` and the
> `clipper-devpack/devpack/` docs are normative and override anything here.
> `02-AV-SYNC-SPEC.md` (frozen) overrides everything except the two dated
> `DECISIONS.md` M4-2 amendments. Read the **2026-07-06** `DECISIONS.md` entries
> (the M5 plan, the T2 dep/deny choices, and the **"M5 T2 fixup"**) plus
> `M5-PLAN.md` (repo root) for the whole M5 story. `LIMITATIONS.md` is the
> honest-limitations list (grown in M5).

**Written:** 2026-07-06, after **Milestone 5 was built, HW-validated on the Nitro V15,
and merged into `main`** (all `m5-*` branches, `--no-ff`). `clipd buffer` now runs a
**tray shell** — icon + menu (Save / Pause / Record / Open folder / Start-with-Windows /
Quit) — over the existing engine, with a **rotating file log**, a **watchdog→tray** hook,
and a **start-with-Windows** toggle. **M0–M5 are all on `main`.** Not yet tagged `m5`
(orchestrator's call).

**M5 exit criteria (`05-MILESTONE-TRACKER.md`) — closed on the Nitro:**

| Criterion | Status |
|---|---|
| Tray icon + states + minimal menu | ✅ HW: every menu item + both hotkeys worked; clean Quit; tray-saved clip verifies green |
| TOML config versioned / never rewritten / `--check-config` | ✅ logic+CI: M5 writes nothing to config; `--check-config` smoke-tested on the built binary |
| Rotating file log; every save logged with outcome | ✅ HW: `%LOCALAPPDATA%\clipd\logs\clipd.log.<date>` with per-save outcome lines |
| Watchdog: encoder stall → tray warning | **[~] PARTIAL** — plumbing HW-proven (Pause amber-flip) + logic unit-tested; the LIVE `§6.3`-divergence Warning needs GPU starvation → folded into the **M6 load matrix**. Not a blocker |
| Start-with-Windows (HKCU Run key, off by default) | ✅ HW: enable+disable both `Ok`; `reg query` confirms absent after disable |
| README honest limitations list | ✅ grew `LIMITATIONS.md` + un-staled README |

> **Tree is clean and green.** Root `clipd`: `just check` + `just test` = **171 tests**
> (13 new in M5; incl. `tests/smoke.rs` that LOADS the built binary), clippy `-D warnings`
> + fmt clean, `cargo-deny` green, 1 HW-gated test `#[ignore]`d. Release binary **2.49 MB**
> (budget 10 MB). New deps (all whitelisted): `tray-icon` (+`muda`), `tracing-appender`,
> dev-dep `assert_cmd`. New `windows` feature: `Win32_System_Registry`. `deny.toml` is now
> scoped to `x86_64-pc-windows-msvc` (DECISIONS "M5 T2").

---

## 1. Where things stand

M0 ✅ · M1 ✅ · M2 ✅ · M3 ✅ (tag `m3`) · M4 ✅ (tag `m4`) · **M5 ✅ merged**. The M3 24 h
soak remains a pre-1.0 acceptance item (not a blocker; ~12 h clean, +0.22 MB/h). MVP is M0–M6.

### The M5 architecture in one screen
- **Tray shell = the main thread (`ui.rs`).** In normal `buffer` mode the main thread builds
  the `TrayIcon` + menu and runs a non-blocking Win32 message pump (`PeekMessageW`), maps menu
  clicks to `EngineCommand`s, and reflects `ShellSignal::State(TrayState)` on the icon/tooltip.
  Solid-colour state icons (Buffering=green / Paused=amber / Warning=orange / Error=red) are
  built from RGBA behind a single swappable `icon_for(state)` seam (DECISIONS: swap for PNGs
  later in one function). Quit → `Shutdown` → clean `stop_and_join`.
- **Two engine seams (`engine.rs`).** `EngineCommand` (`SaveClip`/`ToggleRecord`/`SetPaused`/
  `Shutdown`) is read in the ring thread's `select!` **alongside** the hotkey receiver — the
  tray injects the SAME actions as the hotkeys. `ShellSignal` flows engine→shell. Hotkeys are
  unchanged. **Satellite rule holds:** the engine runs fully headless — `record`, `--autosave`,
  `--record-secs`, `--simulate-device-loss` build NO tray (they keep the Enter/timer loop);
  `buffer` falls back to the headless loop if the tray can't be created.
- **Pause = stop ingest, keep the buffer (`DECISIONS` "M5 plan").** Paused, the ring thread
  DROPS new packets (still counting them consumed so `§6.3` divergence stays honest) but
  RETAINS existing contents — a save while paused writes pre-pause footage. Pausing drains any
  active recording and refuses to start one (privacy). Capture/encode keep running (not a power
  toggle; true suspend is M10 `buffer_when`).
- **Watchdog→tray (`watchdog.rs`).** A pure hysteresis `Watchdog` over
  `PipelineStats::is_diverged()` (`§6.3` frames_in−frames_out > 120) emits UI-neutral
  `Ok/Warning` transitions (no engine↔watchdog import cycle); the ring thread polls it every
  500 ms and flips the tray, suppressed while paused. Dead worker → `any_worker_finished` →
  shell Error.
- **Rotating log (`logging.rs`).** `logging::init_session()` = console + daily-rolled
  non-blocking file in `%LOCALAPPDATA%\clipd\logs\`; its `WorkerGuard` is held in `main` for
  the `buffer`/`record` session. Probes stay console-only (`init_console`).
- **Autostart (`autostart.rs`).** The one permitted registry write: HKCU `…\Run` `clipd` =
  `"<exe>" buffer`. Pure `run_value` + thin `unsafe` Reg calls (SAFETY-noted). Off by default.

### M5 code map (all merged)
- `ui.rs` **(new)** — `Shell` (tray + menu + pump), `icon_for`/`state_color` (swappable icon
  seam), `menu_action` (pure id→action). `logging.rs` **(new)** — `log_dir` + `init_session`/
  `init_console`. `autostart.rs` **(new)** — `run_value` + `is_enabled`/`set_enabled`.
- `engine.rs` — `EngineCommand`/`ShellSignal`/`TrayState`, `BufferEngine::command_sender()`/
  `signals()`, `ingest_video`/`ingest_audio` (pause gating), `toggle_record` (shared), watchdog
  tick. `watchdog.rs` — `is_diverged` + `Watchdog`/`WatchdogState`.
- `main.rs` — `run_buffer` picks tray vs headless; `run_headless_session`. `tests/smoke.rs`
  **(new)** — spawns the built exe (`--version`/`--help`/`--check-config`) to catch load-time
  failures. `Cargo.toml`/`deny.toml` — see DECISIONS.

### The T2 fixup you must not re-introduce (`DECISIONS` "M5 T2 fixup")
The `tray-icon` `common-controls-v6` feature makes `muda` import **v6-only comctl32** functions
by name, which need an embedded app manifest we don't ship → the binary failed to LOAD
(`STATUS_ENTRYPOINT_NOT_FOUND` / `0xc0000139`). `cargo test` missed it (the unit-test harness
dead-strips the unused tray path). Fix: `tray-icon = { default-features = false }` (no
`common-controls-v6`) + `tests/smoke.rs` loads the real binary in CI. If you ever want themed
menus, add a manifest via a build script — do NOT just re-enable the feature.

## 2. DO THIS NEXT

M5 is merged. Options (none block each other):

### 2a. Milestone 6 — the hardware matrix (`05-MILESTONE-TRACKER.md` M6)
The 1.0 gate. **Needs external hardware** (AMD/AMF, Intel/QSV, Win10, 144/240 Hz, hybrid) — the
Nitro only covers the NVIDIA/Optimus row. Includes the **encoder-contention** (Discord
screenshare + buffer) and **100 %-GPU** rows — which is where the **T4 live watchdog→Warning**
gets exercised (fold it in there). Mostly an orchestrator/hardware effort, not agent code.

### 2b. Small M5/M4 follow-ups (agent-sized, no external HW)
- **T4 live-Warning smoke** — optional: a hidden `--simulate-stall N` hook (inflate `captured`
  or stall the mux) to flip the tray Warning on demand for a clean self-test, if you don't want
  to wait for M6. New test-only code; flag in DECISIONS.
- **avrig sync-straddle across a resize** (the M4-2 acceptance step still not run): `just rig
  flash` in a window, resize mid-flash, `just rig measure <clip>` — offset holds across the
  frame-pool recreation.
- **The M4-3 `Draining`→`Stop` tee/ctrl cross-channel race** — still open (within `§5` AV-3
  1-frame tolerance; not a blocker). A real fix drains `rec_item` before finalize.

### 2c. Deferred (unchanged)
- Unknown-key **config preservation on rewrite** → M7 settings pen. `auto_qp_relief` QP bump →
  needs on-HW tuning. Segment-on-epoch for a recording outliving a device loss (v1 stops it).
  M3 24 h soak → pre-1.0 acceptance alongside the M6 matrix.

## 3. Environment facts (this machine = the Nitro V15 test box)

| Thing | Value |
|---|---|
| Repo root | `X:\Projects_X\clipd` |
| Rust | stable **1.95.0**, `x86_64-pc-windows-msvc` (pinned) |
| `CARGO_HOME` | `X:\cargo` (`X:\cargo\bin` **not** on PATH by default — prepend `$env:Path = "X:\cargo\bin;$env:Path"`) |
| GPU | RTX 4050 Laptop (Ada NVENC) + Intel iGPU; Optimus. Primary 1080p on the dGPU |
| Default audio | Realtek Headphones (render) + FIFINE mic (capture), both 48 kHz |
| ffprobe/ffmpeg | **7.0.1** on PATH |
| Config file | none by default — create `%APPDATA%\clipd\config.toml` by hand. Default hotkeys: save `Ctrl+Alt+S`, record `Ctrl+Alt+F9` |
| Log file | `%LOCALAPPDATA%\clipd\logs\clipd.log.<date>` (daily-rolled; created on a `buffer`/`record` run) |
| Run key | `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` value `clipd` (autostart; off by default) |
| Git remote | `origin` HTTPS (`github.com/ImTani/clipd`), gh authed `ImTani`. **M5 merged to `main`; push when ready** |
| Zombie hotkeys | `taskkill /F /IM clipd.exe` between runs if a hotkey wedges |

## 4. Gotchas carried forward

- **Run `buffer` via `just run buffer`** (NOT `just run -- buffer` — the recipe already adds
  `--`, so the extra dashes reach clipd as an arg). In tray mode **Enter does not quit** — use
  the tray **Quit** item. New tray icons hide in the Win11 **"^" overflow flyout**.
- **`common-controls-v6` breaks the binary load** — see §1 fixup above. Keep `tray-icon`
  default-features off.
- **`--simulate-device-loss` runs headless (no tray)** by design (it's an unattended hook) and
  a device loss triggers an epoch rebuild, NOT a `§6.3` divergence — so it is not how you test
  the tray Warning.
- **`clip shorter than requested (§4.2)`** on a save is EXPECTED when the buffer is younger than
  the requested length (walk-back clamps to available footage) — not a bug.
- Carried from M1–M4: `Closed` doesn't fire on window close (Win11) → `IsWindow` poll; fixed
  canvas letterboxes odd-aspect windows; `windows` 0.62 COM interfaces `!Send`/`!Sync`; add only
  the `Win32_*` features actually called; `unsafe` confined to COM/D3D/MF/OS wrappers (now incl.
  `ui.rs` pump + `autostart.rs` reg); never claim a HW path "works" until the machine says so.

## 5. Quick command reference

```
$env:Path = "X:\cargo\bin;$env:Path"          # prepend cargo to PATH first
just check            # fmt + clippy -D warnings + cargo check   (171 tests source)
just test             # nextest, 171 tests (incl. tests/smoke.rs — loads the real exe)
just release          # stripped release + size vs 10 MB budget  (2.49 MB)
just run buffer                               # replay buffer WITH the tray shell (M5)
just run -- buffer --record-secs 8            # headless auto-record 8 s (no tray; self-test)
just run -- buffer --simulate-device-loss 5   # headless §7 device-loss epoch restart
just run -- record --seconds 15               # record straight to disk (headless)
just verify clip.mp4                          # ffprobe assertion script (8 checks)
# M5 checks:
Get-Content "$env:LOCALAPPDATA\clipd\logs\clipd.log.*" -Tail 30      # the rotating log
reg query "HKCU\Software\Microsoft\Windows\CurrentVersion\Run" /v clipd   # autostart state
```
