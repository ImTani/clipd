# Milestone 4 Plan — window mode + timed recording

> Draft for orchestrator review. **No code written yet.** Normative sources:
> `01-PROJECT-PLAN.md §6 M4` (the gate), `02-AV-SYNC-SPEC.md §0` (epoch =
> resolution/target change; a clip must not span epochs), `§7` (device-loss /
> capture-change epoch restart, budget 2 s, buffer retained across the restart),
> `§1.2` (CFR grid rebased per epoch), pitfalls **8** (exclusive-FS + WGC),
> **9** (yellow border), **10** (cursor per target), **11** (resolution/display
> change mid-buffer), **19** (force IDR on record start), **31** (multi-monitor,
> choose target explicitly). All frozen — implement literally.
> `05-MILESTONE-TRACKER.md` M4 is the gate.

## 0. What M4 delivers

M3 made `clipd` a replay buffer: continuous capture of the **primary monitor** into
the ring, hotkey-save the last N seconds. M4 broadens *what* is captured and *how* a
recording is produced, without touching the frozen A/V-sync contract:

1. **Focused-window capture** (borderless/windowed), with a **monitor fallback** for
   exclusive-fullscreen titles WGC can't window-capture — documented, never silent.
2. **Window resize/close mid-buffer** handled by **segmenting at an epoch boundary**
   (`§0`), no crash, buffer stays alive.
3. **"Record next N minutes"** — a live **disk sink** driven off the same pipeline.
4. A **second hotkey** (`[hotkeys].record_toggle`) to start/stop timed recording.

**Exit criteria (tracker M4 — each closes only on a Nitro measurement):**
1. Capture focused window (borderless/windowed); monitor fallback for exclusive
   fullscreen, documented.
2. Window resize/close mid-buffer handled (segment cut, no crash).
3. "Record next N minutes" mode sharing the same pipeline with a disk sink.
4. Second hotkey set for timed record start/stop.

## 1. The substrate M4 builds on (what already exists — read before designing)

M4 is **more wiring than new invention**: most of the hard machinery landed in M1–M3
and is waiting to be connected.

| Already exists | Where | M4 uses it for |
|---|---|---|
| `PacingGrid::restart_epoch()` — bumps `epoch_id`, clears the grid, re-bases the CFR origin on the next frame (`§1.2`) | `capture/pacing.rs` | Every M4 epoch boundary (resize, close→fallback, device loss). |
| Ring is **epoch-agnostic** — holds packets across ≥1 epoch; eviction is whole-GOP, epoch-blind | `ring.rs` | The ring **survives** a resize/close/device-loss restart (`§7`: older epochs stay saveable until evicted). |
| `save::select_window` **already picks the newest epoch** and clamps if `target` precedes that epoch's first IDR (`§4.2`); has multi-epoch unit tests | `save.rs` | A save after a mid-buffer restart Just Works — picks the current epoch, single-epoch clip. |
| `record` **epoch loop** on device loss: finalize segment → rebuild → new segment file | `main.rs::run_record` + `RecordingEngine` | The pattern M4 folds into **buffer** mode (ring-persisting variant). |
| `EngineError::is_device_lost()` (DXGI `DEVICE_REMOVED/RESET`) | `engine.rs` | One of the three M4 restart triggers. |
| Config already carries `CaptureTarget::{FocusedWindow, Monitor(index), Primary}` and `[hotkeys].record_toggle` (default `Ctrl+Alt+R`) — **parsed, validated, unused** | `config.rs` | Wire straight through; no schema change (schema stays v1). |
| `HotkeyPump` (message-pump thread, `RegisterHotKey`) registers **one** hotkey | `hotkey.rs` | Extend to register the record-toggle as a second id. |
| WGC item setup (pool + free-threaded handler + keep-latest) | `capture/wgc.rs::start_primary` | Refactor to share with a new `start_window`. |

**What M4 must genuinely ADD (the four real gaps):**
- **A** — `CreateForWindow` capture + foreground-HWND resolution + target dispatch.
- **B** — a capture thread that detects **`ContentSize` change** (resize) and item
  **`Closed`** (close), and turns each into an epoch boundary instead of a fatal
  stage error (today pitfall 11 "ends the recording" — `engine.rs:428`).
