# Session Handover — Recalibration pass done: M7+M8′ plan is set, T0 quality fix is next

> Onboarding note for the next session. `CLAUDE.md` and the `clipper-devpack/devpack/`
> docs are normative and override anything here. `02-AV-SYNC-SPEC.md` (frozen) overrides
> everything EXCEPT the dated `DECISIONS.md` amendments: the two M4-2 entries and the
> **three 2026-07-07 entries** (§2.2 process-loopback QPC pass-through, §2.5 track
> layout, §4 hybrid-moov finalize). Read **`M7-M8-PLAN.md`** (repo root) + the
> **2026-07-07 DECISIONS.md entry** — they ARE this session's output.

**Written:** 2026-07-07, after a **research/recalibration pass (NO code written)**.
Code state is unchanged since M5: **M0–M5 merged on `main`, 171 tests, clippy/fmt/deny
green, release 2.49 MB.** This session produced: three sourced web-research reports
(process-loopback API, VC-app landscape, multi-track MP4 + competitor UX), the
orchestrator-approved **M7+M8′ plan**, three frozen-spec amendments, an M8 reshape,
and one **measured defect (T0)**.

---

## 1. What was decided (orchestrator-approved, 2026-07-07)

**Strategy: a reshaped M7+M8 runs BEFORE M6.** Goal = a working, customizable download
for friend-testers; their varied hardware becomes the M6 matrix evidence (GTM §2.5
Phase-0 quiet beta). Sequence:

**T0 (quality fix) → Slice A (M7 UI, tasks A1–A8) → friends beta v0 → Slice B (M8′
4-track audio, tasks B1–B7) → friends beta v1 → M6 closes on beta evidence.**

- **4-track audio approved** (reshapes M8): container layout = **mix FIRST** (track 1;
  one-track players/CapCut/Discord/YouTube use exactly that track), then game / voice-
  chat / other-system / mic when `separate_tracks = true`; **mix + mic** when false.
  All tracks flagged enabled (disabled tracks vanish in editors).
- **"Other system" track contains VC audio too — accepted.** The API takes ONE process
  tree per client and excludes don't compose; `system − game − VC` is inexpressible.
  Documented, not engineered around (cross-client subtraction is research-grade).
- **Game-track binding:** window mode = captured window's tree. Monitor mode = no game
  track until the foreground becomes a fullscreen/borderless app, then that PID's tree,
  sticky while the process lives; a different fullscreen app retargets with a logged
  silence-filled gap. Foreground+fullscreen heuristic only — **no game database**
  (non-goal intact). Same detector M10's `buffer_when = "fullscreen-app"` needs.
- **Quality UX = named tiers (Efficient/Default/High/Max) over the CQP engine** with
  derived Mbps/RAM feedback. NO raw-Mbps mode. Raw CQ stays TOML-only.
- **Deps:** `toml_edit` approved (effective when Slice-A config rewrite lands, with the
  usual callout). `eframe`/`egui` already sanctioned for the UI module by CLAUDE.md.
  Process loopback needs NO new dep — whitelisted `wasapi` crate has
  `new_application_loopback_client` (its `include_tree: false` doc comment is WRONG —
  code does EXCLUDE mode).

## 2. DO THIS NEXT — T0: encoder quality calibration (urgent, small, standalone)

**Measured on the Nitro 2026-07-07:** three 1080p60 clips from the current binary
average **2.1 / 3.3 / 5.5 Mbps video** vs spec §6.1's **12–20 Mbps** expectation.
This is the orchestrator's observed "colorful scenes degrade badly" — a real defect,
not tuning taste. Root cause candidate: `mft_h264.rs` maps CQ 23 →
`AVEncCommonQuality = 55` via an uncalibrated linear formula (its own comment claims
"tuned against measured bitrate" — never happened; the NVENC MFT rejects
`AVEncVideoEncodeQP`, hence the 0–100 quality scale).

Task (see M7-M8-PLAN.md §1): on-HW sweep `AVEncCommonQuality` ≈ 55–85 over a 60 s
colorful/high-motion scene; ALSO check whether Quality mode is silently ceilinged by a
default `MF_MT_AVG_BITRATE` (if so, set a generous explicit ceiling). Fix the mapping
in `spec_constants.rs`; acceptance = "Default" tier lands in 12–20 Mbps + visual spot
check. This is §6.1's adjustment rule firing — normative, no re-freeze needed.

Then **Slice A** (A1 config-rewrite/schema-v2 → A2 egui satellite skeleton → A3 VU
meters → A4 status strip → A5 settings editor → A6 press-to-bind → A7 recent clips →
A8 `just dist` beta zip). Full task text in M7-M8-PLAN.md §3.

## 3. Research facts the next session must not re-derive (sourced in plan §5)

