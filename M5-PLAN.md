# Milestone 5 Plan — shell & trust

> Draft for orchestrator review. **No code written yet.** Normative sources:
> `01-PROJECT-PLAN.md §6 M5` (the gate) and `§2` (the "main thread ─ tray icon,
> hotkey events, config, watchdog UI" architecture), `§3` pitfalls **28**
> (AV false-positive), **29** (privacy defaults / pause / clear indicator),
> **30** (config integrity: versioned, never silently rewritten, `--check-config`),
> `02-AV-SYNC-SPEC.md §6.3` (watchdog thresholds — frozen constants, already in
> `spec_constants::watchdog`), `06-SAFETY-AND-VMS.md` (the HKCU Run key is the one
> permitted registry write), `07-DEVFLOW.md` (branch-per-item, `just check`/`just
> test` green, new `windows` features only in the commit that calls them),
> `08-FEATURE-COMPLETE.md` (the M7 line M5 must NOT cross). `05-MILESTONE-TRACKER.md`
> M5 is the gate. Two behavioral decisions locked in `DECISIONS.md` (2026-07-06,
> "M5 plan"): tray Pause semantics and programmatic state icons.

## 0. What M5 delivers

M0–M4 built the engine: continuous capture (monitor or focused window) → hardware
encode → compressed ring → hotkey-save the last N seconds, plus record-to-disk. It is
driven from a console (`buffer` blocks on stdin-Enter to quit). M5 gives it a **shell**
and the **trust surface** a background daemon needs — without adding a settings UI (that
is M7). Six gate items:

1. **Tray icon with states + a minimal menu** — Save clip · Pause · Record N min ·
   Open folder · Quit. Icon reflects Buffering / Paused / Warning / Error.
2. **TOML config: versioned, never silently rewritten, `--check-config`** — largely
   already met (M0–M4); M5 closes it out and adds a documented atomic-write helper for
   the future M7 pen.
3. **Rotating file log; every save attempt logged with outcome** — a real on-disk log
   so "why didn't my clip save" is answerable.
4. **Watchdog: encoder stall / starvation → tray warning** — wire the `§6.3` thresholds
   (already constants) to a tray state, not just a `warn!`.
5. **Start-with-Windows (HKCU Run key, off by default)** — the one permitted registry
   write, toggled from the tray.
6. **README: honest limitations list** — exclusive-FS fallback, DRM black frames, HDR
   tone-map, hotkeys swallowed in some exclusive-FS games, letterbox, paused-still-uses-GPU.

**Exit criteria (tracker M5 — each closes only on a Nitro measurement):**
1. Tray icon with states + minimal menu (Save clip, Pause, Record N min, Open folder, Quit).
2. TOML config, versioned, never silently rewritten, `--check-config`.
3. Rotating file log; every save attempt logged with outcome.
4. Watchdog: encoder stall / starvation detection → tray warning.
5. Start-with-Windows (registry Run key, off by default).
6. README: honest limitations list.

## 1. The substrate M5 builds on (what already exists — read before designing)

Like M4, M5 is **more wiring than new invention**. The engine, config schema, watchdog
thresholds, and hotkey pump all exist; M5 connects them to a user-facing shell.

| Already exists | Where | M5 uses it for |
|---|---|---|
| `BufferEngine` supervisor (start / `stats` / `any_worker_finished` / `stop_and_join`) | `engine.rs` | The shell drives it; `any_worker_finished()` → tray Error. |
| Ring thread `select!` over `item_rx` + `hotkey_rx` + autosave/record ticks | `engine.rs::ring_thread` | Add an `EngineCommand` arm (tray → engine) beside `hotkey_rx`. |
| `GlobalHotKeyEvent::receiver()` read inside the ring thread; `HotkeyPump` registers via a message pump | `engine.rs`, `hotkey.rs` | Hotkeys keep working unchanged; the tray injects the *same actions* through the command channel. |
| `§6.3` thresholds: `FRAMES_DIVERGENCE_MAX`, `ENCODER_QUEUE_DEPTH_MAX`, `SAVE_DURATION_WARN_MS`, `NO_WGC_FRAME_RESTART_MS`, `NO_AUDIO_EVENT_REBUILD_MS` | `spec_constants::watchdog` | The watchdog state machine's edge numbers (no magic numbers inline). |
| `PipelineStats::check_divergence()` (currently `warn!`-only) | `watchdog.rs` | Extend to emit a `ShellSignal` state, not just a log line. |
| `Config` (versioned, validated, `to_toml`), `default_config_path`, `--check-config` | `config.rs`, `main.rs` | Item 2 is mostly done; add the atomic-write helper + startup-invalid toast. |
| `init_tracing` (console + `EnvFilter`) | `main.rs` | Add a `tracing-appender` rolling file layer + a lifetime `WorkerGuard`. |
| `record_path` / save-outcome logging in the mux + save workers | `engine.rs`, `save.rs` | Every save attempt already logs; audit the failure branch for outcome coverage. |

## 2. The architectural spine — main-thread message pump + two channels

**The linchpin.** `tray-icon` (and `global-hotkey`) require a Win32 message loop on the
thread that owns the tray, and deliver menu clicks via a global `muda::MenuEvent::receiver()`.
`01-PROJECT-PLAN.md §2` already assigns this to the main thread: *"main thread ─ tray icon,
hotkey events, config, watchdog UI."*

In **buffer mode only**, the main thread becomes the **shell** (`src/ui.rs`): it registers
the hotkeys, creates the tray + menu, and runs a pump loop that

- pumps Win32 messages (drives `WM_HOTKEY` for `global-hotkey` and the tray's hidden window),
- drains `MenuEvent::receiver()` → maps each click to an `EngineCommand`,
- reads a `ShellSignal` receiver from the engine → swaps the tray icon + tooltip,
- exits on the **Quit** item (replacing today's stdin-Enter quit).

Two new seams in `engine.rs`, both plain `crossbeam` channels (no async — constraint 3):

```
EngineCommand  (shell → ring thread, new select! arm):
    SaveClip            // same path as the save hotkey
    ToggleRecord        // same path as the record hotkey
    SetPaused(bool)     // NEW capability (see §4 T3)
    Shutdown            // clean stop from the Quit menu item

ShellSignal    (engine → shell, drives the tray):
    State(TrayState)    // Buffering | Paused | Warning | Error
    Saved(PathBuf)      // optional toast
    SaveFailed(String)  // optional toast
```

Hotkeys are untouched: the ring thread keeps reading `hotkey_rx`; the tray merely *adds* a
parallel command path to the identical actions. This preserves the **satellite rule**
(`08-FEATURE-COMPLETE.md` M7, applied early): the engine still runs fully headless — the
`record` subcommand, `--autosave`, and `--record-secs` paths get **no** shell and are not
restructured. `ui` depends on engine types; the engine never depends on `ui`.

COM/apartment note: the shell thread (main) pumps messages; the engine is all-MTA on its
worker threads. Tray/menu COM and the pump's `unsafe` stay confined to `ui.rs` with
`// SAFETY:` notes — no `unsafe` leaks into the pure logic modules (CLAUDE.md).

## 3. New dependencies, features, budget

- **`tray-icon`** — on the CLAUDE.md rule-2 whitelist (UI-adjacent). Pulls **`muda`**
  (menus) transitively — allowed as a transitive dep; noted in DECISIONS + task summary.
- **`tracing-appender`** — on the whitelist. Rolling daily file + non-blocking `WorkerGuard`.
- **New `windows` feature `Win32_System_Registry`** — added in the start-with-Windows commit
  that calls `RegSetValueExW`/`RegDeleteValueW` (devflow: features only for APIs used). The
  exe path comes from `std::env::current_exe()` (no `Win32_UI_Shell` needed). The log dir
  resolves from `%LOCALAPPDATA%` via env var (same pattern as `default_config_path` on
  `%APPDATA%`) — no new feature for logging.
- **Budget (constraint 7, measured not assumed):** release is **2.05 MB / 10 MB**.
  `tray-icon` + `muda` + `tracing-appender` will grow it; the new `just release` size is
  reported in the task summary. Comfortable headroom expected.

## 4. Task breakdown (branch per item — `07-DEVFLOW.md §3`)

Ordered by dependency. Each task ends with the mandatory "run X on the test machine,
expect Y" block (pure-logic tasks: "no hardware step; CI green suffices").

### T1 — `m5-log-rotation` (independent; land first)
Rework `init_tracing` to add a `tracing_appender::rolling::daily` file layer writing to
`%LOCALAPPDATA%\clipd\logs\` (non-blocking; the `WorkerGuard` is returned to `main` and held
for process lifetime). Console layer stays. Audit `save.rs` + the mux worker so **every save
attempt logs an outcome** (success + failure branches). A log-dir path builder mirrors
`default_config_path`'s env-var fallback.
- **Tests (pure):** log-dir path builder (env set / unset). No hardware path.
- **Run:** `just check && just test` green; after a short `just run -- buffer`, confirm a
  dated log file exists under `%LOCALAPPDATA%\clipd\logs\` with the save lines. *Expect: file
  present, one structured line per save attempt with its outcome.*

### T2 — `m5-tray-shell` (the spine; unblocks T3/T4)
New `src/ui.rs`: a `Shell` owning the `TrayIcon`, the menu (Save clip · Pause [checkable] ·
Record N min [reflects record state] · Open folder · Quit), and the pump loop. Add the
`EngineCommand` arm to `ring_thread`'s `select!` and the `ShellSignal` sender out of the
engine; `BufferEngine::start` grows a command sender + signal receiver (via `BufferParams` or
its return). `run_buffer` replaces the sleep/Enter loop with `Shell::run()`. **Icons:** a
single `icon_for(TrayState)` builds a solid-colour RGBA disc per state — isolated so a future
swap to `include_bytes!` PNGs is a one-function change (DECISIONS 2026-07-06). "Open folder" →
`std::process::Command` on `explorer` with the output dir (no injection, no new feature).
- **Tests (pure):** menu-id → `EngineCommand` mapping; `ShellSignal` → `TrayState` mapping.
  Pump/COM stays thin (`unsafe` confined, SAFETY-noted).
- **Run:** `just run -- buffer`; click every menu item — Save writes a clip, Record toggles,
  Open folder opens the dir, Quit exits cleanly; confirm the save + record **hotkeys still
  fire** alongside the tray. *Expect: each action works from both tray and hotkey; clean exit;
  `just verify` green on a tray-saved clip.*

### T3 — `m5-pause` (reuses T2's command seam)
Add `SetPaused(bool)` handling to the ring thread. **Pause = stop ingesting new packets
(dropped at the tee point) while retaining existing ring contents and keeping capture/encode
running; any in-progress timed recording is stopped; a save still works on the retained
footage.** Unpause resumes ingest (the buffer carries a gap across the paused span). Tray →
Paused; menu item checked. (DECISIONS 2026-07-06 "M5 plan" — reverses the initial clear+refuse
recommendation per orchestrator; true zero-GPU suspend is deferred to M10 `buffer_when`.)
- **Tests (pure):** ring-thread ingest gating (paused drops, retains contents, resumes);
  save-while-paused still selects a window from retained footage.
- **Run:** `just run -- buffer`; Pause, wait, save → clip holds pre-pause footage (gap across
  the pause); Unpause → new footage resumes. *Expect: no clear, no crash; icon flips; a
  save while paused succeeds on retained footage; `just verify` green.*

### T4 — `m5-watchdog-tray` (reuses T2's signal seam)
Give the watchdog a small state machine over the `§6.3` signals: `FRAMES_DIVERGENCE_MAX`
(>120 → Warning), `SAVE_DURATION_WARN_MS` (>1000 → Warning + the existing WARN), encoder-open
failure (§15 → Warning + one retry), dead worker via `any_worker_finished()` → Error (latched).
Emit `ShellSignal::State` on enter/exit (debounced, with recover→Buffering transitions). Time
the save at the save worker if not already timed.
- **Tests (pure):** state machine with the spec edge numbers (enter/exit Warning on threshold
  cross; Error latch on a dead thread; recover clears Warning).
- **Run:** `just run -- buffer --simulate-device-loss 5` → tray flips to Warning during the
  ~2 s rebuild, back to Buffering after; force a dead worker → Error state + log line.
  *Expect: tray state tracks the watchdog; buffer survives the simulated loss (M4 behavior).*

### T5 — `m5-start-with-windows` (independent; small)
New `autostart.rs` (or a `ui.rs` submodule): read/write `HKCU\Software\Microsoft\Windows\
CurrentVersion\Run` value `clipd` = `"<current_exe>" buffer`; off by default; a checkable tray
item toggles it and reflects current state on startup. `Win32_System_Registry` feature added
in **this** commit. The value-string builder (`exe path → Run value`) is pure; the
`Reg*ValueW` calls are thin `unsafe` with SAFETY notes. This is the **only** registry write in
the whole project (constraint 5 / safety doc).
- **Tests (pure):** the Run-value command-string builder (quoting, arg).
- **Run:** toggle on → `reg query "HKCU\...\Run" /v clipd` shows the value and clipd launches
  at next logon; toggle off → value removed. *Expect: value appears/disappears; no other
  registry keys touched.*

### T6 — `m5-docs-limitations` (independent; doc-only)
Grow `LIMITATIONS.md` + `README.md` with the honest list: exclusive-fullscreen (monitor
fallback), DRM black frames, HDR tone-mapped-to-SDR, hotkeys swallowed by some exclusive-FS
games, aspect-mismatch letterbox, **paused-still-uses-GPU**, and the pointer "why didn't my
clip save → the rotating log at `%LOCALAPPDATA%\clipd\logs\`" (ties T1 to the trust model).
Restate the `§1` non-goals in the README (they are load-bearing per the plan).
- **Run:** no hardware step; docs review only.

## 5. Sequencing

```
T1 (log)        ─┐
T5 (Run key)    ─┼─ independent; land anytime
T6 (docs)       ─┘
T2 (tray shell + EngineCommand/ShellSignal seams)   ← the spine
   ├─ T3 (pause)            — reuses T2's command seam
   └─ T4 (watchdog → tray)  — reuses T2's signal seam
```

Recommended order: **T1** (the log everything writes into) → **T2** (unblocks T3/T4) →
**T3**, **T4**, with **T5**/**T6** slotted in parallel. Tag `m5` when all six close on the
Nitro. Per devflow, an item closes only on a machine measurement, never an agent claim.

## 6. Risks / call-outs

- **Message-pump + hotkey/tray coexistence.** Both rely on a pumping main thread; both use
  global receivers, so the ring thread keeps reading hotkeys unchanged while the tray only
  *adds* a command path. Low regression risk to M3/M4 hotkeys — but it is on the HW checklist
  for T2 (hotkeys must still fire with the tray live).
- **Satellite discipline.** `record`, `--autosave`, `--record-secs`, and every `*-probe`
  keep their current console loops — no shell. The engine must stay fully functional headless.
- **Binary size (constraint 7).** Measured via `just release` in the T2/T5 summaries; three
  new crates in the tree, budget 10 MB (currently 2.05 MB).
- **Config item is nearly closed.** "Never silently rewritten" holds because M5 does not write
  `config.toml` at all (start-with-Windows uses the registry, not config). Unknown-key
  preservation-on-rewrite remains deferred to M7's settings pen (as config.rs already states).
- **AV false-positive surface (pitfall 28).** A tray daemon with global hotkeys that records
  the screen pattern-matches a RAT; M5 keeps to RegisterHotKey (no hooks) and one HKCU Run key
  — signed releases + Defender submission are M10 release-engineering, noted in `LIMITATIONS.md`.

## 7. Locked decisions (see DECISIONS.md 2026-07-06 "M5 plan")

- **Pause** stops ingesting new footage, keeps the buffer active (retained) and the pipeline
  running, stops any in-progress recording, and still allows a save of retained footage.
  Reversible; true zero-GPU suspend is M10.
- **Tray icons** are generated programmatically (solid colour per state) behind a single
  swappable `icon_for(state)` seam, so switching to real image assets later is trivial.