- **C** — **buffer-mode epoch restart**: rebuild producers into a new epoch feeding
  the *same* ring, and give the **save worker a per-epoch output type** (SPS/PPS)
  so a save selects the type matching the window's `epoch_id`. This is the deferred
  `§7` item (HANDOVER §2c) — M4 does it because resize/close *require* it.
- **D** — the **timed-record disk sink** + force-IDR-on-start + the second hotkey.

## 2. Architecture — window capture + the epoch-restart spine

```text
                         ┌───────────── one producer set (unchanged §1/§2) ─────────────┐
 target dispatch ──▶ capture(WGC: monitor│window) ─▶ encode(H.264) ─┐
   (Primary │            │  watches ContentSize + Closed             │
    Monitor(i) │         │  → restart_epoch() on change              │  merged MuxItem
    Focused)             └── on hard restart: producers rebuilt ─────┤  (video + AAC)
                                into a NEW epoch, SAME ring           │
                                                                      ▼
                                              ┌──────────── RING THREAD ────────────┐
                                              │  Ring (epoch-agnostic, §3)           │
                                              │   ├─ (save hotkey) ─▶ SAVE WORKER ───┼─▶ clip.mp4
                                              │   │                    per-epoch type │
                                              │   └─ (record active) ─▶ DISK MUX ─────┼─▶ record.mp4
                                              └──────────────────────────────────────┘
```

- **One producer set** feeds the ring (the M3 spine). Window vs monitor is a capture
  *source* choice, invisible downstream.
- **Soft events stay in-thread; hard events restart the engine.** A `ContentSize`
  change needs a new pool + converter + encoder (new SPS/PPS) → that is a **hard
  restart** (rebuild producers, bump epoch). A transient occlusion/minimize is *not*
  an epoch change — the CFR grid's resubmit rule (`§1.2`) already covers "no new
  frame" without a rebuild.
- **The ring persists across restarts** (`§7`). The save worker keeps a
  `epoch_id → output_type` map so a clip from any retained epoch muxes with the
  right avcC. `select_window` already returns the window's `epoch_id`.
- **Timed record is a second sink** off the ring thread (D1, resolved — §4).

## 3. Task breakdown (branch per item, named after it)

### M4-1 `wgc.rs` — window & target capture  *(HW path; thin probe + checklist)*
**Spec/plan:** pitfalls 8, 9, 10, 31. **Adds gap A.**

- Extract the shared pool/session/handler setup into `start_for_item(gpu, item,
  cursor)`; keep `start_primary` and add `start_window(gpu, hwnd, cursor)` using
  `IGraphicsCaptureItemInterop::CreateForWindow` (the interop factory is already
  used for `CreateForMonitor`). Also add `start_monitor(gpu, index, cursor)` for
  `CaptureTarget::Monitor(i)` (pitfall 31 — the schema already has it; small).
- **Foreground-HWND resolution:** `foreground_window()` via `GetForegroundWindow`
  (`Win32_UI_WindowsAndMessaging`, feature already present from the M3 hotkey pump).
  Resolved once at start (captures whatever is foreground then); if none resolvable
  or the window is uncapturable, fall back to monitor + log. (No terminal-detection
  guard — a console app can't reliably identify its own terminal under ConPTY; the
  M5 tray removes the console. See DECISIONS 2026-07-05 M4-1.)
- **Target dispatch** in `main.rs`: replace the hard-coded `start_primary` with a
  `start_for_target(&cfg.capture.target, …)` used by both `record` and `buffer`.
- **Cursor per target (pitfall 10):** default cursor **on** for monitor/desktop,
  **off** for a focused game window. **Resolved (D4, §4):** keep the explicit
  `cursor: bool` for M4 (no schema change) and document the recommended value per
  target; the per-target *auto*-default (which needs an "unset"/`auto` state the v1
  schema lacks) lands with the M7 settings tri-state.
- **Border (pitfall 9):** `SetIsBorderRequired(false)` is already best-effort in
  `start_for_item`; nothing new, but the probe checklist must eyeball the border on
  this Win11 build.
- **Testing:** capture-source selection is a HW path → a `window-capture-probe
  [SECS]` diagnostic (mirrors `capture-probe`) + a checklist citing
  `04-TEST-MACHINE.md` (hybrid-graphics: verify which adapter WGC delivers a
  *window* texture on — may differ from the monitor case). Pure-logic bits (target
  parsing) are already tested in `config.rs`.

