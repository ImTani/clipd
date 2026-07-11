# UI-redesign merge — code-review findings & fix plan

Rust review of merge `c6cda3f` ("Merge ui-redesign-research: settings redesign +
save-confirmation shell P1a–P1c + HW-findings F1–F8"), diff `083f8d8..3b0fbf6`
(~5,100 lines across 24 files). Reviewed in four passes (core save/hotkey/ring ·
audio/config/infra · settings/recent/tray · new Win32 UI infra); the top findings
were hand-verified against source before landing here.

Branch for the fixes: `fix/p1-review-findings`. See DECISIONS.md 2026-07-10
"P1 code-review follow-ups" for the decision record.

Build state at review time: `just check` (fmt + clippy `-D warnings`) clean.
`just test` could not run — `target\debug\clipd.exe` was locked by a live
`clipd.exe`; the merge claims 351 pass. **Note the C1 regression is NOT covered by
the existing suite** (tests round-trip defaults, which mask it), so a green suite
would not have caught it — a regression test is part of the C1 fix.

Severity legend: 🔴 Critical · 🟠 High · 🟡 Medium · 🟢 Low.
Status: ☐ open · ⧗ in progress · ☑ fixed · ⊘ deferred (documented, not coded).

---

## 🔴 C1 — `[feedback]` config is silently dropped on every UI save   ☑ fixed
- **Where:** `src/config.rs:638` `Config::apply_to_document`; reached via
  `src/ui/settings.rs` `write_atomic`; re-read at `src/ui/tray.rs:248`.
- **What:** the merge adds `Config.feedback: FeedbackConfig`
  (`save_sound`, `save_sound_path`, `save_show` — the P1b sound + P1c/F3 show-mode
  settings), but `apply_to_document` (the `toml_edit` overlay `write_atomic` uses)
  writes `capture/encode/audio/buffer/output/hotkeys` and **stops**. No `[feedback]`
  table is ever written.
- **Repro / impact:** user toggles "play sound on save" or the show-mode in Settings
  → Save → the `[feedback]` values are dropped on write (file keeps stale values, or
  none → reload falls back to `FeedbackConfig::default()` via `#[serde(default)]`).
  Because `tray.rs` re-reads config from disk on each save event ("so the toggle
  applies live"), the change **never takes effect at all**, while the Settings
  window shows it as applied (in-memory `draft`). The one feature this branch ships
  does not persist.
- **Why tests miss it:** `fresh_rewrite_from_empty_is_complete_and_valid` /
  `write_atomic_preserves_comments_and_unknown_keys` round-trip `Config::default()`;
  the missing table falls back to the default, so `back == cfg` holds by coincidence.
- **Fix:** add a `[feedback]` block to `apply_to_document` (mirror `[hotkeys]`:
  `ensure_table` + `set_val` for `save_sound`, `save_sound_path`, and `save_show` via
  a new `save_show_toml_str` helper). Extend `enum_toml_strings_match_serde` to cover
  `SaveShow`, and add a regression test that sets non-default `feedback.*`, calls
  `write_atomic`, reloads, and asserts the change survived.

## 🟠 H1 — WNDPROC has no panic containment (UB / process-abort across FFI)   ☑ fixed
- **Where:** `src/ui/notify.rs:223` `wndproc` (primary); `src/ui/pill.rs:589`
  `wndproc` (defense-in-depth).
- **What:** `notify::wndproc` — invoked on **every** tray-icon click — calls
  `open_folder`, `RefCell` borrows/clones, and `muda::show_context_menu_for_hwnd`
  (external crate; pumps messages; can re-enter), with no `catch_unwind`. A panic
  there unwinds across the `extern "system"` boundary → process **abort**.
- **Impact:** exactly the failure mode CLAUDE.md says the project exists to kill, and
  worse: it takes the engine threads down too, `TrayWindow::drop` never runs so
  `Shell_NotifyIcon(NIM_DELETE)` is skipped (**ghost icon** until Explorer notices),
  and no `tracing` record of why (detached process has no stderr). `engine.rs:286`
  already wraps every worker body in `catch_unwind` per this convention.
- **Fix:** wrap each WNDPROC's dispatch body in
  `std::panic::catch_unwind(AssertUnwindSafe(..))`, `error!`-log on catch, and fall
  through to `DefWindowProcW` / return `LRESULT(0)` rather than resume-unwind.

## 🟡 M1 — `HICON` leak on tray-window creation failure   ☑ fixed
- **Where:** `src/ui/notify.rs:258` `create_window`.
- **What:** takes ownership of `icon: HICON` but never `DestroyIcon`s it on its three
  failure exits (`GetModuleHandleW`/`CreateWindowExW` `?`, and the
  `Shell_NotifyIcon`-false branch, which only `DestroyWindow`s). Contradicts the
  caller's own SAFETY comment ("on any failure we destroy what we made"). Rare path,
  one-time leak.
- **Fix:** `DestroyIcon(icon)` on every early-exit branch (RAII guard or explicit).

## 🟡 M2 — `HFONT` leak on `CreateDIBSection` failure   ☑ fixed
- **Where:** `src/ui/pill.rs:422`.
- **What:** `CreateDIBSection(..).ok()?` early-returns without freeing `font`, while
  the two sibling failure branches (423, 446) both free it. `render_canvas` runs once
  per save → leaks one `HFONT` per failed render under GDI pressure.
- **Fix:** free `font` before the early `return None`.

## 🟡 M3 — Recent-clips scan is O(all clips on disk), not O(20)   ☑ fixed
- **Where:** `src/ui/recent.rs:332` `push_if_clip` → `read_mp4_duration_secs`.
- **What:** opens + seeks/reads each MP4 for its duration for **every** file before
  `pick_recent` (`recent.rs:286`) truncates to `RECENT_LIMIT = 20`. A user with
  thousands of clips gets a synchronous stall on the settings thread on every
  open / Refresh. Confined to the satellite thread (engine unaffected).
- **Fix:** collect cheap metadata (name/app/size/mtime) only; sort + truncate to the
  limit; then read `read_mp4_duration_secs` for the surviving ≤20.

## 🟡 M4 — `PillHandle::shutdown` join is not actually bounded   ☑ fixed
- **Where:** `src/ui/pill.rs:121`.
- **What:** doc claims a bounded join "so process quit is never stalled," but it is a
  plain `thread.join()` with no timeout; if the pill thread is stuck in
  `UpdateLayeredWindow`/`SetWindowPos` under a hung compositor, teardown blocks
  indefinitely.
- **Fix:** bound the join (poll `is_finished` with a short deadline, abandon-log on
  expiry) so process quit is never stalled.

## 🟢 L1 — `SetDurationCap` has no floor   ☑ fixed
- **Where:** `src/engine.rs:2526` / `src/ring.rs:131`.
- **What:** accepts `seconds = 0`, which zeroes `buffer_ticks`. UI presumably guards
  it, but the engine boundary re-validates nothing.
- **Fix:** defense-in-depth `seconds.max(1)` at the engine handler.

## 🟢 L2 — `folder_dialog.rs` STA assumption is comment-only   ☑ fixed
- **Where:** `src/ui/folder_dialog.rs:27,66`.
- **What:** assumes winit already put the thread in an STA; failure is graceful
  (`CO_E_NOTINITIALIZED` → `warn!` + `None`).
- **Fix:** make it self-verifying — `CoInitializeEx(APARTMENTTHREADED)` tolerant of
  `S_FALSE`/`RPC_E_CHANGED_MODE`, paired with `CoUninitialize` only when we actually
  initialized.

## 🟢 L3 — smaller items (documented, not all coded)   ⊘
- `notify.rs` `clipd toast-test` registers a second icon if run alongside a live
  instance (diagnostic path only) → **doc note**, no code change.
- `src/ui/window_state.rs` (`ui-state.toml`) has no schema `version` field → accept
  for now (falls back to defaults on format change); note in DECISIONS.
- `draw_essentials`/`draw_advanced` exceed the 50-line guideline (declarative egui,
  low-risk) → **not changed**.
- CLAUDE.md dependency-whitelist text still literally lists `tray-icon`, not `muda`
  → **doc fix** (the swap itself is compliant, logged in DECISIONS 2026-07-09 P1a).
- `main.rs:1373` rustdoc attaches to the wrong item (cosmetic) → optional.

---

## What the review confirmed is solid (no action)
- Satellite isolation intact (no `engine`→`ui` coupling); settings-thread crash
  disables Settings for the session rather than respawning; `Mutex` poisoning handled.
- Dependency rule 2 honored: `tray-icon → muda` is a 1:1 de-nesting swap, logged in
  DECISIONS 2026-07-09; `dirs`/`option-ext`/`redox_users` dropped (net reduction).
  All new `windows` feature gates minimal and used at a commented call site.
- No `unsafe` added to logic modules (ring/save/hotkey stay 100% safe); every new
  `unsafe` carries a `// SAFETY:` note; COM interfaces RAII-released; the `PWSTR` is
  copied before `CoTaskMemFree`.
- F1 tail-padding (`save.rs`) correct and tested; `TAIL_LIVENESS_TICKS` a proper spec
  constant; `Ring::set_caps` reuses the dual-cap/GOP-eviction path; `hotkey.rs` live
  rebind has a correct rollback; `appfolder.rs` sanitizes path components (no
  traversal) and is cheap on the ring thread; `sound.rs` uses whitelisted
  `PlaySoundW`. No unbounded channels; save/record sends are `try_send`.
</content>
</invoke>
