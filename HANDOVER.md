# Session Handover — Slice A COMPLETE + HW-VALIDATED; `main` PUSHED to origin; **Slice B PLANNED** (`SLICE-B-PLAN.md`, D1/D2 locked 2026-07-08); NEXT = start coding at **B1**

> **2026-07-08 planning session (no code):** wrote **`SLICE-B-PLAN.md`** (repo root) — the
> working plan for Slice B (B1–B7 + B3.5, 4-track audio), grounded in a full read of the
> code + specs. Two decisions locked into `DECISIONS.md` ("2026-07-08 — Slice B planning"):
> **D1** `separate_tracks` semantics change + default flip (`false`=mix+mic default,
> `true`=full 5-track; default clip becomes {mix,mic}) and **D2** B1 track-1 = pass-through,
> real sum in B4. **`main` is now PUSHED to `origin`** (Slice A un-defered). The 2 h
> open-window UI soak (M7 acceptance) and the A6-fast-follow standalone HW test are still
> owed and now fold into the **B7** Nitro cycle. **Next session: begin at B1** — read
> `SLICE-B-PLAN.md` first (it supersedes `M7-M8-PLAN.md §4` for the Slice-B task detail).

> Onboarding note for the next session. `CLAUDE.md` and the `clipper-devpack/devpack/`
> docs are normative and override anything here. `02-AV-SYNC-SPEC.md` (frozen) overrides
> everything EXCEPT the dated `DECISIONS.md` amendments: the two M4-2 entries, the three
> **2026-07-07** entries (§2.2 process-loopback QPC, §2.5 track layout, §4 hybrid-moov),
> the **"T0 resolution"** entry (§6.1 CQP → bitrate-target VBR), the **"A1"** entry (config
> schema v2 / quality tiers / `toml_edit`), the **"A2"** entry (eframe/egui settings window /
> satellite thread / `winit` dep), the **"A3"** entry (lock-free `AudioLevels` / VU-meter seam),
> the **"A4"**–**"A7"** entries, and now the **"A8"** entry (`just dist` friends-beta zip / commented
> config template + drift test / quick-start). Read **`M7-M8-PLAN.md`** (repo root) — it is the working
> plan for this whole phase. **Slice A (A1–A8) is DONE**; next is the batched HW validation of A4–A8
> (§5) → friends-beta v0 → **Slice B** (B1–B7, 4-track audio).

**Written:** 2026-07-07, after **A8 was implemented, self-reviewed, and merged to `main` (local-green;
`just dist` verified end-to-end).** This session added friends-beta packaging: a `just dist` recipe
that zips the stripped release exe + a one-page quick-start + a commented config template (drift-guarded
by a test). **This COMPLETES Slice A** — the full customizable UI (settings editor, status strip, VU
meters, hotkey rebind, recent clips) + a shippable zip.

---

## 1. Code state

- **M0–M5 + T0 + A1–A8 merged on `main` — Slice A COMPLETE.** Working tree clean. **232 tests**
  (nextest; +4 from the 2026-07-08 batched-HW fast-follows — `a5-ff-output-dir` resolve/validate +3,
  `a6-ff-cross-conflict` +1; on top of the earlier A6 live-conflict work). `just check` (fmt + clippy
  -D warnings + check) green. Release build ~**8.81 MB**
  vs the 10 MB budget — effectively unchanged (the fast-follow adds a small control channel + a few
  widgets). `just dist` → `target/dist/clipd-v<ver>.zip` (~3.85 MB compressed), verified end-to-end.