### M4-2 size-change + close → **buffer-mode epoch restart**  *(the core)*
**Spec:** `§0` (epoch on target/res change), `§7` (restart budget 2 s, buffer
retained), `§6.3` ("No WGC frame AND no resubmit possible > 1 s → epoch restart").
**Adds gaps B + C. Folds in the deferred `§7` device-loss restart for buffer mode.**

> **AMENDED 2026-07-05 (DECISIONS):** a window **resize** is NOT an epoch — it rescales
> into a **fixed canvas** (pitfall 11), so clips span resizes at one resolution
> (letterboxed). The epoch restart below applies only to the **cut path**: window
> **close → primary monitor** and **device loss**. The `ContentSize` detection +
> `ResizeTracker` are reused, but the resize *action* is an in-capture-thread pool +
> converter rebuild to the fixed canvas (same encoder/epoch), not a restart.

- **Detect (capture thread):** on each delivered frame compare
  `Direct3D11CaptureFrame::ContentSize` to the pool size; on change, the pool must
  be `Recreate`d — treat it as an epoch boundary. Subscribe to
  `GraphicsCaptureItem::Closed` for window close.
- **Restart mechanism — engine-level, ring-persisting.** Mirror `run_record`'s epoch
  loop but for the buffer engine: on a boundary, stop *only the producers*
  (capture/encode/audio), keep the ring + save worker + hotkeys alive, respawn
  producers into `epoch_id+1` feeding the same ring. Reuses the existing `spawn`
  helpers. (In-capture-thread converter/encoder rebuild was considered and rejected
  for M4: it forces per-epoch output-type plumbing through the encode thread mid-loop
  and re-implements teardown the engine already does — the engine-level restart
  reuses `RecordingEngine`'s proven pattern.)
- **Per-epoch output type in the save worker (gap C).** The encode thread already
  emits its negotiated output type once at start (`engine.rs:541`); on each rebuild
  it emits a new one. The save worker replaces its single `output_type` with a
  `Vec<(epoch_id, SendMediaType)>` (or map) and `save::save_clip` selects the entry
  matching `window.epoch_id`. **`select_window` already yields `epoch_id`** — this is
  the one missing link. Extract the selection as a pure helper
  (`output_type_for_epoch(&map, epoch) -> &MediaType`) so it is unit-testable per
  CLAUDE.md (logic modules 100% safe + tested).
- **Three triggers, one path:** window resize, window close→fallback, and DXGI
  device loss (`is_device_lost`, sleep/resume/TDR — pitfalls 25/26) all funnel into
  the same buffer restart. This closes the HANDOVER §2c "buffer-mode epoch restart"
  deferral **and** M1's long-open sleep/resume validation (HANDOVER §5) as a
  side effect.
- **Close policy (D2, resolved — §4):** on window `Closed`, **fall back to
  monitor capture** in the new epoch (a replay buffer that dies when you alt-F4 a
  game is the incumbent sin) + log + (M5) toast. Same path serves exclusive-FS
  fallback (pitfall 8) and is `§7`'s buffer-retained capture-target-change restart.
- **Tests:** pure-logic — `output_type_for_epoch` selection (present epoch, missing
  epoch → error, newest-of-many); extend `save.rs`'s existing multi-epoch tests to
  assert type-matches-epoch. The restart *timing* (≤ 2 s, `§7`) and "hour-N clip
  after a resize is perfect" are Nitro items (resize a borderless window mid-buffer,
  alt-F4 it, lid-close/resume — all "free tests" per `04-TEST-MACHINE.md`).

### M4-3 timed-record disk sink  *(shares the ring spine)*
**Spec/plan:** `01-PLAN §1` ("record next N minutes straight to disk"), `§6 M4`
("**sharing the same pipeline** with a disk sink"), pitfall 19 (force IDR on start),
`§0` (a recording must not span epochs → segment). **Adds gap D (sink half).
Shape RESOLVED against the devpack (D1 §4): tee off the ring.**

- **Shape (D1 = tee-off-ring, resolved):** the ring thread gains a
  `record_active` state and, while active, forwards each `MuxItem` to a **disk-mux
  channel** feeding the **existing `mux_thread`** (unchanged — it already writes
  crash-safe fragmented MP4 and finalizes on channel disconnect). Start = force an
  IDR so the file opens on a keyframe; stop / `N minutes` elapsed = drop the channel
  → `mux_thread` finalizes → atomic rename.
