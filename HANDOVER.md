# Session Handover — Slice A COMPLETE + HW-VALIDATED; **Slice B UNDERWAY: B1 + B2 + B3 + B4 DONE + MERGED** (2026-07-08); NEXT = **B5 (muxer N-track + hybrid-moov)** — needs only B1

> **2026-07-08 — B4 landed (`b4-mixer` merged `--no-ff` to `main`; local-only, NOT yet
> pushed).** The always-first **Mix** track (container track 0) is now the real **−3 dB
> soft-clipped sum of the desktop loopback + mic**, retiring the B1/B2 **D2 pass-through**
> (track 1 was the raw desktop loopback). New pure **`src/audio/mixer.rs`** —
> **`TwoSourceMixer`**: PTS-aligns two already-resampled / gap-filled / drift-corrected
> 48 kHz streams on a shared **anchor** by absolute frame index, sums frame-for-frame, and
> emits a **gap-free contiguous** stream (**load-bearing**: the AAC encoder is a
> sample-counting clock — `mft_aac::stamp` derives AU PTS from a running sample count, so
> any hole would drift the whole track). Gain = `soft_clip((desktop+mic)·HEADROOM)`,
> HEADROOM −3 dB (0.708), soft clip **unity below |0.8|** then a C¹ cubic-Hermite knee to
> ±1 (normal levels pristine, only overshoot softened). **15 mixer unit tests.** Engine
> wiring (`engine.rs`): `TrackFeed` += **`Mix { mic_present }`** (Mix is no longer
> `Static(EndpointLoopback)`); **`track_feed(kind, mic: Option<&DeviceSelection>,
> supported)`**; new **`mix_process_thread`** (owns the desktop resampler + Mix AAC encoder;
> `select!`s over desktop capture packets + the fanned mic chunks + a 500 ms warm-up timer;
> publishes the Mix VU meter on the **mixed output**; sends the Mix ASC **eagerly** before
> data). **D3 fan-out**: the mic is captured + resampled **once** (Mic track) and its
> `ResampledChunk`s fanned to the mixer via `audio_process_thread`'s new **`chunk_fanout`**
> (**non-blocking `try_send` drop-on-full** — a slow mixer never stalls mic capture; the
> mixer silence-fills a dropped chunk by frame index → **no drift**, and the Mic track still
> encodes every chunk; dropped-count logged on teardown). **Zero double WASAPI clients, one
> drift domain per source. No new dep.** **D4 untouched** (ASC stays eager, track count
> fixed → the `v.len() == num_audio` save gate needs no change). **OtherSystem stays
> deferred** — its `endpoint↔process-exclude-tree(game)` source (D5) is HW work bound to the
> live game PID; a half-version would double game audio into OtherSystem the moment a game
> binds, so it splits to a later task (`planned_kinds` still plans it; `track_feed` still
> returns `None`; the deferral is still logged). **rust-reviewer'd — 1 HIGH (fixed) + 1
> MEDIUM (fixed) + 1 LOW (fixed):** HIGH was a **silent Mix av-sync bug** — `push` lowered
> the anchor when a later-*delivered* source had an earlier PTS (thread scheduling doesn't
> order channel delivery by PTS) but never re-based the already-placed source, so
> desktop/mic summed at the wrong offset; **fixed** by re-basing every placed source on
> anchor-lower (`SourceBuf::rebase`) + a regression test. MEDIUM: the fan-out was a *blocking*
> send mischaracterized as best-effort (a slow mixer could transitively stall the physical
> mic-capture callback) → switched to `try_send` drop-on-full. LOW: `push` doc corrected
> (clamp-to-0 + discard, not "trim"). Local-green **286 tests** (+15 over B3's 271), `just
> release` **8.96 MB** (+0.05). DECISIONS "2026-07-08 — Slice B / B4". **NO HW step on this
> branch (folds into B7).** **Next session: B5 (muxer N-track + hybrid-`moov` finalize)** —
> depends only on B1; handle the empty zero-AU per-app track case (the B3 gap). `main` is now
> **3 commits ahead of `origin/main`** — push when ready.

> **2026-07-08 — B3 landed (`b3-game-vc-binding` merged to `main`; local-only, NOT yet
> pushed).** Live game / voice-chat **PID binding** — this is the branch that turns the
> per-app process-loopback tracks (B2) ON at runtime. New **`src/audio/binding.rs`**: pure,
> exhaustively-tested detection — **`select_vc_pid`** (case-insensitive image match;
> **top-most same-name** = the Electron main, not a helper child; include-tree; config-order
> first-app-wins; tie→lowest PID), **`is_borderless_fullscreen`** (window covers `rcMonitor`
> — separates fullscreen from a taskbar-short maximized window), **`classify_game`**
> (monitor→foreground-fullscreen / window→captured PID; rejects system PIDs < 8), and the
> **`BindingTracker`** retarget state machine — plus **confined-unsafe OS providers**
> (`enumerate_processes` via Toolhelp; `foreground_window` via GetForegroundWindow/rect/
> monitor; `window_pid`), all with `// SAFETY:` notes, **HW-owed → B7**. **Engine wiring**:
> the `sources ≠ tracks` split gains **`TrackFeed{Static(AudioSource)|Bound(BoundRole)}`** +
> **`BoundRole{Game,VoiceChat}`**; `b1_spawnable`/`track_source` retired for
> `spawnable_feed`/`track_feed` (OS-support gated on `process_loopback_supported()`); a
> per-epoch **panic-free `binding_watcher_thread`** (scan 600 ms, stop-poll 120 ms) publishes
> each role's PID into a shared **`BindingState`**, and each bound track's
> **`run_bound_capture`** loop runs B2's `run_process_capture` on it, rebinding on retarget
> (generation-guarded arm/retarget race; `§2.3` fills the gap; the watcher's liveness is the
> bound captures' teardown guarantee). **`BufferParams.vc_apps`** threaded from config;
> `game_detect_for(CaptureSource)`. **OtherSystem stays deferred to B4** (D5 source switch).
> New **`binding-probe`** subcommand (`just run -- binding-probe [SECS]`) = the B7 HW
> instrument (exact engine code path, no drift), header carries the checklist. New `windows`
> feature `Win32_System_Diagnostics_ToolHelp` same-commit; **no new core dep.** **D4 NOT
> relaxed** — the ASC is emitted eagerly at audio-thread startup (source-independent), so
> every spawnable track satisfies the save gate whether or not a PID ever binds; track slots
> are fixed, only the PID under them rebinds (DECISIONS rationale). Confined `unsafe`,
> local-green: **271 tests** (+25), `just release` **8.91 MB**. **rust-reviewer'd — 1 HIGH
> (teardown TOCTOU in `run_bound_capture` → potential hung epoch-restart join; **fixed** with a
> `cap_stop` recheck beside the generation guard) + 1 LOW (fixed).** DECISIONS "2026-07-08 —
> Slice B / B3". **NO HW step on this branch (folds into B7).** **Next session: B4 (mixer) and/or
> B5 (muxer N-track + hybrid-moov)** — both depend only on B1. The `binding-probe` sanity-ran
> on this box (492 procs enumerated, foreground + Discord detected) but that is **not** B7
> validation.

> **2026-07-08 — B2 landed (`b2-process-loopback` merged to `main` `0e7378b`; local-only,
> NOT yet pushed — main is 3 commits ahead of `origin/main`).** The process-loopback capture
> spine is in: new **`src/audio/process_loopback.rs`** — `run_process_capture(kind, pid,
> include_tree, tx, stop)` via `wasapi::new_application_loopback_client` (PROCESS_LOOPBACK).
> Fixed **48 kHz f32 stereo** requested (crippled client can't `get_mixformat`); `QPCPosition`
> passed through the shared `PtsDeriver` (amended §2.2). **PID-liveness watchdog**
> (`OpenProcess`/`WaitForSingleObject`) ends capture on process-exit (silence-forever, no
> WASAPI error); **activations serialized** via a module `Mutex`; **Win10-2004 floor probe**
> (`RtlGetVersion`, build ≥ 19041, exposed `pub` for B3). **`run_capture` reshaped to dispatch
> on `AudioSource`** (endpoint variants → `run_endpoint_capture`; `ProcessLoopback` → the new
> module); B1's `selection()` shim retired. **Capability + dispatch ONLY — `b1_spawnable` is
> UNCHANGED, so the runtime still spawns Mix+Mic; process loopback spawns once B3 binds a PID.**
> Confined `unsafe` (SAFETY notes), pure parts unit-tested (+5). New `windows` feature gates
> (`Wdk_System_SystemServices`, `Win32_System_SystemInformation`) same-commit; **no new core
> dep.** New **`tools/audio-probe`** HW instrument (`just probe`) carries the B7 checklist.
> Local-green: **246 tests** (+5), `just release` **8.87 MB**. rust-reviewer'd (1 MEDIUM,
> addressed). DECISIONS "2026-07-08 — Slice B / B2". **NO HW step on this branch (folds into
> B7).** **Next session: begin at B3** (`SLICE-B-PLAN.md §3` / this doc §3) — game/VC PID
> binding, which drives B2's producer. B4 (mixer) and B5 (muxer/hybrid-moov) depend only on B1
> and can still go in parallel. The 2 h UI soak + A6-fast-follow HW test still fold into **B7**.

> **2026-07-08 — B1 landed earlier (`b1-track-model` merged `0d368e1`, pushed).** The N-track
> audio model: `AudioStreamKind{Desktop,Mic}` → **`AudioTrackKind`** (5:
> Mix·Game·VoiceChat·OtherSystem·Mic) + the **`AudioSource`** enum (the "sources ≠ tracks"
> split). Pure builder `planned_kinds(TrackModel)` (full topology, exhaustively tested);
> **B1 spawns Mix + Mic only** — Game/VoiceChat/OtherSystem *planned but deferred* (`b1_spawnable`
> gate, logged once via `warn_deferred_tracks`). `separate_tracks` **wired** (was schema-only) +
> **default flipped to `false`** (Mix+Mic, D1). DECISIONS "2026-07-08 — Slice B / B1".

> **Planning context (still current):** **`SLICE-B-PLAN.md`** (repo root) is the working plan
> for Slice B (B1–B7 + B3.5) and **supersedes `M7-M8-PLAN.md §4`**. D1/D2 locked + D-B1 logged
> in `DECISIONS.md`. The audio pipeline (`ring`/`save`/`mux`/epoch loop/thread-spawn) is
> N-track generic; B1 proved the enum/wiring edits were narrow as predicted.

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

- **M0–M5 + T0 + A1–A8 (Slice A) + B1 + B2 + B3 + B4 merged on `main`.** Working tree clean. **286
  tests** (nextest; +15 from B4 — 15 in `mixer.rs` + updated engine tests — on top of B3's 271).
  `just check` (fmt + clippy -D warnings) green. Release build **8.96 MB** vs the 10 MB budget
  (+0.05 from B3). `just dist` → `target/dist/clipd-v<ver>.zip` (~3.85 MB), verified
  end-to-end (last run at A8; not re-run for B1–B4).
- **`main` is 3 commits AHEAD of `origin/main` (B4 not yet pushed).** `origin/main` = `43e9ef8`
  and already includes **B1 + B2 + B3** (all pushed — the prior handover's "B3 not pushed / 3
  ahead" note was stale; at this session's start `main == origin/main`). B4 (`1995763` feat →
  `2d784e6` review-fix → `07b324e` merge) is merged locally only (the task said "merge", not
  push). Push when ready: `git push origin main`.
- **B4 (software mixer — real Mix track) DONE + merged (2026-07-08).** New pure
  `src/audio/mixer.rs` (`TwoSourceMixer`, 15 tests); `TrackFeed::Mix{mic_present}` +
  `mix_process_thread` sum the desktop loopback + mic (−3 dB + soft clip), retiring the D2
  pass-through. D3 fan-out (mic captured/resampled once, `try_send` drop-on-full to the mixer).
  D4 untouched; OtherSystem still deferred (its exclude-tree source / D5 is HW, split to a later
  task). rust-reviewer'd (1 HIGH anchor-rebase + 1 MEDIUM fan-out + 1 LOW, all fixed). Pure-logic
  + narrow wiring, local-green, **no HW owed of its own** (folds into B7 audio-COM cycle). See the
  top banner + DECISIONS.
- **B3 (game/VC PID binding) DONE + merged (2026-07-08).** New `src/audio/binding.rs` (pure
  detection + confined-unsafe OS providers); `TrackFeed`/`BoundRole` split; per-epoch
  `binding_watcher_thread` + `run_bound_capture` drive B2's process loopback with a live PID.
  **Per-app tracks (Game/VoiceChat) now spawn at runtime** under `separate_tracks=true` above the
  Win10-2004 floor; OtherSystem still deferred to B4. D4 NOT relaxed (ASC is eager — see banner /
  DECISIONS). `binding-probe` instrument. Confined-unsafe, local-green, **HW owed → B7**.
- **B2 (process-loopback capture) DONE + merged (2026-07-08; `0e7378b`).** New
  `src/audio/process_loopback.rs`; `run_capture` reshaped to dispatch on `AudioSource`;
  PID-liveness + serialized activations + Win10-2004 floor probe; `tools/audio-probe`
  instrument. **Capability + dispatch only — runtime still Mix+Mic (b1_spawnable unchanged);
  process loopback spawns once B3 binds a PID.** See the top banner + §3/§6. Confined-unsafe,
  local-green, **HW owed → B7** (the `tools/audio-probe` header + §5-below carry the checklist).
- **B1 (N-track audio model) DONE + merged + pushed (2026-07-08; `0d368e1`).** `AudioTrackKind`
  (5 variants) + `AudioSource` split; `separate_tracks` wired + default→`false`; Mix pass-through
  (D2). Pure-logic, local-green, **no HW owed** (folds into B7).
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
- Last commits: `bc296f5` Merge b3-game-vc-binding → `57ce7da` B3 review-fix → `af66c1d` the B3
  feat commit → `fe1aedc` (= `origin/main`, the B2 handover).
- **`origin/main` = `fe1aedc`, PUSHED through B1 + B2** (remote HTTPS `github.com/ImTani/clipd`,
  gh authed `ImTani`). Working tree clean; the `b3-game-vc-binding` branch was merged `--no-ff`
  and deleted. **B3 (3 commits) is local-only — push when ready.**
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

## 3. DO THIS NEXT — B5 (muxer N-track + hybrid-moov); read `SLICE-B-PLAN.md §B5` first

**B1 + B2 + B3 + B4 are DONE** (see top banner). The track model, the `sources ≠ tracks` seam, the
process-loopback capture source, the live game/VC PID binding, AND the real desktop+mic **mix** all
exist. Under `separate_tracks = true` (above the Win10-2004 floor) the runtime spawns Mix + Game +
VoiceChat + Mic; the default (`separate_tracks = false`) path is Mix + Mic with the Mix now the real
−3 dB sum. **OtherSystem is the only planned track still deferred** (its endpoint↔process-exclude
source switch / D5 is HW-bound to the live game PID — split out of B4; see the B4 banner).

**Start at B5 (muxer N-track + hybrid-`moov` finalize, `SLICE-B-PLAN §B5`)** — depends only on B1
(the mux/save/ring are already N-track generic):
- Confirm `build_moov` orders Mix first + sets enabled/in-movie flags; compute per-track sample
  tables (`stts`/`stsz`/`stsc`/`stco`/`stss`) + append a finalized `moov` on save (OBS-Hybrid:
  fragments-first for crash-safety, finalized `moov` for editor/Explorer compatibility). Preserve
  §4.7 atomicity + §4.6 fragment-first ordering.
- **⚠ B3 GAP for B5:** an unbound-all-session per-app track (e.g. no VC app ever runs) is an
  **empty audio track** — ASC present, zero AUs. B3/B4 keep the save gate satisfied (ASC is eager)
  but do NOT guarantee an empty track muxes cleanly. **B5 must handle the zero-AU track case**
  (silence-fill the whole clip, or drop the empty track from `moov`). Not on the default (Mix+Mic)
  path, so CI is unaffected; but exercise it in B5/B7 with `separate_tracks=true` and no VC app.
- **Also splits out of B4 (do alongside B5 or as its own task): OtherSystem + D5.** Give
  `AudioTrackKind::OtherSystem` a source — the default-endpoint loopback when no game is bound, a
  process-**exclude**-tree(game) client once a game binds (D5 = within-epoch logged silence gap at
  the switch, NOT a video epoch bump — confirm it doesn't restart the ring/encoder). It is NOT a
  `BoundRole` (endpoint-or-exclude, not include-tree); add its `track_feed` arm + a new feed
  variant. **HW-risk** (exclude-mode process loopback) → validate at B7. B4 left it deferred on
  purpose (a half-version doubles game audio into OtherSystem the moment a game binds).

**The B4 mixer seams for reference** (`engine.rs`/`src/audio/mixer.rs`): `TrackFeed::Mix{mic_present}`
→ the spawn loop routes it to a desktop-loopback `run_capture` + `mix_process_thread`, which owns the
desktop resampler + Mix AAC encoder and `select!`s over desktop packets + the fanned mic chunks
(`audio_process_thread`'s `chunk_fanout`, `try_send` drop-on-full) + a warm-up timer. The pure
`TwoSourceMixer` (mixer.rs) aligns two 48 kHz streams by absolute frame index off a shared anchor and
emits a gap-free contiguous stream (the AAC encoder is a sample-counting clock — do not feed it holes).

**The B3 seams for reference** (`engine.rs`): `spawnable_feed`/`track_feed` (pure, OS-support
gated) → `spawnable_streams` (layers the live `process_loopback_supported()` probe) → the spawn
loop routes `TrackFeed::Static` to `run_capture` and `TrackFeed::Bound` to `run_bound_capture` +
one `binding_watcher_thread`. `BindingState`/`RoleSlot` carry the live target + a generation +
the in-flight run's stop flag (watcher interrupts on retarget/teardown). `binding.rs` is the pure
detection brain (inject a `ProcessInfo`/`ForegroundWindow` snapshot to unit-test).

**B3's HW validation is owed at B7** — `just run -- binding-probe [SECS]` and the checklist in the
`run_binding_probe` header (`main.rs`): Discord tray-minimized detection; VC config order;
game bind on a borderless title; foreground/maximized false-bind rejection; retarget gap;
cross-check PIDs vs Task Manager. **B2's** HW is also owed at B7 — `just probe` +
`tools/audio-probe` header (QPCPosition epoch vs raw QPC; process-exit + liveness teardown;
dead-PID activation HRESULT; same-PID double capture — now reachable via B3's binding; Discord
tray-minimized; serialized-activation no-deadlock). The still-owed **2 h UI soak** and the
**A6-fast-follow standalone HW test** fold into the same **B7** Nitro cycle.

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

**New from B4:**
- **The Mix track (track 0) is no longer a `Static` capture — it's `TrackFeed::Mix` + a dedicated
  `mix_process_thread`.** Don't reintroduce a `Static(EndpointLoopback)` feed for Mix; the desktop
  loopback is captured by a plain `run_capture` thread whose packets feed the mix thread, which owns
  the desktop resampler + the Mix AAC encoder and sums in the mic.
- **The mixer's emitted stream MUST stay gap-free from its anchor.** The AAC encoder is a
  sample-counting clock (`mft_aac::stamp`: AU PTS = `anchor + au_index·frame_dur`), so a hole in the
  mix input drifts the whole track. `TwoSourceMixer` guarantees this by placing chunks by absolute
  frame index (gap → silence-pad) and only ever advancing a monotonic `emitted` cursor. If you touch
  the mixer, preserve that invariant (the `output_is_contiguous_across_incremental_drains` test guards it).
- **Anchor can LOWER during warm-up (out-of-PTS-order channel delivery) and must rebase placed
  sources.** The B4 review caught a silent av-sync bug where lowering the anchor didn't shift an
  already-placed source. `push` now calls `SourceBuf::rebase` on both sources when the anchor drops
  (regression test `later_pushed_source_with_earlier_pts_rebases_the_first`). Any change to anchoring
  must keep this.
- **The mic → mixer fan-out is `try_send` drop-on-full, NOT a blocking send** (`forward_to_mixer`).
  A dropped chunk is silence in the mix at that frame position (no drift, because the mixer places by
  index) and the Mic track still encodes it — a slow mixer must never stall the physical mic capture
  callback. Dropped-count is logged per track on teardown. Keep it non-blocking.
- **Desktop-only mix is −3 dB vs the old D2 pass-through** (the mix-bus headroom applies with one or
  two sources; the "−3 dB gain exact" test pins it). Accepted; the only default-path behaviour change.
- **`track_feed(kind, mic: Option<&DeviceSelection>, supported)`** — the mic arg is `Some(sel)` when
  the mic is on (feeds both the Mic track and the Mix), `None` when off. Not `&DeviceSelection` anymore.

**New from B2:**
- **`run_capture` now takes an `AudioSource`, not a `DeviceSelection`.** It dispatches:
  endpoint variants (`EndpointLoopback`/`MicEndpoint(sel)`) → `run_endpoint_capture` (the old
  body, renamed, unchanged device-rebuild machinery); `ProcessLoopback{pid, include_tree}` →
  `process_loopback::run_process_capture`. The B1 `AudioSource::selection()` shim is **gone** —
  don't reintroduce it.
- **Process-loopback capture is `src/audio/process_loopback.rs`.** It requests a **fixed 48 kHz
  f32 stereo** format (the loopback client's `get_mixformat`/`get_device_period`/… are
  `E_NOTIMPL`), so `packet.sample_rate == 48_000` and the downstream resampler runs an identity
  ratio — but the `§2.4` drift controller still corrects, because it works off the **real
  `QPCPosition`** (the master domain, amended §2.2) vs sample count, not the nominal rate. Don't
  "fix" the identity ratio.
- **`include_tree = true` is INCLUDE, `false` is EXCLUDE** (the `wasapi` crate's own doc example
  is misleading). Game/VC = include-tree; other-system-with-game-bound = exclude-tree(game).
- **Process exit is silent — no WASAPI error.** `run_process_capture` owns a **PID-liveness
  watchdog** (`OpenProcess(PROCESS_SYNCHRONIZE)` + `WaitForSingleObject(h,0)` each tick); on exit
  it ends the capture (track → silence, downstream §2.3 fills it). It returns **`Ok(())`** on
  process-exit / activation-failure / unsupported-OS — by design (same as the endpoint path's
  device-loss rebuild); never an engine error. Best-effort: if the PID can't be opened, capture
  runs without exit detection.
- **Activations are serialized** by a module `static ACTIVATION_LOCK: Mutex<()>` held across
  `new_application_loopback_client` only (parallel activation spam froze OBS). Any new
  activation path must take the same lock.
- **`process_loopback_supported()` is the Win10-2004 (build 19041) floor probe** (`RtlGetVersion`;
  `GetVersionEx` lies without a manifest we don't ship). Docs *claim* 20348 — the doc is wrong,
  19041 is the real floor. **B3's spawn gate must call it to hide the per-app tracks below the
  floor.** `build_supports_process_loopback(build)` is the pure, tested mapping.
- **B2 did NOT flip `b1_spawnable`** — runtime is still Mix+Mic. Process loopback is
  dispatchable + probe-exercised but not spawned until B3 binds a PID. So **D4 (ASC-complete save
  gate) is still untouched** — relax it in B3 when conditional/late tracks appear.
- **New `windows` features:** `Wdk_System_SystemServices` (RtlGetVersion) +
  `Win32_System_SystemInformation` (OSVERSIONINFOW). `Win32_System_Threading`/`Win32_Foundation`
  (OpenProcess/WaitForSingleObject/CloseHandle) were already on. **No new core dep.**
- **`tools/audio-probe` (`just probe`) is the B2 HW instrument** — a standalone crate (own
  `[workspace]`, `wasapi` + `hound`, never linked into `clipd`). It re-implements the activation
  open sequence and is kept in lock-step with `process_loopback::open_process_session` **by
  comment** — if you change the module's open sequence, mirror it there. Its header carries the
  full B7 checklist.

**New from B1:**
- **`AudioStreamKind` is gone — it's `AudioTrackKind` (5 variants; `Desktop`→`Mix`).** It's now
  the **track/meter identity**, NOT the source. Capture source is the new `AudioSource`
  (`EndpointLoopback` · `MicEndpoint(DeviceSelection)` · `ProcessLoopback{pid,include_tree}`).
  Keep the split: a track is fed by a source (Mix by a sum of two in B4). `COUNT=5`; the
  `levels.rs` `const _` assert + `index()`/`label()`/`title()` matches force every new arm.
- **B1 spawns Mix + Mic ONLY.** `planned_kinds` builds the full 5-track topology but
  `spawnable_streams` filters to `b1_spawnable` (Mix, Mic); the rest are logged once
  (`warn_deferred_tracks`) and dropped. To make Game/VoiceChat/OtherSystem real, flip their
  `b1_spawnable` arm **and** give them a `track_source` — do NOT add a second spawn path.
- **`spawnable_streams`/`spawnable_kinds` are the single source of truth** for the supervisor's
  capture list AND the shell's VU-meter set (both pure fns of the same `BufferParams`) — they
  can't drift. Don't reintroduce a second "which tracks" computation.
- **`separate_tracks` is now WIRED** (it was schema-only/unread through Slice A — the old
  `config.rs` doc was wrong) and **defaults to `false`** (Mix+Mic; D1). The config template + the
  `shipped_config_template_matches_defaults` drift test track this — change all three together.
- **`spec_constants::audio::TRACK_DESKTOP`/`TRACK_MIC` were REMOVED** (dead + wrong order).
  `AudioTrackKind::index()` is the sole source of container-track order. Don't reintroduce index
  constants.
- **`run_capture` still takes `(AudioTrackKind, DeviceSelection)`**; `AudioSource::selection()`
  bridges at the spawn loop. **B2 should reshape `run_capture` to take an `AudioSource`** and add
  the process-loopback open path (the endpoint `match kind` arms for Game/VoiceChat/OtherSystem
  are unreachable placeholders today).

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
just test             # nextest, 286 tests (incl. smoke.rs loading the real exe)
just release          # stripped release vs 10 MB budget (8.96 MB with the UI+B4 stack)
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
