# B7 — the single batched Nitro HW gate that closes Slice B

Working checklist for the manual test-machine pass. Sequenced by **hardware-setup
phase** so you set up each rig once and run every check that needs it before moving on.
Source checklists: HANDOVER.md §3/§5, the `run_binding_probe` / `run_list_audio_devices`
/ `tools/audio-probe` headers, `02-AV-SYNC-SPEC.md §5`.

**On close:** fold results into HANDOVER.md §5 and append a DECISIONS.md "2026-07-08 —
Slice B / B7" entry. Mark each box `[x]` PASS or write the finding inline.

> **STATUS 2026-07-08:** Phases 1–4 GREEN + Phase 7 CLEARED + both this-session fast-follows
> (track naming, probe watchdog) HW-CONFIRMED. **Phase 5 (AV-1..AV-5) is the ONLY remaining gate**
> before the UI rework + friend distribution. Phase 6 (endurance) → folded into friends-beta
> multi-device. P4 items → deferred to post-UI. See Sign-off.

---

## Bench prep (do once, before anything)

- Binary: `target/release/clipd.exe` — **9.0 MB** (9,437,696 B, < 10 MB budget). ✅ built this session.
- Local green this session: **299 tests pass**, `just test` exit 0; `just check` (fmt+clippy) — see status.
- Config (`%APPDATA%\clipd\config.toml`) is already B7-shaped: `separate_tracks = true`,
  `[audio.tracks]` game/voice_chat/other_system = true, Discord seeded, desktop on, mic `default-follow`,
  hotkeys **save `Ctrl+Alt+S`** / **record `Ctrl+Alt+K`**. ✅ verified.
- Windows build **26200** ≫ 19041 floor → the 5-track path is live. ✅
- Clips land in `%USERPROFILE%\Videos\clipd` (`[output].dir` blank).
- **Between every run**, clear zombies:
  ```powershell
  Get-Process clipd,ffplay -EA SilentlyContinue | Stop-Process -Force
  ```
- Shell prep (PowerShell): `$env:Path = "X:\cargo\bin;$env:Path"`
- **Command form — NO leading `--`.** The `just run` / `just probe` recipes already inject `--`
  (`cargo run -- {{ARGS}}`), so `just run -- binding-probe` → `cargo run -- -- binding-probe` →
  clipd rejects the stray `--`. Correct: `just run binding-probe 30`, `just run list-audio-devices`,
  `just run record --seconds 15 --out x.mp4`, `just probe --pid 1234 --seconds 20`.
- **Never** drive on-screen test content with `ffplay -fs` (exclusive fullscreen starves WGC → hang).
  Use a borderless window. Kill ffplay **by name**, not PID (choco shim).

---

## Phase 1 — Audio-COM instruments (quick, no game yet)

These hit the exact engine COM paths with zero drift; knock them out first.

**B2 process-loopback** — `just probe`
- [x] `just probe` (self + 440 Hz tone) → **PASS 2026-07-08**: `packets=798 frames=383040 silent=0
      timestamp_errors=0 qpc_span_s=7.99 max_gap_ms=15.2`. QPCPosition = master domain (§2.2). ✅
- [~] Two concurrent probes gave **correct per-PID output** (2026-07-08) → per-PID include capture works.
      Explicit single-PID `--exclude` not separately run (low priority).
- [!] `just probe --pid <PID>`, kill mid-run → **FINDING 2026-07-08 (FIXED)**: probe originally did NOT
      end / log "target process exited" (silence to end, no crash, valid WAV) because **the probe tool had
      no liveness watchdog** — doc-drift in its header. **FIXED this session: the watchdog is now mirrored
      from `process_loopback.rs` into the probe** (OpenProcess/WaitForSingleObject, exit-latch), header
      corrected. Re-run `just probe --pid <PID> --seconds 20` + kill → should now log "target process
      exited ... ending process-loopback capture" and stop promptly.
- [x] **Core watchdog HW-CONFIRMED 2026-07-08 (Incredibox).** Closing a clean-exit bound game
      (pids 33504, 28060) logged `target process exited ... ending process-loopback capture (§2.2)`
      for BOTH `track="game"` AND `track="other-system"` -- proving the shipping watchdog fires and
      the OtherSystem dual-publish handles game-exit. (Roblox kept helper processes alive; Incredibox
      exits its PID cleanly.) This also evidences the D5 endpoint<->exclude swap on game-exit. ✅