- **Force IDR (pitfall 19):** a control signal to the encode thread →
  `CODECAPI_AVEncVideoForceKeyFrame` via `ICodecAPI::SetValue`. **Needs HW
  verification** (the plan flags "verify your MFT honors" it; NVENC may reject some
  runtime props — same caveat as the deferred `auto_qp_relief` QP bump). Fallback if
  unsupported: start the file from the newest ring IDR (≤ 1 GOP / 2 s pre-roll,
  identical to the save contract) — which the ring already holds, so timed record
  even gets **instant pre-roll** for free. Recommend shipping the ring-IDR-start as
  the primary mechanism and treating runtime force-IDR as an optimization.
- **Segment on epoch (`§0`):** a timed recording crossing an M4-2 restart cuts at the
  boundary into `name.mp4`, `name-1.mp4`, … — the exact `segment_path` logic
  `run_record` already has; lift it.
- **Disk-mux setup:** the disk mux needs the output type + track ASCs, exactly like
  the save worker — reuse the per-epoch type map from M4-2.
- **Tests:** pure-logic — segment-path naming (already tested for record; assert the
  buffer path reuses it), record-active state transitions. The disk file
  correctness is `just verify` on the Nitro (same 8 checks; a timed clip must be as
  clean as a saved clip).

### M4-4 second hotkey + `main` wiring + docs  *(glue + the honest-limits doc)*
**Spec/plan:** `§6 M4` (second hotkey), pitfall 8 (document FS fallback),
`01-PLAN §6 M5` README limits (start the list now).

- **`HotkeyPump` → N hotkeys:** register both `save_clip` and `record_toggle`,
  return both ids. The ring thread's `select!` already matches on id; add the
  record-toggle arm → toggle `record_active` (M4-3).
