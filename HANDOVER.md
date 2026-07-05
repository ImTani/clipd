# Session Handover — M4 COMPLETE (window mode + timed recording), merged to `main` (tag `m4`)

> Onboarding note for the next session. `CLAUDE.md` and the
> `clipper-devpack/devpack/` docs are normative and override anything here.
> `02-AV-SYNC-SPEC.md` (frozen) overrides everything **except** the two dated
> `DECISIONS.md` **M4-2 amendments** (2026-07-05), which reinterpret `§0`'s
> "resolution change → epoch" for the window case (see §0 note below) with explicit
> orchestrator approval. `DECISIONS.md` is the append-only rationale log — read the
> 2026-07-05 entries (M4-1 → M4-4, the two fixed-canvas amendments, and the timed-record
> edge fixes) for the whole M4 story. `M4-PLAN.md` (repo root) is the M4 design + the
> D1–D4 resolutions (with the two amendment notes inline). `LIMITATIONS.md` is the
> honest-limitations list started in M4 (feeds the M5 README).

**Written:** 2026-07-05, after **Milestone 4 was built, HW-validated / self-verified, and
merged into `main`** (`--no-ff`, tagged **`m4`**). `clipd buffer` now captures a **focused
window** (or monitor) into the ring with a **fixed output canvas**, saves the last N s, and
**records to disk** on a second hotkey. **M0–M4 are all on `main`.** The M4 branch
`m4-window-capture` is merged and can be parked/deleted.

**M4 exit criteria (`05-MILESTONE-TRACKER.md`) — all closed on the Nitro:**

| Criterion | Status |
|---|---|
| Focused-window capture + monitor fallback, documented | ✅ HW: window capture (odd sizes via even canvas), cross-monitor, exclusive-FS/close→primary fallback |
| Window resize mid-buffer → FIXED CANVAS (letterboxed, no epoch) | ✅ HW: resize grow/shrink/aspect + monitor drag rescale into the canvas; a clip spans them, `just verify` green, one resolution |
| Capture-target handled, no crash: close → monitor (SPANS), device-loss cut | ✅ HW: closing the window keeps the buffer alive on the monitor and a save retains the pre-close window footage; device-loss restart via `--simulate-device-loss` |
| "Record next N minutes" disk sink (tee off the ring, D1) | ✅ self-verified: `--record-secs 8` → an 8 s 1920×1080 recording passes all 8 `just verify` checks (§4-clean edges) |
| Second hotkey (record_toggle) start/stop | ✅ self-verified via `--record-secs`; manual press HW-validated (default now `Ctrl+Alt+F9` — a letter combo `Ctrl+Alt+R` was taken on the Nitro) |

> **Tree is clean and green.** Root `clipd`: `just check` + `just test` = **149 tests**,
> clippy `-D warnings` + fmt clean; 1 HW-gated test `#[ignore]`d (`convert::odd_input_
> scales_into_fixed_canvas`, run with `--ignored` on the machine). Release binary
> **2.05 MB** (budget 10 MB). **No new deps, no new `windows` features** in all of M4.
>
> **Final strict devpack review (this session) passed:** no dependency/feature changes;
> `unsafe` confined to `wgc.rs`/`convert.rs` (COM/OS wrappers) with `SAFETY:` notes; pure
> modules (`canvas`, `resize`, ring, save, config, pacing) 100 % safe + unit-tested;
> `record --seconds` re-verified (no M1/M2 regression from the shared capture thread);
> the §6.3 no-frame threshold now derives from `spec_constants::watchdog::
> NO_WGC_FRAME_RESTART_MS`; `[encode].max_height` gained a range test.

---

## 1. Where things stand

M0 ✅ · M1 ✅ · M2 ✅ · M3 ✅ (tag `m3`) · **M4 ✅ merged (tag `m4`)**. Only the M3 24 h soak
remains open, and it is **reclassified as a pre-1.0 acceptance item, NOT a blocker**
(DECISIONS 2026-07-05; ~12 h clean, +0.22 MB/h). MVP is M0–M6.

### The M4 architecture in one screen

- **Fixed output canvas (the M4-2 amendment — the key idea).** A window is captured at
  its native (changing) size and **rescaled-to-fit, centered, letterboxed** into a fixed
  canvas by the video processor (`capture/convert.rs`, `capture/canvas.rs`). The canvas =
  the capture monitor's resolution capped at `[encode].max_height` (default 2160), evened.
  Because the **encoded resolution never changes**, a window **resize** or **close→monitor**
  is NOT a `§0` epoch — the clip spans it at one resolution. Only a **device loss** (encoder
  rebuild, unavoidable) starts a new epoch. This fixed the replay-buffer UX (resizing/closing
  no longer truncates a save to since-the-event).
