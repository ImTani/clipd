# UI-Redesign HW Test Runbook (M7 UI + P-series + F-series)

The hardware acceptance checklist for the `ui-redesign-research` work now merged to `main`:
the settings-window redesign (T-batch), the save-confirmation shell rebuild (P1a–P1c), and the
two HW-findings batches (F1–F8). These are the checks CI **cannot** run — they need a real GPU,
real audio endpoints, a real tray/desktop, and a human to alt-tab, hot-plug, and swap devices.

> Run on the Nitro V15. Record outcomes under `testlogs/<date>/SUMMARY.md`. Nothing here is
> "verified" until the machine says so — the agent built and locally-greened all of it (`just
> check` + `just test`, ~351 tests), but could not see a render or drive the shell. The frozen
> `02-AV-SYNC-SPEC.md` is the source of truth for the sync numbers.

---

## 0. Before you start (once per session)

```powershell
$env:Path = "X:\cargo\bin;$env:Path"
just release            # target\release\clipd.exe, prints size vs the 10 MB budget
```
Run the release build (the debug build perturbs drift/load). Launch it; the tray icon appears.

`just check` and `just test` are green in CI — this runbook is only the un-automatable rest.

---

## 1. Tray — FULL M5 re-verification (P1a: single visible icon on our own WNDPROC)

P1a replaced the `tray-icon` crate with one window + one visible `Shell_NotifyIcon` we own, and
folded the balloon onto it. This is proven-M5 surface rebuilt — **re-check ALL of it, assume
nothing.**

- [ ] **Exactly one clipd tray icon** at all times; **no double-icon flicker** during startup.
- [ ] Right-click AND left-click both open the menu; every item works:
      Save clip · Pause buffering (checkmark toggles, icon → amber) · Start/Stop recording
      (label flips with the live state) · Settings… (opens the window) · Open clips folder ·
      Start with Windows (checkmark reflects the HKCU Run key) · Quit.
- [ ] Tooltip + state glyph colour track the state (buffering lavender / paused amber /
      warning / error).
- [ ] Global hotkeys still fire with the tray live (save + record).
- [ ] **Clean quit:** menu → Quit exits; the log shows `bad_qpc=0 ts_violations=0`, no dead
      thread, no orphan icon left in the tray.

## 2. Save-confirmation channels (P1b sound · P1c pill · F2 recording · F3 preference)

- [ ] **Balloon now shows** (visible icon, `NIS_HIDDEN` dropped): save on the desktop → the
      notification appears; clicking it opens the clip folder; a failure balloon opens the log
      folder. When suppressed (gaming DND), confirm it still lands in the Action Center.
- [ ] **Sound (P1b):** a short blip on a successful save (default on). Toggle "Play a sound when
      saved" off in Settings → it stops (live). A custom `.wav` via the Advanced "Save sound"
      field plays instead (F7).
- [ ] **Pill (P1c):** a corner pill on the active monitor — "Clip saved · N s" (accent, ~3 s) /
      "Clip NOT saved — …" (red, ~6 s), fade in/out, click-through (doesn't steal focus). It does
      NOT draw over an *exclusive*-fullscreen game (borderless shows it).
- [ ] **F3 preference:** Settings ▸ "When a clip saves, show" = Notification / Pop-up / Both.
      Success honours the choice; **a FAILED save always shows BOTH** regardless (fails-loudly).
- [ ] **F2 recording:** finalize a timed/hotkey recording → the SAME confirmation fires, worded
      "**Recording saved · N min**" (toast + sound + pill). A record failure says "Recording NOT
      saved — …".

## 3. Settings UI (F4 · F5 · F6 · F7)

- [ ] **F4 mic refresh:** open Settings, then plug in a mic. Open the Microphone dropdown → the
      new device appears **without reopening the window** (re-enumerates on open).
- [ ] **F5 layout:** shrink the window to its minimum. **Browse… stays fully visible** (the
      "Save clips to" and "Save sound" fields shrink/scroll their text); no row clips.
- [ ] **F6:** the "Debug information" expander's left edge lines up with the cards above it (no
      vertical indent line).