- **A6 fast-follow landed 2026-07-08 (local-green; HW validation is a STANDALONE gate — see §5 "A6
  FAST-FOLLOW HARDWARE TEST"):** live "combo already taken" detection in the settings editor, plus two
  same-day first-run UI fixes — bindings show the human token (`Ctrl+Alt+K`, not `Ctrl+Alt+KeyK`) and
  the binding is an editable text field (so combos another app owns, which press-to-bind can't capture,
  can be typed and get the live "taken" warning). DECISIONS "2026-07-08 — A6 fast-follow"; §2/§5/§6
  updated. The item closes only after the standalone Nitro test passes.
- **The mic-device-dropdown fast-follow (A5) is NOT done here — folded into Slice B as B3.5** (§3 /
  M7-M8-PLAN §4), where the WASAPI endpoint-enumeration COM wrapper rides B2/B7's audio-COM HW cycle.
- **A4–A8 are LOCAL-GREEN + (A4–A7) rust-reviewer'd, NOT yet HW-validated.** The whole settings-window
  UI + `just dist` are owed one batched HW pass — see §5 (five per-task checklists, A4→A8). A2/A3 are
  already HW-verified.
- Last commits: `01622e2` Merge a8-dist → `8574c74` the A8 feat commit (+ this doc commit on
  `main`).
- **`main` is PUSHED to `origin/main`** (2026-07-08 — Slice A un-defered; remote HTTPS
  `github.com/ImTani/clipd`, gh authed `ImTani`). The Slice-B planning docs
  (`SLICE-B-PLAN.md`, DECISIONS + this handover) are uncommitted working-tree changes at
  the time of writing — commit + push them before starting B1.
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
  `[hotkeys]`, restart-noted (re-registered at startup — the working binding is still applied on
  restart). Hotkey validation is UI-side only (parse + self-conflict on parsed `HotKey`s) — NOT in
  `Config::validate`, because that would make `load(..).unwrap_or_default()` silently discard a whole
  config on one bad hotkey (DECISIONS "A6").
- **A6 fast-follow — live "combo already taken" detection (DONE 2026-07-08).** The A6-flagged
  cross-thread pump-control channel now exists: `HotkeyPump` (main.rs) exposes a cloneable
  `HotkeyControl` (threaded main → `Shell` → `SettingsHandle::open` → `Editor`). On each rebind the
  editor asks the pump to **test-register** the candidate on the manager's own thread (woken by a
  `WM_APP` `PostThreadMessageW`); a free combo → `✓ available`, an OS-owned combo → `⚠ in use by
  another app`, our own current combo → `✓ available`. Non-blocking (fire-on-bind, `try_recv` per
  frame). **Each hotkey row is now an editable monospace `TextEdit`** (+ the Rebind press-to-bind
  button): a combo another app owns is swallowed by the OS and never reaches egui, so press-to-bind
  can't catch it — the user *types* it and the same probe reports it taken. Bindings store/show the
  **human token** (`Ctrl+Alt+K`, not `Ctrl+Alt+KeyK`; `key_to_token`) — parses to the identical
  `HotKey`, matches the shipped defaults. **Deferred to the post-Slice-B UI pass (decide then, not
  owed before):** live *re-registration* of the working hotkey (needs an `EngineCommand` to swap the
  ring thread's frozen `save`/`record` ids live) + its dependent "re-default record_toggle on
  persistent conflict" (DECISIONS "2026-07-08 — A6 fast-follow"; M7-M8-PLAN §7).
- **A7 recent-clips list** (`src/ui/recent.rs`) scans the engine's resolved `output_dir` (threaded
  from the tray, NOT re-derived from config) for `clipd_*.mp4` files, newest 20, files-only; Open /
  Folder-reveal / Copy-path shell out to Explorer. Re-scans on each re-show via a `Shared.rescan_recent`
  flag the tray sets (the window persists hidden, so a once-at-open scan would go stale). Filter/sort
  is pure + tested.

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

## 3. DO THIS NEXT — start Slice B at B1 (read `SLICE-B-PLAN.md` first)