- **`main.rs`:** `buffer` mode gains the record-toggle; a `--minutes N` flag (and/or
  the hotkey) bounds a timed recording. Keep `record --seconds` working (see D1
  note on `RecordingEngine`'s future).
- **Docs:** a short `LIMITATIONS.md` (or README section) — exclusive-FS →
  monitor-fallback, cursor default per target, one-clip-per-epoch (a clip can't span
  a resize) — seeding the M5 "honest limitations" deliverable while it's fresh.

## 4. Decisions — RESOLVED against the devpack (non-iterative contract)

Per CLAUDE.md ambiguity rules 1–2 (apply the spec/plan when they answer) and rule 3
(else simpler/more-logged/reversible), all four resolve from the devpack — none needs
an orchestrator question (rule 4: none is irreversible or crosses a hard constraint).
Each lands in `DECISIONS.md` when its task merges.

- **D1 — timed-record convergence → tee off the ring to the existing `mux_thread`.**
  *Resolved by:* `01-PLAN §6 M4` (*"'Record next N minutes' mode **sharing the same
  pipeline** with a disk sink"*) + `§2` (the ring/buffer-mux is one of the four
  permanent threads — the spine) + the already-logged **M3 decision #2** (*"M4
  converges timed-record onto the same ring spine"*). `§1`'s *"straight to disk"*
  describes the sink (continuous output vs. a held ring), not a bypass of the
  producers — the more specific §6 M4 governs. Reuses `mux_thread` wholesale (rule 3
  simpler/more-logged/reversible). Simultaneous buffer+record falls out nearly free,
  though M4 need not *require* concurrency. **Consequence:** `RecordingEngine` (the
  M1/M2 ring-less disk path) becomes redundant — **keep it working through M4**,
  retire it in a separate cleanup once the converged disk sink is HW-validated (don't
  delete validated code before its replacement is proven on the Nitro; staged,
  reversible). *Rejected:* (b) drive `RecordingEngine` by the hotkey (two disk paths;
  contradicts M3 decision #2); (c) a separate disk-sink engine (no `mux_thread` reuse).
- **D2 — window close / exclusive-FS → fall back to monitor in a new epoch + log.**
  *Resolved by:* pitfall 8 (*"for stubborn exclusive-FS titles, fall back to
  capturing the monitor. Say so in docs"*) + `§6 M4` (*"monitor fallback … 
  documented"*) + `§7` (a capture-target change is an epoch restart with the buffer
  **retained**). Keeping the buffer alive is the project's reason for existing (*"a
  dead thread with a live tray icon is the incumbent failure mode we exist to
  kill"*). *Rejected:* stop-cleanly (a dead buffer on alt-F4).
- **D3 — include `Monitor(index)` selection in M4-1.** *Resolved by:* pitfall 31
  (*"Choose capture target explicitly … `monitor = "primary" | index |
  "focused-window"`; never guess"*) — the schema already ships this; a small
  `start_monitor` honors the promise. Rule 3: implementing the config we already
  validate is simpler than leaving a dead option.
- **D4 — cursor: keep the explicit `cursor: bool` for M4; defer per-target
  auto-default to the M7 settings tri-state.** *Resolved by:* pitfall 10 (*"expose
  as config, default on for desktop, off for game window capture"*) — the "expose as
  config" core is met by the existing bool. The per-target *auto*-default wants an
  "unset"/`auto` state the v1 schema lacks; adding it now touches the versioned
  schema (pitfall 30 "never silently rewritten") for cosmetic gain, so M4 documents
  the recommended per-target value and the `auto` tri-state lands with M7 settings
  where it fits the model. Rule 3: simpler/reversible, no schema churn mid-milestone.
  *(Partial satisfaction of pitfall 10's default nuance — logged, not silent.)*

## 5. Test matrix (maps to exit criteria)

| Exit criterion | Covered by | Kind |
|---|---|---|
| 1. Focused-window capture + FS fallback | M4-1 `window-capture-probe` + checklist; `04-TEST-MACHINE.md` hybrid-adapter note | Nitro |
| 2. Resize/close mid-buffer (segment, no crash) | M4-2 pure-logic (`output_type_for_epoch`, save epoch-match) + Nitro (resize/alt-F4/lid-cycle, ≤ 2 s restart, hour-N clip clean) | mixed |
| 3. Record N minutes to disk | M4-3 pure-logic (segment naming, record-state) + `just verify` on a timed clip | mixed |
| 4. Second hotkey start/stop | M4-4 unit (two-hotkey parse/register) + manual toggle on the Nitro | mixed |

**Regression guard:** the full M3 `just verify` suite must stay green on
primary-monitor buffer saves — M4 must not perturb the frozen `§4` save path.

## 6. Suggested sequencing

1. **M4-1 window/target capture** — the visible new capability; independently
   HW-checkable (`window-capture-probe`) before any epoch work. (Nitro.)
2. **M4-2 epoch restart** — the core; unlocks resize/close *and* the deferred `§7`
   device-loss + M1 sleep/resume. Pure-logic parts CI-green; restart timing on Nitro.
3. **M4-3 timed-record disk sink** — depends on D1 + the M4-2 per-epoch type map.
   (Nitro: `just verify` a timed clip; segment across a resize.)
4. **M4-4 second hotkey + docs** — glue; last. (Nitro: toggle start/stop.)

M4-1 and the *pure-logic* half of M4-2 are "CI green suffices"; every task still ends
with a "run X on the Nitro, expect Y" block per CLAUDE.md task hygiene.

## 7. Deps & scope notes

- **No new crates anticipated.** Window capture (`CreateForWindow`,
  `GetForegroundWindow`) and force-IDR (`ICodecAPI`) are all in the already-present
  `windows` features (`Win32_UI_WindowsAndMessaging` from M3; `CreateForWindow`/
  `Recreate` are in the WGC/Direct3D11 features already used). Any *new* `Win32_*`
  feature gate is added in the same commit that calls it (devflow rule) and noted.
- **No tray icon** — that's M5 (scope ratchet). M4's surface stays the headless
  `buffer`/`record` subcommands + logs + the two hotkeys. Toasts are M5.
- **Frozen spec untouched.** M4 adds epoch *triggers* and a *sink*; it changes no
  constant, threshold, or rebasing rule in `02-AV-SYNC-SPEC.md`. Every new constant
  (restart budget already `§7`, force-IDR interval) cites its spec section in
  `spec_constants.rs` — no inline magic numbers.
- **Every decision D1–D4 lands in `DECISIONS.md`** when its task merges, and is
  called out at the top of that task's summary (never buried).
