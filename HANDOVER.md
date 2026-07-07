# Session Handover — T0 encoder calibration DONE; Slice A (M7 UI) is next

> Onboarding note for the next session. `CLAUDE.md` and the `clipper-devpack/devpack/`
> docs are normative and override anything here. `02-AV-SYNC-SPEC.md` (frozen) overrides
> everything EXCEPT the dated `DECISIONS.md` amendments: the two M4-2 entries, the three
> **2026-07-07** entries (§2.2 process-loopback QPC, §2.5 track layout, §4 hybrid-moov),
> and now the **2026-07-07 "T0 resolution" entry** (§6.1 CQP → bitrate-target VBR). Read
> **`M7-M8-PLAN.md`** (repo root) — it is the working plan for this whole phase.

**Written:** 2026-07-07, after **T0 was implemented, HW-validated, and merged.** This
session took the M7+M8′ plan's first task (T0 encoder calibration) from "measured defect"
to "fixed on hardware," writing real code for the first time since M5.

---

## 1. Code state

- **M0–M5 + T0 merged on `main`.** Working tree clean. **173 tests** (nextest), was 171
  (+2 T0 invariant tests). `just check` (fmt + clippy -D warnings + check) green. Release
  builds at **2.51 MB** (2,632,192 bytes) vs the 10 MB budget.
- Last commits: `861c2b4` Merge t0-encoder-calibration → `80a4c3a` the T0 fix.
- **Not pushed.** `origin/main` is 2 commits behind local. Push when you're ready
  (`git push`; remote is HTTPS `github.com/ImTani/clipd`, gh authed `ImTani`).

---

## 2. What T0 changed (READ THIS — it overturns a devpack assumption)

**The frozen spec's §6.1 CQP mandate is unachievable on this hardware, and the handover's
assumed root cause was wrong.** Measured on the Nitro (RTX 4050, MF NVENC H.264 MFT):

- `AVEncCommonQuality` (the 0–100 knob the old code mapped CQ onto) is a **NO-OP** —
  sweeping it 55→85 moved bitrate <2%. Recalibrating that formula (the planned fix) would
  have done nothing.
- `AVEncVideoEncodeQP` (true CQP) is **rejected** (`E_INVALIDARG`) in every rate-control
  mode. There is no QP lever.
- `MF_MT_AVG_BITRATE` is the **only** lever and is precise (16M→16.4 Mbps, 60M→60.4).

**The fix (shipped):** the encoder now targets a bitrate via **PeakConstrainedVBR** —
average = the §6.2 table (`spec_constants::encoder::video_target_bitrate_bps`: 1080p60=16,
1440p60=26, 4K60=50 Mbps, fps-scaled), peak = 1.5× average. The §6.2 table is now the
single source of truth (the ring byte cap `est_bitrate_bps` delegates to it), and the
peak-cap invariant (`PEAK_BITRATE_HEADROOM ≤ BYTE_CAP_HEADROOM`, unit-tested) means a
peak-capped stream can never blow the ring byte budget. Measured content-adaptivity at the
16 Mbps default: **mandelbrot 16.4 / testsrc2 15.5 / static desktop 6.0 Mbps** — in-band on
active content, cheap when idle. Full detail: `DECISIONS.md` "2026-07-07 — T0 resolution".

**Consequence for later work:** §6.2's **auto-QP-relief** rule is now conceptually obsolete
(no QP to relieve). It was already deferred/unimplemented. In bitrate mode the equivalent
response to sustained byte-cap eviction is *lowering the target bitrate* — decide this in
Slice A or M6, don't implement it against the dead QP concept.

**New mechanism you'll build on:** `EncoderOverrides` (in `src/encode/mft_h264.rs`) +
hidden `record`/`buffer` hooks `--encode-rc-mode|--encode-quality|--encode-qp|
--encode-avg-bitrate|--encode-max-bitrate`. All absent = shipping path. These are how
Slice A's **named quality tiers** (Efficient/Default/High/Max) get wired — as multipliers
over `video_target_bitrate_bps`. Harness + docs live in **`tools/calibration/`**.