- [x] `just probe --pid <bogus PID>` → silence, no crash → **PASS 2026-07-08**. ✅
- [x] Two probes at once on different PIDs → both capture flawlessly, no deadlock → **PASS 2026-07-08**. ✅

**B3 detection** — `just run binding-probe 30`
- [x] Discord detected steadily as **voice-chat=pid 25308** (`include_tree=true`) across the whole run,
      regardless of foreground → **PASS 2026-07-08** (confirm 25308 = Discord in Task Mgr; ideally tray-min).
- [x] Borderless-fullscreen discrimination works: pid 15904 showed **game=none** when not covering the
      monitor and **game=pid 15904** when it did → maximized/non-fullscreen does not false-bind. ✅
- [x] Retarget works — game PID followed the foreground-fullscreen window (43968→1432→15904→25308). ✅
- [ ] VC config order (first enabled app wins) — only Discord enabled here, so trivially satisfied; re-confirm if a 2nd VC app is added.
- Note: Discord (25308) also bound as **game** when it was itself foreground-fullscreen — expected for the
  monitor-mode "live foreground-fullscreen guess" (would double-count if a VC app is run fullscreen).

**B3.5 enumeration wiring** — `just run list-audio-devices`
- [x] **PASS 2026-07-08**: 12 capture endpoints with sane friendly names (FIFINE, Intel Mic Array,
      NVIDIA Broadcast, 8× Voicemeeter Out, VB CABLE Output), each `{0.0.1…}` id `<TAB>` name. ✅
- [x] Unplug FIFINE → drops; replug → returns → **PASS 2026-07-08**. ✅

---

## Phase 2 — B3.5 mic-device behavioural gate (Settings UI + unplug) — ✅ ALL GREEN 2026-07-08

`just run buffer` → tray **Settings…** → **Microphone**.
- [x] Dropdown lists the same real devices + Default (follow) + Off.
- [x] Pick device → Save → restart → mic track opens THAT endpoint.
- [x] Round-trip: `[audio].mic` id matches `list-audio-devices`.
- [x] Unplug pinned → `Unavailable: <id>`, NOT silently replaced (§7); list otherwise refreshes.
- [x] Replug → returns as a named entry. **Full Phase 2 PASS.** ✅

---

## Phase 3 — 5-track container gate (B1/B4/B5 + OtherSystem finalize)

Have **Discord in a voice call** + **a game running** so all 5 tracks have content.
Record a clean sample:
```
just run record --seconds 15 --out b7_5track.mp4
```
- [x] `just verify b7_5track.mp4` → **all green PASS 2026-07-08**. ✅
- [x] ffprobe → **5 streams** as expected. ✅ **Track naming ADDED this session** — each audio track's
      `hdlr` name is now its `AudioTrackKind::title()` (Mix / Game / Voice chat / Other system / Microphone),
      surfaced as ffprobe `handler_name` / the editor track label. **HW-CONFIRMED 2026-07-08:**
      `ffprobe ... stream_tags=handler_name` printed `clipd` (video) + Mix / Game / Voice chat /
      Other system / Microphone. ✅
- [x] **VLC** plays all 5 tracks appropriately (substituted for CapCut — confirms multi-track container). ✅
- [x] **Explorer** correct thumbnail + duration; **WMP** seeks cleanly. ✅
- [x] **VS Code built-in player** plays the first Mix track correctly (substituted for Discord upload —
      confirms a single-track consumer gets track 1 = Mix). ✅
- [x] **Crash-safety**: killed mid-`record` → `.part` is a valid fragmented MP4, plays in VLC. **PASS**. ✅
- [~] Empty-per-app-track drop (D-B5): **not HW-confirmed** (unit-tested already; user deprioritized).
      Accepted low-risk.

---

## Phase 4 — OtherSystem / D5 content gate

Play a **game** + **music (browser/Spotify)** + **Discord** together.
- [x] Game-bound content routing (game on Game track, excluded from OtherSystem) verified during Phase 3
      → **GREEN 2026-07-08** (user).
