# Session Handover — A1 (config schema v2) DONE; A2 (egui settings window) is next

> Onboarding note for the next session. `CLAUDE.md` and the `clipper-devpack/devpack/`
> docs are normative and override anything here. `02-AV-SYNC-SPEC.md` (frozen) overrides
> everything EXCEPT the dated `DECISIONS.md` amendments: the two M4-2 entries, the three
> **2026-07-07** entries (§2.2 process-loopback QPC, §2.5 track layout, §4 hybrid-moov),
> the **"T0 resolution"** entry (§6.1 CQP → bitrate-target VBR), and now the
> **"A1" entry** (config schema v2 / quality tiers / `toml_edit`). Read **`M7-M8-PLAN.md`**
> (repo root) — it is the working plan for this whole phase; you are at Slice A task **A2**.

**Written:** 2026-07-07, after **A1 was implemented, self-reviewed, and merged to `main`.**
This session wrote the config schema v2 + the format-preserving rewrite path — the
foundation the settings UI (A2–A5) writes through.

---

## 1. Code state

- **M0–M5 + T0 + A1 merged on `main`.** Working tree clean. **186 tests** (nextest), was
  173 (+13 A1: config +11, spec_constants +2). `just check` (fmt + clippy -D warnings +
  check) green. Release builds at **2.57 MB** (2,698,240 bytes) vs the 10 MB budget
  (+66 KB from T0's 2.51 MB — that's `toml_edit`).
- Last commits: `2a034cc` Merge a1-config-schema-v2 → `74581db` the A1 feat commit.
- **`main` is 2 ahead of `origin/main`** (the A1 feat + merge). T0 and everything before it
  is ALREADY on origin (`origin/main` = `5ac1040`). **Not pushed.** Push when ready
  (`git push`; remote HTTPS `github.com/ImTani/clipd`, gh authed `ImTani`).

---

## 2. What A1 changed + the pain points (READ before touching config)

**Config is now schema v2.** `config.rs` reads via serde into the typed `Config`, migrates
v1→v2 in memory, and — new — writes back via `toml_edit` preserving comments + unknown keys.
Full rationale: `DECISIONS.md` "2026-07-07 — A1". The load-bearing facts:

- **Quality tiers are BITRATE MULTIPLIERS, not CQ.** `encode.quality =
  efficient|default|high|max` → `0.6 / 1.0 / 1.5 / 2.0` × the T0 target (1080p60 = 9.6 / 16
  / 24 / 32 Mbps). **The M7-M8-PLAN §3 A1 text literally says "per-vendor CQ map" — that is
  WRONG post-T0 (CQP is a no-op on NVENC-MF). Do NOT reintroduce CQ.** The multiplier lives
  as the trailing `quality_mult: f64` arg on `spec_constants::encoder::video_target_bitrate_bps`,
  `video_peak_bitrate_bps`, and `ring::est_bitrate_bps`.
- **The multiplier MUST feed BOTH the encoder target AND the ring byte cap.** This is the
  non-obvious coupling: if only the encoder scaled, High/Max streams would be evicted by a
  byte cap sized for Default. `A5`'s "estimated Mbps / RAM" feedback should read from the
  same `video_target_bitrate_bps(w,h,fps, quality.multiplier())`.
- **`encode.resolution = native|1440|1080|720`.** `native` → the historical 2160 cap (zero
  behavior change; decided). Raw `max_height` survives as `Option<u32>` advanced override
  (TOML-only, omitted from output when unset). `EncodeConfig::effective_max_height()` is the
  single value the canvas is built from — **use it, not `max_height` directly** (I already
  rewired the two `BufferParams` fill sites in `main.rs`).
- **`[audio.tracks]` + `[[audio.vc_apps]]` are SCHEMA-ONLY in A1.** Parsed/validated/round-
  tripped, seeded with the Discord P0 default, but **the engine does not read them yet** —
  Slice B (B2/B3) wires them and adds the full P1/P2 VC table. Don't be surprised they do
  nothing.
- **Writes go through `Config::write_atomic(path)` ONLY** (satellite law / pitfall 30).
  The A5 settings editor calls this; there is no second config representation.

### Pain points I hit (so you don't re-derive them)

1. **`toml` 1.x DROPPED `toml_edit`** — it's a fully separate crate now (added `toml_edit
   0.25.12`, default features `display`+`parse`, **no `serde` feature** — fields are applied
   manually). It was NOT transitively available. Both carry a `+spec-1.1.0` version suffix.
2. **The scalar-after-subtable footgun does NOT bite.** I worried that inserting a missing
   scalar (e.g. `mic`) into an `[audio]` table that already has `[audio.tracks]` would append
   it AFTER the subtable header → invalid TOML. **Verified empirically it does not** —
   `toml_edit` keeps leaf keys ahead of subtable headers. Two tests lock this
   (`rewrite_partial_v2_with_subtable_before_missing_scalars_stays_valid`,
   `rewrite_v1_audio_section_with_scalars_stays_valid`). Don't re-investigate.
3. **Comment preservation on a CHANGED value** needs care: `table[k] = value(x)` strips the
   value's inline `# comment`. The `set_val` helper clones the existing value's decor and
   restores it after overwriting — reuse that pattern for any new keys.