- **Process loopback** (`ActivateAudioInterfaceAsync` + PROCESS_LOOPBACK): works
  Win10 19041+ (docs claim 20348 — runtime-probe + hide below floor), anti-cheat-safe,
  no endpoint binding. The client is crippled (GetMixFormat/GetStreamLatency/
  IAudioClock/GetDevicePeriod E_NOTIMPL, GetBufferSize garbage, DevicePosition 0) BUT
  **`GetBuffer.QPCPosition` is valid and is already our tick master domain** — OBS 28+
  trusts it unconditionally. Request 48 kHz f32 directly (honored). Silence arrives as
  SILENT-flagged packets (keep gap synthesis armed). Process exit ⇒ likely silence
  forever, NO error — needs our own PID-liveness watchdog. Serialize activations.
  Known field issues: OBS #8086 long-session crackle/desync (unfixed there; our §2.4
  per-stream drift controller is the mitigation — prove it in B7), Win11 22H2
  device-loss crash report.
- **VC detection:** by process enumeration, NEVER by window (tray-minimized Discord
  breaks window pickers). Discord = top-most `Discord.exe` (parent not same-name) +
  include-tree (audio lives in an Electron child). Table ships as TOML data: Discord/
  PTB/Canary (P0 default), Vesktop/Legcord/TS3/TS6/Mumble (P1), Steam voice + Game Bar
  (P2). Skype + Guilded are dead — never add. In-game voice (Vivox/EOS/Steamworks:
  Valorant, Fortnite, Apex, LoL) renders INSIDE the game process — never separable →
  LIMITATIONS.md. Only Medal auto-detects Discord today; this is a differentiator.
- **Container:** MKV folklore doesn't apply (it was crash-safety + old OBS muxer);
  fMP4-on-disk quirks (Explorer duration, WMP seek) are solved by the approved
  OBS-Hybrid-style appended `moov` on save (§4 amendment). Uploads flatten to one
  track; editors read all enabled tracks.
- **Competitor defaults:** Steam Recording 12 Mbps default tier / NVIDIA ~20–50
  computed / Medal 3–100 slider; only OBS exposes CQP ("Indistinguishable" ≈ 18).
  Resolution UX convention: "Source (recommended)" + downscale tiers; hide options
  above source; GPU downscale rides our existing VideoProcessor canvas
  (`encode.max_height` already exists).

## 4. Environment facts (this machine = the Nitro V15 test box)

| Thing | Value |
|---|---|
| Repo root | `X:\Projects_X\clipd` |
| Rust | stable **1.95.0**, `x86_64-pc-windows-msvc` (pinned) |
| `CARGO_HOME` | `X:\cargo` (`X:\cargo\bin` NOT on PATH — prepend `$env:Path = "X:\cargo\bin;$env:Path"`) |
| GPU | RTX 4050 Laptop (Ada NVENC) + Intel iGPU; Optimus. Primary 1080p on the dGPU |
| Default audio | Realtek Headphones (render) + FIFINE mic (capture), both 48 kHz |
| ffprobe/ffmpeg | 7.0.1 on PATH |
| Config file | none by default — `%APPDATA%\clipd\config.toml` by hand. Hotkeys: save `Ctrl+Alt+S`, record `Ctrl+Alt+F9` |
| Log file | `%LOCALAPPDATA%\clipd\logs\clipd.log.<date>` |
| Run key | `HKCU\...\Run` value `clipd` (autostart; off by default) |
| Git remote | `origin` HTTPS (`github.com/ImTani/clipd`), gh authed `ImTani`. Push when ready |
| Zombie hotkeys | `taskkill /F /IM clipd.exe` between runs |
| Stray files | three test clips (`clipd_*.mp4`) sit UNTRACKED in the repo root — the T0 evidence; don't commit them |

## 5. Gotchas carried forward

- **Run `buffer` via `just run buffer`** (NOT `just run -- buffer`). Tray mode: Enter
  does not quit — use tray Quit. New icons hide in the Win11 "^" overflow flyout.
- **`common-controls-v6` breaks binary load** (DECISIONS "M5 T2 fixup") — keep
  `tray-icon` default-features off; `tests/smoke.rs` guards it. If Slice-A UI work
  ever wants themed controls, that's a manifest via build script, NOT the feature flag.
- **Satellite law for Slice A:** engine must never depend on/block on `ui`; window is
  lazily created; UI writes config ONLY through the versioned-TOML path.
- `--simulate-device-loss` is headless by design and does NOT exercise the tray
  Warning. `clip shorter than requested (§4.2)` on a young buffer is EXPECTED.
- Watchdog live-Warning (M5 leftover) is folded into the load rows of the eventual
  matrix; dead-worker → Error is wired.
- Carried M1–M4: `Closed` doesn't fire on window close → `IsWindow` poll; fixed canvas
  letterboxes odd aspects; `windows` 0.62 COM interfaces `!Send`/`!Sync`; only the
  `Win32_*` features actually called; `unsafe` confined to COM/D3D/MF/OS wrappers;
  never claim a HW path works until the machine says so.

## 6. Quick command reference

```
$env:Path = "X:\cargo\bin;$env:Path"          # first, always
just check            # fmt + clippy -D warnings + cargo check
just test             # nextest, 171 tests (incl. smoke.rs loading the real exe)
just release          # stripped release vs 10 MB budget (2.49 MB)
just run buffer                               # tray shell (M5)
just run -- buffer --record-secs 8            # headless auto-record self-test
just run -- record --seconds 15               # timed record (headless)
just verify clip.mp4                          # ffprobe assertion script
ffprobe -v error -select_streams v:0 -show_entries stream=bit_rate <clip>   # T0 check
```
