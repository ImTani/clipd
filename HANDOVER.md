# Session Handover — A6 (press-to-bind hotkeys) DONE (local-green, HW pending); A7 (recent clips) is next

> Onboarding note for the next session. `CLAUDE.md` and the `clipper-devpack/devpack/`
> docs are normative and override anything here. `02-AV-SYNC-SPEC.md` (frozen) overrides
> everything EXCEPT the dated `DECISIONS.md` amendments: the two M4-2 entries, the three
> **2026-07-07** entries (§2.2 process-loopback QPC, §2.5 track layout, §4 hybrid-moov),
> the **"T0 resolution"** entry (§6.1 CQP → bitrate-target VBR), the **"A1"** entry (config
> schema v2 / quality tiers / `toml_edit`), the **"A2"** entry (eframe/egui settings window /
> satellite thread / `winit` dep), the **"A3"** entry (lock-free `AudioLevels` / VU-meter seam),
> the **"A4"** entry (lock-free `EngineStatus` / status-strip seam), the **"A5"** entry (settings
> editor / `clear_after_save` hot-swap / mic-picker scope), and now the **"A6"** entry (press-to-bind
> hotkeys / restart-note / UI-side validation). Read **`M7-M8-PLAN.md`** (repo root) — it is the
> working plan for this whole phase; you are at Slice A task **A7**.

**Written:** 2026-07-07, after **A6 was implemented, self-reviewed, rust-reviewer'd, and merged to
`main` (local-green; HW checklist owed — see §5).** This session added press-to-bind rebinding for the
save-clip and record-toggle hotkeys in the settings editor: press a combo → captured → validated →
written via `Config::write_atomic`, restart-noted (re-registered at startup).

---

## 1. Code state

- **M0–M5 + T0 + A1 + A2 + A3 + A4 + A5 + A6 merged on `main`.** Working tree clean. **220 tests**
  (nextest; +4 from A5's 216 — all in `ui/settings.rs`: `key_to_code`, `accelerator_from` incl.
  rejection cases, `validate_hotkeys`, hotkeys-in-restart-fields). `just check` (fmt + clippy -D
  warnings + check) green. Release build **8.78 MB** (9,204,736 bytes) vs the 10 MB budget —
  **+5 KB from A5's 8.77 MB** (pure logic + a few widgets). ~1.22 MB headroom left.
- **A6 is LOCAL-GREEN + rust-reviewer'd, NOT yet HW-validated.** Press-to-bind writes `[hotkeys]`
  via `Config::write_atomic`, restart-noted; validated UI-side (parse + self-conflict on parsed
  `HotKey`s). HW checklist (Rebind a hotkey, save, restart, confirm it fires) is owed — see §5. A3's
  meters remain HW-verified.
- Last commits: `7ccd61f` Merge a6-hotkey-bind → `13884da` the A6 feat commit (+ this doc
  commit on `main`).
- **`main` is ahead of `origin/main`** (A1–A6 feat+merge + handover/DECISIONS docs).
  `origin/main` = `5ac1040`. **Not pushed** (orchestrator chose leave-local through Slice A).
  Push when ready (`git push`; remote HTTPS `github.com/ImTani/clipd`, gh authed `ImTani`).
- **Still owed (M7 acceptance, not task-specific):** the **2 h open-window soak** — zero engine
  stalls attributable to the UI thread. Not yet run; do it during a longer session before M6
  sign-off.

---

## 2. The engine→UI publish seams + the editor write path (READ before touching UI/config)

### A5 — settings editor (newest; `src/ui/settings.rs`)

The first UI→engine WRITE path (A3/A4 were read-only). Full rationale: `DECISIONS.md`
"2026-07-07 — A5". Load-bearing facts:

- **Config is written ONLY through `Config::write_atomic`** (the single representation, same typed
  path as `--check-config`). The editor holds a draft `Config` the widgets edit in place; Save does
  `mic.to_cfg()` → `validate()` (surfaces the exact `ConfigError` string, writes nothing on failure)
  → `write_atomic()`. It loads the current config on window open (missing/invalid → defaults, never
  silently overwritten).