- [ ] **F7 "Record":** Essentials shows a plain "Record" picker. On this (single-monitor?)
      machine it shows only **This screen / The focused window** — no monitor jargon. Switch it →
      the restart banner appears; restart applies it. On a multi-monitor box, per-screen choices
      appear.
- [ ] **F7 tracks:** turn on "Record separate audio tracks" (Advanced) → the per-source toggles
      (Game / Voice chat / Other apps & system) appear nested; hidden when it's off. Each change
      raises the restart banner.
- [ ] **F7 cursor:** "Show the mouse cursor" toggles → restart banner → applied after restart.

## 4. Engine save-correctness — F1 (idle track) + F8 (sticky binding)

**F1 — the original complaint (44 s where 60 s expected).**
- [ ] With `separate_tracks = true`: play a game, alt-tab to the settings window (so no game is
      foreground), then save. The clip must span the **FULL configured window** — not truncate to
      when the game was last foreground. Run `just verify <clip>`: full window, all tracks end
      within one AAC frame.

**F8 — sticky game binding + edge-debounce.**
- [ ] Bind a game (foreground-fullscreen), then **alt-tab repeatedly** between it and other
      windows for ~30 s → the log shows **ZERO game/other-system retargets** while the game lives
      (was 10+/min before). Game audio keeps being captured while tabbed out.
- [ ] **Kill** the game → it unbinds within one poll (~0.6 s).
- [ ] Launch **game B while A is alive** and bring B fullscreen → the binding retargets to B only
      after it holds ~1.2–1.8 s (a fullscreen *flash* does not steal it).

## 5. Buffer honesty (F7) + `clear_after_save` default

- [ ] Fresh start / after a save: the settings header reads **"Filling up — keeping the last N s
      of your M s replay so far."** (neutral) while the buffer climbs — the normal fill-up must
      NOT read as a shortfall.
- [ ] High Quality + a long replay length so the byte cap binds: the header reads **"Keeping the
      last ~N s of your M s replay — high quality uses more memory; …"** (capped). Tray tooltip
      shows `· holding N/M s (capped)`.
- [ ] At/near target: no honesty line; tooltip has no suffix.
- [ ] A **fresh config** (`clipd --check-config` on a new profile) shows `clear_after_save =
      false`; a quick second save is now the full window (not short). An existing config keeps its
      explicit value.

## 6. Still-owed AV items (from the P-series HW pass)

- [ ] **Mismatched-format mic swap (§7):** mid-buffer, swap FIFINE → NVIDIA Broadcast (or the
      Realtek array) — a different sample rate / channel count. The clip spans the swap; listen
      for rate/pitch artifacts across the seam (the §7 rebuild must resample/upmix through the
      48 k pipeline). Follow-Windows-default ↔ pinned counts as a swap.
- [ ] **Banner census:** on the Essentials screen, confirm only **Quality / Resolution / Frame
      rate / mic on↔off / game-&-app-sound / separate-tracks / per-source tracks / Record source /
      cursor** raise the restart banner; the T2b-live fields (replay length, output folder,
      hotkeys, mic *device* swap, sound/pill/show prefs) never do.
- [ ] **AV-2 drift spot-check:** a long clip's audio/video stay within the §5 drift bound
      (`just verify`).

## 7. E2E-FINAL — acceptance for the F1+F8 chain against the original complaint

- [ ] Game running → **alt-tab to settings** → **swap the mic mid-buffer** → **sing** → **save**.
      The single clip must:
      - span the **FULL configured window** (F1 — no idle-track truncation),
      - contain **real game audio throughout** (F8 sticky — captured even while tabbed out),
      - contain the **mic-swap seam** and **voice on both sides of it** (§7 + §2.3),
      - pass `just verify` (monotonic PTS, CFR, tracks within one AAC frame, valid fragments).
      This one clip is the acceptance for the whole F1+F8 chain.

---

## Sign-off

Record `PASS`/`FAIL` per line under `testlogs/<date>/SUMMARY.md`. Any FAIL blocks the beta;
capture the clip + the `%LOCALAPPDATA%\clipd\logs\` line that explains it. On a clean sweep this
branch is HW-accepted and ready for `rust-review` → friend distribution.
