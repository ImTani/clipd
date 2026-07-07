# M7+M8′ Plan — friends-beta slice (UI, 4-track audio, calibrated defaults)

**Written:** 2026-07-07, from a research/recalibration pass (no code). Orchestrator
approved pulling a reshaped M7+M8 ahead of M6 so a working, customizable build can go
to friend-testers — whose varied hardware then *becomes* the M6 matrix evidence
(GTM §2.5 Phase-0 "20-user quiet beta"). This file is the working plan; `CLAUDE.md`
and the devpack stay normative except where the dated DECISIONS **2026-07-07**
amendments below override `02-AV-SYNC-SPEC.md`.

Research basis: three sourced web-research reports (process-loopback API, VC-app
landscape, multi-track MP4 compat + competitor UX), summarized in §5. Findings that
contradict devpack assumptions are called out inline.

---

## 0. Orchestrator decisions (2026-07-07, quoted intent)

1. **4-track audio approved**; "Other system" track containing VC audio too (API
   cannot express `system − game − VC`) is **accepted and documented**.
2. **Game-track binding**: window mode → the captured window's process tree. Monitor
   mode → *no game track* until the foreground becomes a fullscreen/borderless app;
   then bind that PID's tree. No game-title database (non-goal intact) — pure
   foreground+fullscreen heuristics. Binding sticks while the process lives; a
   *different* fullscreen app retargets with a logged, silence-filled gap.
3. **Quality UX = named tiers over the CQP engine** (no raw-Mbps rate-control mode).
   Orchestrator flagged current quality as visibly bad in colorful scenes →
   **measured and confirmed as a calibration bug**, see T0 below.