- **Apply model = hot-swap the one safe field, restart-note the rest.** `clear_after_save`
  hot-applies via the **new `EngineCommand::SetClearAfterSave(bool)`** (the ring thread's `cfg` is
  now `mut`; it is the only editable field with no pipeline side effect — single consumer, no race).
  Everything else (quality/resolution/fps/buffer/output/desktop/mic) needs an epoch/encoder rebuild,
  so Save lists exactly which changed fields need a restart. **`EngineCommand` lost `Copy`** (now
  `Clone`) to allow owned payloads; all sends/matches are by value, so nothing relied on `Copy`.
- **Mic picker = policy dropdown {Default (follow) | Off} + an advanced pinned-id text field, NOT a
  full device list.** `audio/devices.rs` has no enumeration API; adding WASAPI `EnumAudioEndpoints` +
  friendly-name reads is new confined-unsafe COM only verifiable on HW (deferred fast-follow —
  DECISIONS "A5"). Desktop loopback follows the default render endpoint → plain on/off.
- **Derived feedback uses the SAME spec fns the engine uses.** Mbps = `video_target_bitrate_bps` at
  the selected res tier (native ≈ 1080p); RAM = `byte_cap_bytes` at nominal 1080p over
  `buffer_seconds + one GOP` — mirrors the engine's actual byte cap, so it doesn't under-report.
- **The editor lives entirely on the settings-window thread**; it never blocks the engine (satellite
  law). File I/O (`load` on open, `write_atomic` on Save) is user-initiated + infrequent.
- **A6 press-to-bind hotkeys** ride the same editor: a "Rebind" button captures the next combo
  (`accelerator_from`/`key_to_code` → `parse_hotkey`-validated; Ctrl-or-Alt required), written to
  `[hotkeys]`, restart-noted (re-registered at startup — no live re-registration; the pump lives in
  main.rs on its own thread, a cross-thread control channel is the deferred fast-follow). Hotkey
  validation is UI-side only (parse + self-conflict on parsed `HotKey`s) — NOT in `Config::validate`,
  because that would make `load(..).unwrap_or_default()` silently discard a whole config on one bad
  hotkey (DECISIONS "A6").

### A4 — status strip (`src/status.rs`)

**New pure-logic module `src/status.rs`** — the status-publishing type + the derived-display math.
Full rationale: `DECISIONS.md` "2026-07-07 — A4". The load-bearing facts:

- **Same shape as A3, a second lock-free `Arc<EngineStatus>`: engine PUBLISHES → UI READS.** An
  immutable header (GPU adapter `Arc<str>`, fps, configured buffer seconds — set in
  `BufferEngine::start`) + per-field `Relaxed` atomics for the live cells. The UI takes one decoded
  `snapshot()` per frame. It is NOT `ShellSignal` (the tray's state-only channel). `status.rs`
  references nothing under `ui`; direction stays `ui → engine`.
- **One `Arc` fans out to THREE engine threads.** Ring thread → state (each transition) + buffer
  fill + stage counts (on the 500 ms watchdog tick). Capture thread → resolution + capture target
  (canvas init & window→monitor fall-back) + dropped frames. Mux worker → last-save outcome. The
  supervisor publishes `Error` on fatal teardown. Created before `gpu` moves into the supervisor;
  survives §7 epoch rebuilds (each respawned capture thread gets a fresh clone).
- **Dropped frames accumulate as a DELTA (`add_dropped`/fetch_add), never a `store`.** A fresh
  `PacingGrid` per epoch restarts its drop count at 0; storing the absolute would erase prior
  epochs' drops on a device-loss respawn. Each capture thread forwards only its own increment into
  the shared session total (rust-reviewer caught the original `set_dropped` doc-vs-behavior bug).
  `captured`/`encoded`/`muxed` reuse the existing `Arc`-atomic `PipelineStats`.
- **Codec = hardwired "H.264"; "vendor" = the GPU adapter description** (not the MFT friendly name
  — reading it is COM plumbing for a cosmetic string, deferred). **Last-save time = a Unix-ms stamp
  formatted RELATIVE to now by the UI** ("12 s ago" — pure `format_elapsed`; no `chrono`). A
  skipped save (young buffer) publishes `Failed`, never a stale success.
- **The panel rides A3's visibility-gated 30 fps repaint** — a hidden window still idles at zero
  CPU. Derived mappings (`ticks_to_seconds`/`bytes_to_mib`/`fill_fraction`/`format_elapsed`) are
  pure + unit-tested. When Slice B widens the audio/track set, the status fields grow the same way.

### A3 — VU meters (`src/audio/levels.rs`)

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

## 3. DO THIS NEXT — A7 (recent clips list)

Full task text in `M7-M8-PLAN.md` §3. Order within Slice A = devpack priority (meters → status →
editor → hotkeys → recent clips). Branch per task (`a7-recent-clips`).

- **A7 — recent clips list** in the settings window: the last ~20 saved clips, each with **open /
  open folder / copy path**. NO editor, NO thumbnails-with-scrubbing (explicit non-goals, keep it
  lean). Just a scannable list the user can act on.
- **Seam notes:** the clip files land in the output dir (`config.output.dir`, or the OS Videos
  default, resolved as in main.rs `run_buffer`). Simplest source of truth = scan that directory for
  the product's `*.mp4` files, newest-first (by mtime), take 20 — no new persisted state, no engine
  coupling. "Open" = `explorer <file>` / the shell open verb; "Open folder" already exists on the
  tray (`open_folder` in tray.rs — reuse the pattern); "Copy path" = egui clipboard
  (`ui.output_mut(|o| o.copied_text = …)` or `ctx.copy_text`). The directory scan + newest-20 +
  filename→display mapping is pure — unit-test it like the A5 estimates. The list can refresh on
  window open (and maybe a manual "Refresh" button); it does NOT need to live-watch the FS.
- Then **A8** `just dist` beta zip + one-page quick-start (incl. SmartScreen note) + default-config
  template. After A8: **friends-beta v0** (2-track, full UI), then Slice B.
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

### A6 HARDWARE TEST — OWED (do at the next HW batch; `just run buffer`, release)

- [ ] Settings → **Hotkeys** section shows the two current bindings + a **Rebind** button each.
- [ ] Click **Rebind** for Save clip → "press a combo…" → press e.g. `Ctrl+Alt+K` → the row shows
      `Ctrl+Alt+KeyK`. **Esc** during capture cancels (binding unchanged).
- [ ] Try to bind the SAME combo to both → **Save** shows "save-clip and record hotkeys must
      differ" and writes nothing. Bind a bare key (no Ctrl/Alt) → capture ignores it.
- [ ] **Save** with new distinct bindings → `[hotkeys]` in `config.toml` updates; result says
      "Restart clipd to apply: …, hotkeys". **Restart** → the new combo fires the save/record; the
      old one no longer does.
- [ ] (Conflict) Bind a combo another app owns → on restart the log warns "could not register
      hotkey (already in use…)" and that hotkey simply doesn't fire (buffer keeps running).

### A5 HARDWARE TEST — OWED (do at the next HW batch; `just run buffer`, release)

- [ ] Tray **Settings…** → a **Settings** section shows quality/resolution/fps/buffer/output/
      clear-after-save/desktop-audio/mic controls, seeded from the current `config.toml`.
- [ ] Change quality/resolution + move the buffer slider → the "≈ N Mbps video · buffer ≈ N s / X
      MiB RAM" line updates live and looks sane (Default 1080p60 ≈ 16 Mbps).
- [ ] **Save settings** → `%APPDATA%\clipd\config.toml` is written (check it; comments/unknown keys
      preserved), and the result line reads "Saved. Restart clipd to apply: …" listing the changed
      restart fields.
- [ ] Toggle **Clear buffer after save** + Save → applies live (no restart): the next save clears
      (or keeps) the ring accordingly; the log shows `clear-after-save updated (live)`.
- [ ] Set mic to **Off** + Save, restart → the mic meter/track disappears; set back to **Default
      (follow)** → returns. (Full device enumeration is a deferred fast-follow, see DECISIONS "A5".)
- [ ] Make an invalid edit (e.g. mic "Specific device id…" left empty) + Save → the exact
      `--check-config` error shows in red and **nothing is written**.

### A4 HARDWARE TEST — OWED (do at the next HW batch; `just run buffer`, release)

- [ ] Tray **Settings…** → the window shows a **Status** section above Audio levels.
- [ ] **State** line tracks reality: green "buffering"; tray **Pause** → amber "paused" → resume →
      "buffering". Force a §6.3 divergence (heavy scene) → "warning" if it trips.
- [ ] **Capture** line shows target (Monitor/Window) · WxH · fps · H.264, and **Encoder GPU** shows
      the RTX 4050 (or the selected adapter). Window source → capture that window → shows "Window";
      close it → falls back to "Monitor" live (no epoch).
- [ ] **Buffer** line climbs to ~configured seconds as the ring fills; the bar tracks it; MiB is
      plausible for the tier.
- [ ] **Frames** counters climb (captured ≈ encoded ≈ muxed); **dropped** stays low and only ever
      increases (never resets after a `--simulate-device-loss` epoch rebuild — the delta fix).
- [ ] Save a clip → **Last save: OK … (N ms)** with a relative time that ages ("just now" → "N s
      ago"). A save on a too-young buffer shows "failed".
- [ ] Panel animates only while the window is visible; close-to-tray → reopen resumes cleanly (rides
      A3's visibility gate — no hidden-window spin).

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

**New from A6:**
- **Hotkey validation is UI-side only** (`Editor::validate_hotkeys`), deliberately NOT in
  `Config::validate` — folding it in would make `Config::load(..).unwrap_or_default()` silently
  discard a whole user config on one bad `[hotkeys]` value. Compare hotkeys as PARSED `HotKey`s, not
  strings. Press-to-bind requires Ctrl or Alt (no bare-key global hotkeys). Re-registration is
  restart-only; live re-register/conflict-detection is the flagged fast-follow (needs a pump-control
  channel — the `HotkeyPump` is in main.rs on its own thread).

**New from A5:**
- **The editor is the only place UI writes config — always via `Config::write_atomic`.** Never add
  a second TOML writer or mutate config any other way (CLAUDE.md "UI rules"). Validate first; surface
  `ConfigError`'s `Display` text; write nothing on failure.
- **`EngineCommand` is no longer `Copy`** (now `Clone`) — a live-apply command may carry an owned
  payload. `SetClearAfterSave` is the ONLY live-apply field so far; classify any new editable field
  as hot-swap (single-consumer, side-effect-free) vs restart-note, and log it (DECISIONS "A5" has the
  rubric).
- **Mic picker is policy-only (Default-follow / Off) + a pinned-id text field** — no device
  enumeration yet. A full enumerated picker needs a WASAPI `EnumAudioEndpoints` wrapper (confined
  unsafe COM) + HW validation; it's a flagged fast-follow, not a regression.

**New from A4:**
- **Two engine→UI publish `Arc`s now exist and must stay the same shape** — `AudioLevels` (A3) and
  `EngineStatus` (A4). Any new UI read-data seam publishes to a lock-free `Arc`, UI reads a clone;
  never the reverse. (The A5 editor is the WRITE exception — it goes through `Config::write_atomic`.)
- **Dropped-frame count is a per-thread DELTA into a shared total, not a `store`** (`add_dropped`).
  A fresh `PacingGrid` per epoch restarts at 0 — storing the absolute erases prior epochs' drops.
  If you add any other cumulative-across-epochs counter, accumulate deltas the same way (or reuse
  the `Arc<AtomicU64>` `PipelineStats` pattern that is created once and survives rebuilds).
- **Status `snapshot()` clones the adapter as an `Arc<str>`** (cheap pointer clone, not a String)
  since the UI reads it every frame. The immutable header (adapter/fps/buffer-seconds) is set at
  `BufferEngine::start` and read without atomics.

**New from A3:**
- **Level path is `Arc<AudioLevels>` (atomics), NOT `ShellSignal`.** Keep any new UI-data seam the
  same shape: engine publishes to a lock-free `Arc`, UI reads a clone; never the reverse. Publish
  silence/zeroed state when a producer stops so the UI decays instead of lying.
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