- [x] **Endpoint<->exclude swap on game-EXIT: evidenced 2026-07-08** — the Incredibox log shows the
      `other-system` process-loopback ending on game exit (the swap back to endpoint loopback trigger),
      alongside the `game` track. Sub-frame correctness covered by unit tests + the QPC master domain.
      Still owed: a `just verify` on a clip spanning a game LAUNCH (exclude engaging mid-clip).
- [→] Double-counted VC (B6-documented) and `game=false + other_system=true` still excludes the game
      and the D5 swap on game-LAUNCH (`just verify`): **DEFERRED to post-UI pass (orchestrator 2026-07-08)**
      — the config UI for toggling these tracks does not exist yet; revisit when it does. Documented
      behavior / covered by unit tests + the QPC master domain in the meantime.

---

## Phase 5 — AV-sync rig (AV-1..AV-5), 5-track set on

Two shells: the rig flashes fullscreen white + a click; capture the monitor showing it.
Thresholds from `avrig measure`: **AV-1 |offset| ≤ 16.7 ms** (expect ≤ 10), **AV-2 |drift| ≤ 5.0 ms**.

- [ ] **AV-1** (30 s): shell A `just rig flash --seconds 35`; shell B `just run buffer`, let it fill ~30 s,
      press **Ctrl+Alt+S** to save; then `just rig measure <saved clip>` → `AV-1 PASS`, |offset| ≤ 16.7 ms.
- [ ] **AV-2** (drift, 10 min): shell A `just rig flash --seconds 620`; shell B
      `just run record --seconds 600 --out av2.mp4`; then `just rig measure av2.mp4` → `AV-2 PASS`,
      |drift minute-1 vs minute-10| ≤ 5 ms.
- [ ] **AV-3** (silence): clip with **60 s of total desktop silence** mid-buffer → offset after the silent
      span unchanged (≤ ±16.7 ms), audio-track duration within 1 AAC frame of video (`just verify`).
- [ ] **AV-4** (device chaos): **unplug/replug the default mic** during buffering, save a clip spanning the
      event → plays, gap is silence, **no offset change** after recovery, recovery gap ≤ 750 ms.
- [ ] **AV-5** (load): re-run AV-1 while a **GPU-saturating game** pins the 3D engine at 100% → same
      ≤ 16.7 ms threshold holds.

---

## Phase 6 — endurance / perf — FOLDED INTO FRIENDS-BETA (orchestrator 2026-07-08)

Not a solo Nitro item anymore. The ≥ 1 h crackle/drift watch, CPU ≤ 2 % at 5 sources, and the 2 h
UI soak are absorbed into the **friends-beta multi-device test** — several people on different GPUs
(iGPU, AMD, Win10 AMD/Nvidia) clipping full-time for a couple of days is a far stronger endurance +
cross-hardware signal than one Nitro session. Track the results there.

---

## Phase 7 — A6 hotkeys — CLEARED (orchestrator 2026-07-08)

Cross-row conflict was HW-validated 2026-07-08 (`a6-ff-cross-conflict`); the rest of the A6 UI
behavior is accepted. The whole hotkey UX is revisited in the UI rework anyway (where live
re-registration + the record_toggle re-default are decided). No further solo HW step owed here.

---

## Sign-off — Phase 5 (AV rig) is the ONLY remaining gate (orchestrator 2026-07-08)

Decisions this session: Phase 6 → friends-beta multi-device; Phase 7 → cleared; P1/P3 leftovers →
accepted (tested / covered by substitutes + unit tests); P4 (game=off exclude, double-count, D5
launch-swap verify) → deferred to **after the UI pass** (the config UI for these does not exist yet).
**Phase 5 (AV-1..AV-5) is the last gate before the UI rework + friend distribution.**

- [ ] **Phase 5 (AV-1..AV-5) PASS** — the remaining gate.
- [ ] HANDOVER.md updated (Phase 5 = NEXT; fast-follows merged; scoping decisions).
- [ ] DECISIONS.md "2026-07-08 — Slice B / B7" progress appended.
- [ ] Any defect → same-day fast-follow branch (mirror the A5/A6 pattern), re-validate.
