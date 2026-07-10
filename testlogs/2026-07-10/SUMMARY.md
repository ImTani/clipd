# HW Test Summary — 2026-07-10 (Nitro V15)

Runbook: `UI-REDESIGN-HW-TESTS.md`. Record `PASS` / `FAIL` per line. Any FAIL blocks the
beta — capture the clip + the `%LOCALAPPDATA%\clipd\logs\` line that explains it.

Build: release (`just release`). Config output dir: `D:\Clips\clipd` (per-app subfolders active).

---

## Findings this session

### FIND-1 — Show-in-folder revealed Documents, not the clip folder — **FAIL → FIXED**

- **Where:** §2 (save-confirmation channels) — recent-clips row `⋮` → "Show in folder".
- **Symptom:** clicking "Show in folder" opened `C:\Users\tanis\OneDrive\Documents`
  instead of the clip's actual folder.
- **Evidence (log):**
  ```
  2026-07-10T00:16:32.164875Z  INFO clipd::ui::recent: revealed clip in folder
    path=D:\Clips\clipd\Antigravity IDE\clipd_1783639111339.mp4
  ```
  The path logged is correct; the log records a successful *spawn* — Explorer mis-parsed
  the argument after launch, so the log line is honest but the reveal still failed.
- **Root cause:** `reveal_path` (`src/ui/recent.rs`) passed `/select,<path>` as a single
  arg; `Command::arg` wraps the whole token in quotes once the path contains a space (the
  T5 per-app folder `Antigravity IDE`), producing
  `"/select,D:\Clips\clipd\Antigravity IDE\clipd_...mp4"`. `explorer.exe` uses a
  non-standard parser (needs `/select,` unquoted, only the path quoted), can't read that
  form, and falls back to its default location (Documents). Never seen before because
  earlier app folders (`clipd`, `Discord`) had no spaces.
- **Fix:** branch `fix/reveal-path-spaces`, commit `64fb1ab` — build the command line
  verbatim with `raw_arg`, quoting only the path (`explorer /select,"C:\a b\f.mp4"`).
  `just check` + 352 tests green. **Awaiting HW re-verify** on the rebuilt release binary:
  Show-in-folder on a clip in a space-named folder (`Antigravity IDE`) must select the
  file in `D:\Clips\clipd\Antigravity IDE\`.

### NOTE — failure-toast could not be triggered via the Settings folder field

- Setting the output folder to `q:\Clips\clipd` via Settings surfaced an inline
  "Couldn't apply — output folder … (os error 3)" and **kept the previous good dir**, so
  the save succeeded ("Clip saved · 5 s"). The UI validates and rejects a bad path — it
  does NOT reach the engine's save-failure path. To exercise the failure toast/pill/log,
  edit `config.toml`'s output dir directly while clipd is closed (unmapped drive or a
  permission-denied existing path), then press the save hotkey.

---

## Checklist status (pending unless marked)

### 1. Tray — FULL M5 re-verification (P1a)
- [ ] Exactly one tray icon; no double-icon flicker at startup
- [ ] Every menu item works (Save / Pause / Start-Stop label / Settings / Open folder / Start-with-Windows / Quit)
- [ ] Tooltip + glyph colour track state
- [ ] Hotkeys fire with tray live
- [ ] Clean quit → `bad_qpc=0 ts_violations=0`, no orphan icon

### 2. Save-confirmation channels (P1b sound · P1c pill · F2 · F3)
- [ ] Balloon shows; success click → clip folder; failure click → log folder; suppressed → Action Center
- [x] **Show-in-folder opens the correct folder** — FAIL→FIXED (FIND-1), awaiting re-verify
- [ ] Sound on save; toggle off is live; custom `.wav` plays
- [ ] Pill (accent ~3s / red ~6s), click-through, not over exclusive-fullscreen
- [ ] F3 preference honoured on success; failure always shows BOTH
- [ ] F2 recording → "Recording saved · N min"

### 3. Settings UI (F4 · F5 · F6 · F7)
- [ ] F4 mic refresh on dropdown open
- [ ] F5 min-size: Browse stays visible, no row clips
- [ ] F6 Debug expander left edge aligned
- [ ] F7 Record source picker + separate-tracks nested toggles + cursor → restart banner

### 4. Engine save-correctness — F1 + F8
- [ ] F1: alt-tab to settings, save → clip spans FULL window; `just verify` green
- [ ] F8: alt-tab churn ~30s → ZERO retargets; kill game → unbind in ~0.6s; game B fullscreen → retarget after ~1.2–1.8s

### 5. Buffer honesty (F7) + `clear_after_save`
- [ ] "Filling up…" neutral while climbing
- [ ] Capped wording + tooltip `(capped)` under High quality + long length
- [ ] At target: no honesty line
- [ ] Fresh config shows `clear_after_save = false`

### 6. Still-owed AV items
- [ ] Mismatched-format mic swap (FIFINE → NVIDIA Broadcast), listen for rate/pitch artifacts
- [ ] Banner census — only the restart-bearing fields raise it
- [ ] AV-2 drift spot-check (`just verify`)

### 7. E2E-FINAL — F1+F8 acceptance
- [ ] Game → alt-tab to settings → swap mic mid-buffer → sing → save → one clip spans full
      window, real game audio throughout, mic-swap seam with voice both sides, `just verify` green

---

## Sign-off

On a clean sweep this branch is HW-accepted → `rust-review` → friend distribution.
Outstanding: rebuild release with `fix/reveal-path-spaces` and re-verify FIND-1.