---

## 3. DO THIS NEXT — Slice A (M7 "the satellite", tasks A1–A8)

Full task text in `M7-M8-PLAN.md` §3. Order = devpack priority (meters before cosmetics),
branch per task. Then friends-beta v0 (`just dist` zip), then Slice B (M8′ 4-track audio,
B1–B7), then M6 closes on beta evidence.

- **A1 — config rewrite path + schema v2.** `toml_edit` for unknown-key/comment
  preservation (whitelist callout required in the task summary — it's approved, effective
  when this lands). `config_version 1→2` migration (v1 files load, gain new-key defaults).
  New keys: `encode.quality = "efficient"|"default"|"high"|"max"` (→ bitrate-target
  multipliers over the T0 `video_target_bitrate_bps`; **NO raw-Mbps mode**; raw CQ/bitrate
  stays TOML-only), `encode.resolution` (native|1440|1080|720; subsumes `max_height`),
  `[audio.tracks]` + `[[audio.vc_apps]]` (defaults per plan §2). UI writes ONLY through
  this path (same as `--check-config`).
- **A2** egui/eframe settings window skeleton (satellite: lazily created from the tray;
  engine must run fully if it never opens; `ui` depends on engine types, never reverse).
- **A3** VU meters · **A4** status strip · **A5** settings editor · **A6** press-to-bind
  hotkeys · **A7** recent-clips list · **A8** `just dist` beta zip.

The T0 groundwork means A1's quality tiers have a real, calibrated bitrate target to map
onto — don't reintroduce CQ as the control.

---

## 4. Research facts the next session must not re-derive (sourced in M7-M8-PLAN §5)

Carried forward — all still relevant for Slice A/B:

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
  (audio in an Electron child). Ships as TOML table: Discord/PTB/Canary (P0), Vesktop/Legcord/
  TS3/TS6/Mumble (P1), Steam voice + Game Bar (P2). Skype + Guilded are DEAD — never add.
  In-game voice (Vivox/EOS/Steamworks: Valorant/Fortnite/Apex/LoL) renders INSIDE the game
  process — never separable → LIMITATIONS.md. Only Medal auto-detects Discord today (a
  differentiator).
- **4-track layout (Slice B):** mix FIRST (track 1; one-track players/CapCut/Discord/YouTube
  use exactly it), then game / voice-chat / other-system / mic when `separate_tracks=true`;
  mix+mic when false. All tracks flagged enabled. "Other system" contains VC too (API can't
  express system−game−VC) — accepted, documented.
- **Container:** MKV folklore doesn't apply; fMP4-on-disk quirks solved by the approved
  OBS-Hybrid appended-`moov`-on-save (§4 amendment). Uploads flatten to one track; editors
  read all enabled tracks.
- **Competitor defaults:** Steam 12 Mbps default tier / NVIDIA ~20–50 computed / Medal 3–100
  slider; only OBS exposes CQP. Resolution UX: "Source (recommended)" + downscale tiers, hide
  options above source (rides our existing `encode.max_height` canvas).

---

## 5. Environment facts (this machine = the Nitro V15 test box)