**Slice B is now PLANNED** — `SLICE-B-PLAN.md` (repo root) is the working plan and
**supersedes `M7-M8-PLAN.md §4`** for Slice-B task detail (D1/D2 locked in DECISIONS
2026-07-08). Key facts the plan establishes so the next session doesn't re-derive them:
the audio pipeline (`ring`/`save`/`mux`/epoch loop/thread-spawn) is **already N-track
generic** (driven by `num_audio`/positional `track_index`) — the "knows there are two"
edits are narrow (the `AudioStreamKind` enum, `enabled_audio_kinds`, one `match kind`,
`main.rs:555`, `levels.rs` asserts). The real work is a **sources ≠ tracks** split (Mix
is a derived sum; Other-system's source switches at runtime; Game/VC are conditional) +
four genuinely-new pieces: process-loopback capture (B2), the mixer (B4), game/VC binding
(B3), and the **hybrid-`moov`-finalize (B5) which is NOT yet implemented** (`finish()` only
flushes fragments today). Also relax the ASC-complete save gate (`v.len() == num_audio`,
`engine.rs:1956,1908`) for conditional/late tracks. **Start at B1** (enum/track-model
generalization, CI-green winnable, no HW). The still-owed **2 h UI soak** and the
**A6-fast-follow standalone HW test** fold into the **B7** Nitro cycle.

---

### (Historical) Slice A close-out — batched HW validation (A4–A8) → friends-beta v0

**Slice A (A1–A8) is code-complete, local-green, and HW-VALIDATED on the Nitro 2026-07-08** (§5 for
per-task results): **A4 ✅ · A7 ✅ · A2/A3 ✅** (earlier). **A5 and A6 each surfaced one defect — both
FIXED same-day** as merged fast-follows (`a5-ff-output-dir`, `a6-ff-cross-conflict`; DECISIONS
2026-07-08) **and re-validated on the Nitro** (A5: bad path → red error, blank → `…\Videos\clipd`, good
path created; A6: cross-row combo → red "⚠ same as …", not green ✓). **A8 dist deferred** to
post-Slice-B + UI pass (orchestrator). What remains before Slice B:

1. **The still-owed 2 h open-window soak** (M7 acceptance: zero engine stalls attributable to the UI
   thread) — the ONE remaining Slice-A HW item. It doesn't block starting Slice B coding; run it during
   a longer session before M6 sign-off. `just dist` produces the zip to hand to friend-testers when
   wanted.
2. **Then Slice B (B1–B7, 4-track audio)** — the real HW-risk engine work. Start at **B1**
   (`M7-M8-PLAN.md` §4): generalize `AudioStreamKind` (2-variant) → the mix/game/vc/other/mic track
   model through capture→resample→gaps→drift→AAC→ring→save→mux. **When B1 adds a stream variant, bump
   `AudioStreamKind::COUNT` + the `levels.rs`/`status.rs` exhaustive matches + the VU-meter and status
   color/label paths** (the seams are built to grow; see §6). Research facts for B1–B7 are in §4 and
   `M7-M8-PLAN.md` §5 — do not re-derive them. **Slice B also carries the last owed Slice-A fast-follow,
   the mic-device dropdown (B3.5** in M7-M8-PLAN §4 — WASAPI `EnumAudioEndpoints` wrapper on the B2/B7
   audio-COM HW cycle). The A6 live-hotkey-conflict fast-follow is already done (2026-07-08).

The Slice-A UI seams (two lock-free publish `Arc`s + the `Config::write_atomic` write path) are the
foundation Slice B extends; §2 documents them.
- Sequencing: friends-beta v0 (2-track, full UI) → Slice B (B1–B7, 4-track) → friends-beta v1 →
  **UI pass** → final friend release (M6 closes on beta evidence along the way; M7-M8-PLAN §7). The
  **UI pass planning is the gate** for the two deferred A6 items (live hotkey re-registration + its
  dependent record_toggle re-default) — decide build-or-drop then; not owed before.

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

### A8 DIST TEST — DEFERRED to post-Slice-B + UI pass (orchestrator, 2026-07-08)

Not run in this batch — the clean-machine unzip / SmartScreen "Run anyway" path will be exercised after
Slice B and the UI pass, on the friends-beta v1 build. Checklist kept for then:

- [ ] `just dist` → `target/dist/clipd-v<ver>.zip` builds (budget check passes first).
- [ ] Copy the zip to a **clean** Windows machine (or a fresh user), unzip → one `clipd-v<ver>/`
      folder with `clipd.exe`, `QUICKSTART.txt`, `config.template.toml`.
- [ ] Double-click `clipd.exe` → SmartScreen "unknown publisher" → **More info → Run anyway** →
      the tray icon appears and buffering starts (this IS the friends-beta first-run path).
- [ ] The quick-start's paths/hotkeys are accurate on that machine (clips folder, config, log).

### A7 HARDWARE TEST — DONE (Nitro V15, 2026-07-08) — all green ✅

- [ ] Save a couple of clips (hotkey), then Settings → **Recent clips** lists them newest-first
      (filenames `clipd_<ms>.mp4`); non-clipd `.mp4`s in the folder are NOT listed.
- [ ] **Open** plays the clip in the default player; **Folder** opens Explorer with the clip
      selected; **Copy path** puts the full path on the clipboard (paste to confirm).
- [ ] Close Settings (hide), save another clip, reopen Settings → the new clip appears **without**
      clicking Refresh (re-scan-on-reshow). **Refresh** also updates the list.
- [ ] Empty output dir → "No clips yet in …"; a huge folder → only the newest 20 shown.

### A6 FAST-FOLLOW HARDWARE TEST — STANDALONE, OWED (gates closing the 2026-07-08 fast-follow; `just run buffer`, release)

**This is its own gate, not part of the batched A4–A8 pass** — the live-conflict + text-entry
fast-follow closes only after this passes on the Nitro. Covers DECISIONS "2026-07-08 — A6 fast-follow".

- [ ] Each **Hotkeys** row shows the binding as an **editable monospace field** (e.g. `Ctrl+Alt+S`,
      NOT `Ctrl+Alt+KeyS`) + a **Rebind** button.
- [ ] **Rebind** a free combo (press `Ctrl+Alt+K`) → the field shows **`Ctrl+Alt+K`** (pretty token,
      no `KeyK`) and a green **✓ available** appears.
- [ ] **Live "taken":** in the field, TYPE a combo another running app owns (a classic: `Ctrl+Alt+R`,
      or an overlay's combo) → the row shows **⚠ in use by another app** with no restart. (Note: you
      must *type* it — pressing it via Rebind can't work, the OS routes the keystroke to the owning
      app; the capture prompt says as much.)
- [ ] Type the row's OWN current combo → **✓ available** (own combo, not a false "taken"). Type a
      free combo → **✓ available**. Type gibberish (`Ctrl+Foo`) → no note while incomplete; **Save**
      then shows the exact parse error and writes nothing.
- [x] **Cross-row conflict (`a6-ff-cross-conflict`, PASSED 2026-07-08):** type the OTHER row's current
      combo (e.g. Save's `Ctrl+Alt+S` into the Record field) → the row shows red **⚠ same as Save clip**
      (NOT a green ✓ available). Try it both directions. Modifier-order alias (`Alt+Ctrl+S`) is caught
      the same. Clearing the duplicate returns the row to ✓/⚠-taken as appropriate.
- [ ] A **⚠ taken** combo still **Saves** (surface, don't block) — config is written; on restart the
      log warns "could not register hotkey (already in use…)" and it simply doesn't fire.
- [ ] Check the log for a `could not release a probed hotkey` warning — there should be **none** in
      normal use (it would mean a probe leaked a registration).

### A6 HARDWARE TEST — DONE + fast-follow RE-VALIDATED (Nitro V15, 2026-07-08) ✅

**Result:** press-to-bind / restart-to-apply work. **Finding:** typing one row's combo into the OTHER
row (e.g. Save's `Ctrl+Alt+S` into the Record field) showed a false green **✓ available** — the pump's
availability probe reports our own already-registered combos as free and so can't see a cross-row
duplicate. **FIXED — branch `a6-ff-cross-conflict`** (merged 2026-07-08; DECISIONS "2026-07-08 — A6
fast-follow #2"): the row now shows red **⚠ same as {other row}** (UI-side parsed-combo compare, takes
precedence over the probe). **RE-VALIDATED on the Nitro 2026-07-08 — cross-row combo shows the red note
both directions, no false ✓. CLOSED.**

**Original A6 checklist (re-run alongside the cross-row re-check):**

- [ ] Settings → **Hotkeys** section shows the two current bindings (editable fields) + a **Rebind**
      button each.
- [ ] Click **Rebind** for Save clip → "press a combo…" → press e.g. `Ctrl+Alt+K` → the field shows
      `Ctrl+Alt+K`. **Esc** during capture cancels (binding unchanged).
- [ ] Try to bind the SAME combo to both → **Save** shows "save-clip and record hotkeys must
      differ" and writes nothing. Bind a bare key (no Ctrl/Alt) → capture ignores it.
- [ ] **Save** with new distinct bindings → `[hotkeys]` in `config.toml` updates; result says
      "Restart clipd to apply: …, hotkeys". **Restart** → the new combo fires the save/record; the
      old one no longer does.

### A5 HARDWARE TEST — DONE + fast-follow RE-VALIDATED (Nitro V15, 2026-07-08) ✅

**Result:** most of the editor works. **Two findings:**
1. **Output folder was not verified → silent clip-save failure.** A bogus dir (`ddddddddd`) was
   accepted + written; every later save then failed (`mux I/O error: os error 3`, logged, status
   "failed"). **FIXED — branch `a5-ff-output-dir`** (merged 2026-07-08; DECISIONS "2026-07-08 — A5
   fast-follow"): editor now `create_dir_all`s the folder on Save (rejects only if uncreatable, red
   error, nothing written); empty field now defaults to `%USERPROFILE%\Videos\clipd`; engine
   `prepare_output_dir` create-dir-with-fallback so saves can't silently break. **RE-VALIDATED on the
   Nitro 2026-07-08 — all three re-check items below pass. CLOSED.**
2. **Mic device id isn't checked to exist** — a bad pinned id just fails to open the stream. **Deferred
   to Slice B `B3.5`** (WASAPI `EnumAudioEndpoints` device list replaces the free-text id on the B2/B7
   audio-COM HW cycle) — accepted, not a regression.

**A5 re-check items — PASSED on the Nitro 2026-07-08** (`just run buffer`, release):

- [x] Set output folder to a **bad path** (e.g. a path under a file) + Save → **exact IO error in red,
      nothing written** (config unchanged).
- [x] Set output folder to a **new, creatable path** + Save → the folder is created; clips land there.
- [x] **Leave the folder blank** + Save → clips land in **`%USERPROFILE%\Videos\clipd`** (created if
      missing); the startup banner `clips -> …` shows that path.

**Original A5 checklist (the parts that passed 2026-07-08 stay green; re-run alongside the above):**

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

### A4 HARDWARE TEST — DONE (Nitro V15, 2026-07-08) — all green ✅

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

**New from A7:**
- **The settings window persists hidden across opens** (A2 model) — anything that must reflect state
  changed while hidden needs a re-show hook, not a once-at-construction read. A7's recent-clips list
  re-scans via a `Shared.rescan_recent` flag the tray sets on re-show + the app swaps. Reuse that
  pattern for any future "refresh on open" data.
- **Recent-clips uses the tray's resolved `output_dir`**, threaded through `SettingsHandle::open`
  (now takes `output_dir: &Path`) — the engine's actual save dir, not `config.output.dir`.

**New from A6:**
- **Hotkey validation is UI-side only** (`Editor::validate_hotkeys`), deliberately NOT in
  `Config::validate` — folding it in would make `Config::load(..).unwrap_or_default()` silently
  discard a whole user config on one bad `[hotkeys]` value. Compare hotkeys as PARSED `HotKey`s, not
  strings. Press-to-bind requires Ctrl or Alt (no bare-key global hotkeys).
- **Live conflict-detection now exists (A6 fast-follow, 2026-07-08); live *re-registration* does
  not.** The pump-control channel (`HotkeyControl` in `hotkey.rs`) test-registers a candidate combo to
  answer "already taken by another app?" at bind time — but the *working* hotkey is still applied only
  on restart. If you later want live re-register, the missing piece is telling the engine the new
  `HotKey::id()` (captured once at `BufferEngine::start`) without a restart. Any new pump-control verb
  rides the same `WM_HOTKEY_CONTROL`-woken channel; keep it `ui/UI → pump`, pump never touches `ui`.

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
  unsafe COM) + HW validation; **now folded into Slice B as B3.5** (rides B2/B7's audio-COM HW cycle —
  M7-M8-PLAN §4), not a separate A-follow-up. It's a deferred fast-follow, not a regression.

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