4. **serde container-level `#[serde(default)]` fills missing fields from the CONTAINER's
   `Default`**, not each field's own default. That's why a file missing `vc_apps` gets the
   seeded Discord entry (from `AudioConfig::default()`), not an empty `Vec`. Know this if you
   add audio keys.
5. **TOML field ORDER matters for the serde serializer**: scalar fields must precede
   table/array-of-table fields in a struct. `AudioConfig` declares `tracks` then `vc_apps`
   LAST for this reason. Adding a scalar after them breaks `to_toml()`.
6. **`Option<u32>` needs `#[serde(default, skip_serializing_if = "Option::is_none")]`** so
   the advanced `max_height` override stays absent from output when unset.
7. **clippy `derivable_impls`**: once every `EncodeConfig` field had a default, the manual
   `impl Default` became a clippy error — use `#[derive(Default)]`. (`Codec` is NOT `Copy`
   while the new `Quality`/`Resolution` are — a mixed loop trips a move error; borrow it.)
8. **The `ecc:rust-reviewer` subagent runs in a sandbox with no `cargo`/`.cargo\bin`** — it
   can only do static review, not run the gate. Do the gate yourself.

---

## 3. DO THIS NEXT — A2 (egui settings-window skeleton)

Full task text in `M7-M8-PLAN.md` §3. Order within Slice A = devpack priority (meters
before cosmetics), branch per task (`a2-settings-window` etc.).

- **A2 — settings window skeleton** (egui/eframe). **Satellite law is the hard part**:
  lazily created from the tray; the engine must run fully if the window never opens; enforce
  the dependency direction `ui → engine` and NEVER the reverse (module visibility). Cold-open
  **< 300 ms** budget. `eframe`/`egui` are already CLAUDE.md-sanctioned for the UI module —
  they enter the build here (expect the binary to jump ~5–6 MB; still under 10). **First UI
  task ⇒ first `eframe`/`egui` dep add**: standalone-ish, note it in the task summary.
- Then **A3** VU meters (highest-value UI element, ships before cosmetics) · **A4** status
  strip · **A5** settings editor (writes via the A1 `Config::write_atomic` path; shows
  derived Mbps/RAM from `video_target_bitrate_bps × quality.multiplier()`) · **A6** press-to-
  bind hotkeys · **A7** recent-clips list · **A8** `just dist` beta zip.
- After A8: friends-beta v0 (2-track, full UI), then Slice B (B1–B7, 4-track audio), then M6
  closes on beta evidence.

`M7 acceptance` (from 08): cold-open < 300 ms; 2 h open-window soak, zero engine stalls
attributable to UI.

---

## 4. Research facts the next session must not re-derive (sourced in M7-M8-PLAN §5)

Carried forward — all still relevant for A2–A8 / Slice B:

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
| GPU | RTX 4050 Laptop (Ada NVENC) + Intel iGPU; Optimus. Primary **1080p** on the dGPU |
| Default audio | Realtek Headphones (render) + FIFINE mic (capture), both 48 kHz |
| ffmpeg/ffplay/ffprobe | 7.0.1 on PATH (ffplay is a **chocolatey shim** — see gotchas) |
| Config file | none by default — `%APPDATA%\clipd\config.toml` by hand. Hotkeys: save `Ctrl+Alt+S`, record `Ctrl+Alt+F9` |
| Log file | `%LOCALAPPDATA%\clipd\logs\clipd.log.<date>` |
| Git remote | `origin` HTTPS (`github.com/ImTani/clipd`), gh authed `ImTani`. `origin/main` = `5ac1040`; local `main` 2 ahead (A1) — push when ready |
| Zombie procs | `Get-Process clipd,ffplay -EA SilentlyContinue \| Stop-Process -Force` between runs |
| Local cruft (gitignored) | `ram.csv` (M5 RAM-budget log — delete if unneeded) |

---

## 6. Gotchas carried forward (+ new A1 ones)

**New from A1** (details in §2 pain points):
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
  default-features off; `tests/smoke.rs` guards it. Themed controls later = a manifest via
  build script, NOT the feature flag. **(Relevant to A2: egui window styling should not need
  this, but watch the manifest story.)**
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
just test             # nextest, 186 tests (incl. smoke.rs loading the real exe)
just release          # stripped release vs 10 MB budget (2.57 MB)
just run buffer                               # tray shell (M5)
just run -- buffer --record-secs 8            # headless auto-record self-test
just run -- record --seconds 15               # timed record (headless)
just run -- --check-config [PATH]             # print effective config (now schema v2)
just verify clip.mp4                          # ffprobe assertion script
ffprobe -v error -select_streams v:0 -show_entries stream=bit_rate <clip>   # bitrate check

# Quality-tier spot check (A1): a High-tier clip should measure ~24 Mbps @ 1080p60 vs
# Default's ~16. Set `[encode] quality = "high"` in %APPDATA%\clipd\config.toml, then:
just run -- record --seconds 15 --out c.mp4

# T0 calibration harness + hidden encoder hooks (tools/calibration/README.md):
just run -- record --seconds 15 --out c.mp4 --encode-rc-mode pcvbr --encode-avg-bitrate 16000000
```