| Thing | Value |
|---|---|
| Repo root | `X:\Projects_X\clipd` |
| Rust | stable **1.95.0**, `x86_64-pc-windows-msvc` (pinned) |
| `CARGO_HOME` | `X:\cargo` (`X:\cargo\bin` NOT on PATH — prepend `$env:Path = "X:\cargo\bin;$env:Path"`) |
| GPU | RTX 4050 Laptop (Ada NVENC) + Intel iGPU; Optimus. Primary **1080p** on the dGPU |
| Default audio | Realtek Headphones (render) + FIFINE mic (capture), both 48 kHz |
| ffmpeg/ffplay/ffprobe | 7.0.1 on PATH (ffplay is a **chocolatey shim** — see gotchas) |
| Config file | none by default — `%APPDATA%\clipd\config.toml` by hand. Hotkeys: save `Ctrl+Alt+S`, record `Ctrl+Alt+F9` |
| Log file | `%LOCALAPPDATA%\clipd\logs\clipd.log.<date>` |
| Git remote | `origin` HTTPS (`github.com/ImTani/clipd`), gh authed `ImTani`. Local `main` is 2 ahead — push when ready |
| Zombie procs | `Get-Process clipd,ffplay -EA SilentlyContinue | Stop-Process -Force` between runs |
| Local cruft (gitignored) | `ram.csv` (M5 RAM-budget log, Jul 4–5 — left in place; delete if you don't need it). Stray `clipd_*.mp4` T0 evidence clips were cleaned up this session |

---

## 6. Gotchas carried forward (+ new T0 ones)

**New from T0:**
- **Exclusive fullscreen starves WGC monitor capture** → no frames → the encode thread
  blocks on `size_rx.recv()` → `stop_and_join` hangs forever. If you drive on-screen test
  content, use a **borderless window**, never `ffplay -fs`. (Cost me a 30-min hang.)
- **Chocolatey `ffplay` is a shim** that spawns the real ffplay and exits — a `Start-Process
  -PassThru` handle points at the dead shim, so kill ffplay **by name**, not by that PID.
- **`--encode-*` hooks contaminate "no bitrate target" tests:** setting any override
  suppresses the shipping PCVBR default (`EncoderOverrides::is_default()` gates it), so a
  probe with only `--encode-quality` sends NO avg target — intended, but know it.
- PCVBR peak cap (1.5× avg = 24 Mbps @ 1080p) was never approached even by mandelbrot
  (hardest content hit 16.4), so it doesn't clamp real quality — it's pure byte-cap safety.

**Carried:**
- **Run `buffer` via `just run buffer`** (NOT `just run -- buffer`). Tray mode: Enter does
  not quit — use tray Quit. New icons hide in the Win11 "^" overflow flyout.
- **`common-controls-v6` breaks binary load** (DECISIONS "M5 T2 fixup") — keep `tray-icon`
  default-features off; `tests/smoke.rs` guards it. Themed controls later = a manifest via
  build script, NOT the feature flag.
- **Satellite law (Slice A):** engine must never depend on/block on `ui`; window lazily
  created; UI writes config ONLY through the versioned-TOML path.
- `--simulate-device-loss` is headless by design (doesn't exercise the tray Warning). `clip
  shorter than requested (§4.2)` on a young buffer is EXPECTED.
- Carried M1–M4: `Closed` doesn't fire on window close → `IsWindow` poll; fixed canvas
  letterboxes odd aspects; `windows` 0.62 COM interfaces `!Send`/`!Sync`; only the `Win32_*`
  features actually called; `unsafe` confined to COM/D3D/MF/OS wrappers; **never claim a HW
  path works until the machine says so.**

---

## 7. Quick command reference

```
$env:Path = "X:\cargo\bin;$env:Path"          # first, always
just check            # fmt + clippy -D warnings + cargo check
just test             # nextest, 173 tests (incl. smoke.rs loading the real exe)
just release          # stripped release vs 10 MB budget (2.51 MB)
just run buffer                               # tray shell (M5)
just run -- buffer --record-secs 8            # headless auto-record self-test
just run -- record --seconds 15               # timed record (headless)
just verify clip.mp4                          # ffprobe assertion script
ffprobe -v error -select_streams v:0 -show_entries stream=bit_rate <clip>   # bitrate check

# T0 calibration harness + hidden encoder hooks (tools/calibration/README.md):
powershell -ExecutionPolicy Bypass -File tools\calibration\t0_sweep.ps1
just run -- record --seconds 15 --out c.mp4 --encode-rc-mode pcvbr --encode-avg-bitrate 16000000
```