- **Capture-thread triggers (`engine.rs::capture_thread`, buffer mode only).** All handled
  **in-thread** on the fixed canvas: a settled **resize** (`capture/resize.rs::ResizeTracker`
  debounces WGC's per-frame ContentSize flood) → recreate pool + rebuild converter; a window
  **close** (`wgc::is_window` poll — WGC's `Closed` event does NOT fire on Win11, confirmed by
  probe) or exclusive-FS **no-frame** (§6.3) → switch source to the primary monitor. Record
  mode passes `triggers_enabled=false` (a size change ends the segment, pitfall 11).
- **Supervisor + persistent core (M4-2 §7, from M3-adjacent work).** `BufferEngine` is a
  supervisor over a persistent ring thread + **mux worker**, and a rebuildable producer set;
  a device loss rebuilds producers (+ the D3D device) into a new epoch feeding the SAME ring.
  The mux worker holds an output type **per epoch** (`§4.2` — a save picks the type matching
  the clip's epoch).
- **Timed recording (M4-3, tee off the ring per D1).** The ring thread tees each `MuxItem`
  (cheap `Arc` clone, `try_send` so a slow disk stops the recording rather than stalling the
  buffer) to the mux worker, which drives a live `Fmp4Writer`. **§4-clean edges:** head —
  buffer audio while `Pending` and replay it into the writer on the first IDR (its prebuffer
  aligns it ≤ 1 AAC frame); tail — the ring thread `Draining`s at stop, teeing only audio
  until it reaches the last video PTS. Recordings pass all 8 `just verify` checks.
- **Two hotkeys (M4-4).** `HotkeyPump::spawn(&[save_clip, record_toggle])`; the ring thread's
  `select!` dispatches by id. **Registration is tolerant** — a combo owned by another app
  warns instead of killing buffer mode.

### M4 code map (all merged)
- `capture/canvas.rs` **(new, pure, 7 tests)** — `canvas_size` + `letterbox_rect` geometry.
- `capture/resize.rs` **(new, pure, 6 tests)** — `ResizeTracker` (debounce the ContentSize flood).
- `capture/convert.rs` — `Converter::new(input, canvas, fps)`: VP scales input → fixed canvas
  with letterbox (dest rect + black background). `recreate_pool` support via `wgc`.
- `capture/wgc.rs` — `CaptureSource` (Primary/Monitor(i)/FocusedWindow/Window(hwnd)),
  `start`/`start_for_item`, `recreate_pool`, `is_window`, `window_monitor_size`, `content_size`,
  the `Closed` flag (best-effort). `window-capture-probe`/`window-events-probe` diagnostics.
- `engine.rs` — the whole buffer supervisor, capture-thread triggers, mux worker (saves +
  recording), ring thread (tee + drain), `RecordCtrl`, per-epoch types. `main.rs` — target
  dispatch, `--record-secs`/`--simulate-device-loss` hooks, record hotkey wiring.
- `config.rs` — `[encode].max_height` (canvas ceiling). `hotkey.rs` — N hotkeys, tolerant.
- `spec_constants.rs` — `DEFAULT_MAX_ENCODE_HEIGHT` + range.

### `§0` interpretation note (the one frozen-spec reinterpretation — orchestrator-approved)
`§0` says a *resolution change* starts an epoch and a clip must not span epochs. M4 keeps the
**encoded/output** resolution fixed (the canvas), so a window resize/close is not a `§0`
resolution change → no epoch → clips span it. This is documented as the `§0` interpretation
in the two dated M4-2 amendments in `DECISIONS.md`, approved twice by the orchestrator after
the epoch-per-event UX was rejected. Device loss (encoder rebuild) remains a genuine epoch.

## 2. DO THIS NEXT

M4 is merged. Pick either — neither blocks the other.

### 2a. One small M4 confirmation left (HW; not a blocker)
- **avrig sync-straddle across a resize** (the orchestrator's M4-2 acceptance step not yet
  run): `just rig flash` in a window, resize mid-flash, `just rig measure <clip>` — the
  click/flash offset should hold across the frame-pool recreation (the `§1.2` resubmit rule
  covers the brief gap). Proves audio/grid sync rides through a resize.
  - (The **record-hotkey manual press is DONE** — default changed to `Ctrl+Alt+F9` because
    `Ctrl+Alt+R` was taken on the Nitro; validated start→stop→`just verify` green, then pushed.)

### 2b. Start Milestone 5 — shell & trust (`05-MILESTONE-TRACKER.md` M5)
Tray icon + states + minimal menu (Save clip / Pause / Record N min / Open folder / Quit) —
`tray-icon` joins the build here (whitelisted); the settings window (egui) is **M7**, not M5.
Versioned TOML never silently rewritten + `--check-config` (config.rs is ready; the rewrite
path is the new work). Rotating file log. Watchdog → tray warnings (wire the `§6.3` thresholds
already in `spec_constants::watchdog` — queue depth, divergence, save-duration — to tray state).
Start-with-Windows (HKCU Run key, off by default). The honest README (grow `LIMITATIONS.md`).

### 2c. Deferred follow-ups (flagged; do NOT start without an explicit ask)
- **Retire `RecordingEngine`.** The M1/M2 ring-less disk path is now fully redundant with the
  buffer engine + M4-3 disk sink; fold `record --seconds` onto the converged path and delete
  it (the record hotkey is now orchestrator-validated). Reversible cleanup.
- **Segment-on-epoch for a recording that outlives a device loss** (v1 stops it — device loss
  is rare); **force-IDR-on-start** (not needed — drop-until-first-IDR gives a clean open within
  ≤ 1 GOP). Both flagged in DECISIONS "M4-3".
- **`auto_qp_relief` QP bump (`§6.2`)** — still deferred (needs live-encoder QP on-HW tuning).
- **Mic-startup head-silence on early saves** — a clip whose window includes the first ~1 s of
  a fresh buffer can have up to ~60 ms mic head-silence (DECISIONS "M4-1 HW-run finding"); fix
  = synthesize leading silence at save time. Pre-existing, not M4.
- **M3 24 h soak** — reclassified pre-1.0 acceptance (run against a release-candidate binary
  alongside the M6 matrix). `--autosave N` + Private-Bytes/HandleCount sampler.

## 3. Environment facts (this machine = the Nitro V15 test box)

| Thing | Value |
|---|---|
| Repo root | `X:\Projects_X\clipd` |
| Rust | stable **1.95.0**, `x86_64-pc-windows-msvc` (pinned) |
| `CARGO_HOME` | `X:\cargo` (`X:\cargo\bin` **not** on the agent shell PATH — prepend: `$env:Path = "X:\cargo\bin;$env:Path"`) |
| GPU | RTX 4050 Laptop (Ada NVENC) + Intel iGPU; Optimus. Primary 1080p on the dGPU |
| Default audio | Realtek Headphones (render) + FIFINE mic (capture), both 48 kHz |
| ffprobe/ffmpeg | **7.0.1** on PATH (ffmpeg 7 uses `pts_time`, not `pkt_pts_time`) |
| Config file | none by default — `clipd` never writes one; create `%APPDATA%\clipd\config.toml` by hand (e.g. to set `target = "focused-window"` or `[encode].max_height`). Default hotkeys: save `Ctrl+Alt+S`, record `Ctrl+Alt+F9` |
| Git remote | `origin` HTTPS (`github.com/ImTani/clipd`), gh authed `ImTani`. **M4 (tag `m4`) + the `Ctrl+Alt+F9` fix are merged to `main` and PUSHED** |
| Zombie hotkeys | a killed-by-`timeout` `clipd.exe` can linger holding a hotkey — `taskkill //F //IM clipd.exe` between test runs |

## 4. Gotchas carried forward (M1–M4)

- **`Closed` does NOT fire on window close (Win11)** — use `IsWindow` polling for close
  detection (`is_window`); `is_closed()` is a best-effort secondary (monitor removal).
- **During an active resize-drag the vacated canvas shows stale pixels** (WGC composites into
  the not-yet-recreated pool); self-cleans on the settle (~0.4 s). Cosmetic, documented.
- **Fixed canvas letterboxes** — a window of different aspect than its monitor's canvas gains
  black bars; never stretched (`LIMITATIONS.md`).
- **The mux worker owns BOTH saves and the live recording** — a long save briefly muxing can
  queue teed record items (256-deep `rec_item`); a sustained backlog stops the recording
  (protects the buffer), not the buffer.
- Binding from earlier: `windows` 0.62 COM interfaces `!Send`/`!Sync` (per-type `unsafe impl
  Send` + SAFETY); add ONLY the `Win32_*` features for APIs actually called (M4 added none);
  `unsafe` confined to COM/D3D/MF/OS wrapper modules; pure logic 100 % safe + unit-tested;
  never claim a HW path "works" — claim it "builds and is ready for procedure X".

## 5. Quick command reference

```
$env:Path = "X:\cargo\bin;$env:Path"   # prepend cargo to PATH first
just check           # fmt + clippy -D warnings + cargo check   (149 tests source)
just test            # nextest, 149 tests                       (root clipd)
just release         # stripped release + size vs 10 MB budget  (2.05 MB)
just run -- buffer                        # replay buffer: save + record hotkeys (M4)
just run -- buffer --record-secs 8        # auto-record 8 s to disk (self-test hook)
just run -- buffer --simulate-device-loss 5   # test the §7 device-loss epoch restart
just run -- window-capture-probe 8        # capture the FOCUSED WINDOW, report frames+size
just run -- window-events-probe 30        # log resize (ContentSize) + close events
just run -- record --seconds 15           # M1/M2 dumb recorder (still works)
just verify clip.mp4                      # ffprobe assertion script (8 checks)
just rig flash --seconds 35 / just rig measure clip.mp4   # §5 sync rig
cargo test --lib --ignored                # the HW-gated odd-input→canvas VP test
```

## Handoff strict-review outcome (this session)
Ran M4 against the devpack, thorough + strict, before merge. **Clean.** One noted item: the
`§0` fixed-canvas reinterpretation (above) — the sole divergence from the frozen spec's letter,
explicitly orchestrator-approved and documented. Two small strictness fixes applied (`max_height`
test; §6.3 constant sourced from `spec_constants`). `record` mode re-verified (no regression).