4. **Spec/plan amendments approved** (recorded in DECISIONS 2026-07-07):
   - `§2.5` track layout: v1's "two tracks, no mixed" → mixed-first + optional
     per-app tracks (layout table §2 below).
   - `§2.2` audio timestamping: process-loopback streams have no DevicePosition /
     IAudioClock / GetStreamLatency; their per-packet `QPCPosition` from
     `IAudioCaptureClient::GetBuffer` IS the master domain — pass it through
     directly. §2.3 gap synthesis and §2.4 drift control apply unchanged.
   - `§4` saved-clip finalization: adopt OBS-Hybrid-MP4-style `moov` finalize on
     save (fragments + appended moov) for editor/Explorer compatibility. Crash-
     safety intent of §4.6 is preserved (fragments still written first).
   - **M8 reshaped** from "include/exclude modes + optional third mixed track" to
     the fixed 4-track topology below.
   - `toml_edit` joins the whitelist **when the config-rewrite task lands** (needed
     for pitfall-30 unknown-key/comment preservation; callout required in that
     task's summary). `eframe`/`egui` already sanctioned for the UI module by
     CLAUDE.md.

---

## 1. T0 — Encoder quality calibration (URGENT, standalone, before/with Slice A)

**Measured 2026-07-07 on the Nitro** (ffprobe, 1080p60 H.264 saves from the current
binary): video avg bitrate **2.1 / 3.3 / 5.5 Mbps** across three clips vs the spec
§6.1 expectation of **12–20 Mbps** at NVENC CQ 23. Root cause candidate:
`mft_h264.rs` maps CQ 23 → `AVEncCommonQuality = 55` via an uncalibrated linear
formula (`100 − cq·100/51`); the code comment says "tuned against measured bitrate
on the test machine" — that tuning never happened.

Task: on-HW sweep of `AVEncCommonQuality` (≈55/60/65/70/75/80/85) over the standard
60 s colorful/high-motion scene; also test whether NVENC-via-MF Quality mode is
silently ceilinged by a default `MF_MT_AVG_BITRATE` (if so, set a generous explicit
ceiling). Deliverable: corrected CQ→quality mapping table in `spec_constants.rs`
(per-vendor rows stay), measured Mbps per tier recorded here and in DECISIONS.
Acceptance: "Default" tier lands in the §6.1 12–20 Mbps band on the test scene;
visual spot-check of a confetti/smoke clip.

This is the §6.1 adjustment-rule machinery firing — the rule is normative, no spec
re-freeze needed.

---

## 2. Target audio topology (Slice B end-state)

| # | Track | Source | Notes |
|---|-------|--------|-------|
| 1 | **Mix** (always on, default-only track for non-track users) | endpoint loopback + mic, software sum, −3 dB headroom, soft clip (M8's recipe) | MUST be first audio track: CapCut/browsers/Discord/platform re-encodes play/keep exactly one track, first-wins. Windows already mixes game+VC+system for us — the mix is nearly free |
| 2 | Game | `include-tree(game PID)` | Binding per decision 0.2. Silent/absent when no game bound |
| 3 | Voice chat | `include-tree(VC app PID)`, process-detected, Discord default | Contains pings/soundboard/Go-Live audio too. In-game voice (Vivox/EOS/Steamworks — Valorant, Fortnite, Apex, LoL) renders inside the game process and can NEVER be separated → LIMITATIONS.md |
| 4 | Other system | `exclude-tree(game PID)` when game bound; plain endpoint loopback otherwise | **Also contains VC** (accepted, decision 0.1). DMCA case works: mute track 4, music dies, voice survives on 3. Editors keeping 3+4 double the VC — document |
| 5 | Mic | existing WASAPI capture | unchanged |

`separate_tracks = false` (default) ⇒ container has tracks 1+5? **No** — default =
track 1 (mix) only… **decide at build time between** (a) mix-only, (b) mix+mic
(current users' muscle memory). Plan default: **mix + mic** (2 tracks: preserves
today's "my voice recoverable in post" value, still CapCut-safe because mix is
first). `separate_tracks = true` ⇒ all five.

All tracks: `track_enabled | track_in_movie` flags set (disabled tracks vanish in
editors — HandBrake bug-report precedent). AAC 160 kbps each (§2.6 unchanged).

### VC app detection table (ships as TOML data, not code)

| Priority | App | Process names | Capture |
|---|---|---|---|
| P0 default | Discord | `Discord.exe`, `DiscordPTB.exe`, `DiscordCanary.exe` | top-most same-name process (parent not same-name), include-tree — audio lives in an Electron child; works tray-minimized |
| P1 | Vesktop / Legcord | `Vesktop.exe` / `Legcord.exe` (verify on HW) | same pattern |
| P1 | TeamSpeak 3 / 5-6 | `ts3client_win64.exe` / `TeamSpeak.exe` (verify TS6) | direct PID / tree |
| P1 | Mumble | `mumble.exe` | direct PID |
| P2 | Steam voice | `steam.exe` + tree (audio in `steamwebhelper.exe`) | tree also catches store videos — caveat |
| P2 | Xbox Game Bar party | `XboxGameBarWidgets.exe` (verify) | UWP; flaky reports in OBS |
| never | Skype (dead 5/2025), Guilded (dead 12/2025) | — | — |

Detection rules (from OBS field failures): detect by **process enumeration, never by
window** (tray-minimized Discord breaks window pickers); log chosen PID + why on
every capture start.

Platform floor: process loopback needs Win10 2004 (19041; docs claim 20348).
Runtime-probe, hide the per-app tracks below the floor (M8's original plan) — the
mix/mic pipeline is unaffected.

---

## 3. Slice A — "M7: the satellite" (UI first; CI-green winnable)

Order within slice = devpack priority (meters before cosmetics). Branch per task.

- **A1 — config rewrite path + schema v2.** Unknown-key/comment preservation via
  `toml_edit` (whitelist callout); `config_version 1→2` migration (v1 files load,
  get new keys' defaults); new keys: `encode.quality = "efficient"|"default"|
  "high"|"max"` (per-vendor CQ map), `encode.resolution` (native|1440|1080|720 —
  subsumes/deprecates raw `max_height`), `[audio.tracks]` block + `[[audio.vc_apps]]`
  table (defaults per §2). UI writes ONLY through this path (same as --check-config).
- **A2 — settings window skeleton** (egui/eframe, lazily created from tray,
  satellite law: engine never blocks on it; enforce `ui → engine` dependency
  direction). Cold-open < 300 ms budget.
- **A3 — VU meters** (both current streams; grows to N tracks in Slice B). Ships
  before anything cosmetic — devpack: highest-value UI element.
- **A4 — status strip**: state, buffer fill (seconds held vs configured), capture
  target, res/fps/codec/vendor, last save result+time, dropped-frame + watchdog
  counters.
- **A5 — settings editor**: quality tier (with **derived feedback**: measured-rate
  estimated Mbps + "buffer ≈ N s / X MB RAM"), resolution (native default,
  downscale tiers via existing VideoProcessor canvas; hide options above source),
  fps, buffer seconds, audio device pickers, output dir, clear-after-save. Invalid
  edits show --check-config's exact errors.
- **A6 — hotkey press-to-bind** with conflict detection (tolerant RegisterHotKey
  already warns; surface it). Also re-default `record_toggle` if conflicts persist.
- **A7 — recent clips list** (last 20: open / open folder / copy path — no editor,
  no thumbnails-with-scrubbing).
- **A8 — friends-beta packaging (lean M10 cut)**: `just dist` portable zip, one-page
  quick-start (incl. SmartScreen "unknown publisher" note), default-config template.
  No signing/winget/installer yet.

M7 acceptance (unchanged from 08): cold-open < 300 ms; 2 h open-window soak, zero
engine stalls attributable to UI.

## 4. Slice B — "M8′: four-track audio" (engine work + real HW cycle)

- **B1 — N-track generalization**: `AudioStreamKind` 2-variant enum → track model
  (mix/game/vc/other/mic) through capture→resample→gaps→drift→AAC→ring→save→mux.
  Pure-logic parts stay 100% safe + unit-tested.
- **B2 — process-loopback capture module** (`audio/process_loopback.rs`):
  `wasapi` crate's `new_application_loopback_client` (already whitelisted; NB its
  `include_tree: false` doc comment is wrong — it's EXCLUDE mode); 48 kHz f32
  requested directly; event-driven; QPCPosition pass-through per amended §2.2;
  PID-liveness watchdog (process exit ⇒ likely silence-forever, no error — own
  detection required); serialized activations (parallel activation spam froze OBS).
- **B3 — game/VC binding**: fullscreen-foreground detector (monitor mode) + captured-
  window PID (window mode); VC process scanner over the TOML table; rebind logic
  with logged gaps.
- **B4 — mix track**: loopback+mic sum, −3 dB headroom, soft clip; alignment via
  existing QPC stamps.
- **B5 — muxer**: N audio tracks, mix first, all enabled-flagged; Hybrid-style
  `moov` finalize on save (amended §4).
- **B6 — LIMITATIONS.md + docs**: in-game voice not separable; VC bleed in track 4;
  pings/soundboard on VC track; uploads flatten to track 1; browser-based VC out of
  scope; Win10 <2004 hides per-app tracks.
- **B7 — HW validation cycle (Nitro)**: AV-1..AV-5 re-pass with tracks on (M8's
  acceptance line) + empirical checklist: QPCPosition epoch vs raw QPC; process-exit
  behavior; mute-state behavior; dead-PID activation HRESULT; same-PID double
  capture; Discord tray-minimized; long-session (≥1 h) crackle/drift watch (OBS has
  an unfixed desync there — our per-stream §2.4 controller is the mitigation, prove
  it); a 5-track clip → Discord upload + CapCut import behave (mix plays).

Budget flags (constraint 7, surfaced now): §6.4's "CPU 2% (2 audio)" needs
re-baselining at 5 streams + 5 AAC encoders (expect ~+0.5–1%, measure); ring RAM
+~1.2 Mbit/s (negligible); binary +egui (expect ~5–6 MB total vs 10 MB budget).

## 5. Research summary (details in session log, sourced)

- **Process loopback**: real, anti-cheat-safe, no IMMDevice/endpoint binding;
  crippled client (`GetMixFormat`/`GetStreamLatency`/`IAudioClock`/`GetDevicePeriod`
  E_NOTIMPL, `GetBufferSize` garbage, DevicePosition 0) BUT `GetBuffer.QPCPosition`
  valid — OBS 28+ trusts it unconditionally in production. Silence arrives as
  SILENT-flagged packets (keep gap synthesis armed anyway). One tree per client;
  excludes don't compose — hence track-4 compromise. Known field issues: OBS #8086
  long-session crackle/desync (closed, unfixed), Win11 22H2 device-loss crash report.
- **VC landscape**: Discord dominant (+PTB/Canary/Vesktop/Legcord); TS6/Mumble/Steam/
  Game Bar niche; Skype & Guilded dead. Only Medal auto-detects Discord to its own
  track; NVIDIA App = 2 tracks max, no VC split; Steam Recording = 1 track. Auto VC
  track = genuine differentiator.
- **Container**: multi-track MP4 fine IF mix is first + all tracks enabled-flagged;
  CapCut reads one track only; YouTube/X flatten; MKV folklore = crash-safety + old
  OBS muxer, not a container limit; fMP4-on-disk quirks (Explorer duration, WMP
  seeking) solved by OBS-Hybrid-style finalize.
- **Rate-control UX**: consumers speak Mbps (Steam 12 Mbps default tier, NVIDIA
  computed ~20–50, Medal 3–100 slider); only OBS exposes CQP ("Indistinguishable" ≈
  18). Named tiers + derived Mbps/RAM feedback = our shape. Resolution: "Source
  (recommended)" + downscale tiers is the universal convention; GPU downscale rides
  our existing VideoProcessor stage.

## 6. Out of scope for this slice (unchanged ratchet)

Cross-client audio subtraction (research-grade, nobody ships it); browser-tab VC
detection; software mixer beyond sum+clip (no AGC/filters — permanent); M9 codecs;
M10 signing/installers; anything on the 08 REJECTED list.

## 7. Sequencing

**T0 → Slice A (A1..A8) → friends beta v0 (2-track, good quality, full UI) →
Slice B (B1..B7) → friends beta v1 (4-track) → M6 matrix closes on beta evidence.**
T0 first because every beta clip inherits its fix; A before B because A delivers
the "looks fine, customizable" ask and B carries the validation risk.
