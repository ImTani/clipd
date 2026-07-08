# UI Pass Plan — "lavender accent · tray glyph · UX polish" (M7 close-out before the friends beta)

**Written:** 2026-07-08, after a UI/brand research pass (no engine code changed; the
CI license gate was fixed and is committed separately). This is the working plan for
the **final UI cleanup pass before the friends build**. It implements the orchestrator
decisions recorded in `DECISIONS.md` "2026-07-08 — UI/brand cleanup pass". The brand
reference (rendered palette + glyph) is the session artifact; the numbers below are the
source of truth.

> **Normativity.** `CLAUDE.md` + the devpack (`clipper-devpack/devpack/`) are normative.
> `08-FEATURE-COMPLETE.md` M7 fixes the UI's visual language — *"dark, dense, quiet;
> egui default dark + one accent"* — and the **satellite law**: the settings window is
> lazily created from the tray, talks to the engine only over existing channels, and the
> engine must run fully if the window never opens. `ui` depends on engine types, never the
> reverse. This pass adds **no** new dependency and **no** new UI crate (egui/eframe only),
> keeps the < 10 MB binary budget (no image decoder — the glyph is hand-rasterized), and
> touches **only** `src/ui/*` + one small `src/ui/theme.rs`.

---

## 0. What this pass delivers (end-state)

1. **A single lavender accent**, contrast-calculated against egui's real dark surfaces,
   replacing the incidental status-green that was doing double duty as the app's only
   non-semantic colour.
2. **A real tray glyph** — a procedurally-drawn "last-slice" mark (rolling timeline, kept
   tail lit, live-edge playhead), brand-forward (lavender when healthy), replacing the
   solid-colour square. Still zero-dependency; a proper vector logo / embedded `.ico` is an
   **M10 / official-release** job (§7).
3. **The two P1 UX fixes** a first-time friend-tester actually hits: VU meters lifted to the
   top of the window, and "needs restart" shown *inline as you change a field* rather than
   only after Save.
4. **Light structure** (P2): each section framed as a quiet card, the one primary action
   (Save) promoted to a filled-lavender button, and a one-line first-run orientation strip.

Non-goals for this pass (ratchet — `08-FEATURE-COMPLETE.md` REJECTED + M7 scope): no tabs /
window redesign, no new widgets beyond the above, no theme switcher, no editor features, no
SVG/`.ico`/signing (all M10). The **name stays `clipd`** — the rename is deferred to M10
(`DECISIONS.md` "2026-07-08 — Name deferred to M10").

---

## 1. The palette (the single source of truth)

Computed per WCAG 2.1 relative-luminance against egui 0.35 dark `panel_fill`
`#1B1B1B` (`from_gray(27)`) and `extreme_bg_color` `#0A0A0A` (`from_gray(10)`).

| Constant | Hex | Role | Contrast |
|----------|-----|------|----------|
| `ACCENT` | `#A78BFA` | primary — links, focus stroke, progress fill, active toggle, selection stroke, the healthy tray state | 6.3:1 on `#1B1B1B`, 7.3:1 on `#0A0A0A` — AA text + graphical |
| `ACCENT_HOVER` | `#C4B5FD` | hovered link / bright emphasis / peak tip | 9.3:1 |
| `ACCENT_FILL` | `#5B4B9E` | selection **background** (light text sits on top) | text-on 4.8:1 (AA), fg-on-bg 2.4:1 → **fill only, never text** |

**Semantic colours are unchanged** (they encode state, not brand — keep them):
`GOOD #3FB950`, `AMBER #C99A24`, `WARN #E68A00`, `BAD #D03B2F`. These already live inline in
`tray.rs::state_color`, `settings.rs::state_display`, `meter_color`, and `OK_GREEN/ERR_RED`.

**Where these constants live.** A new `src/ui/theme.rs` — the one place UI colours are
defined, mirroring how `tray.rs::state_color` is already documented as "the single place to
re-theme." These are **UI** constants and do **not** belong in `spec_constants.rs` (that file
is reserved for `02-AV-SYNC-SPEC.md` numbers; adding colours there would violate its doc
mandate). `theme.rs` exports the accent/semantic `Color32`s + a `configure_visuals(&Context)`
helper (§2), and both `tray.rs` (as `[u8;4]`) and `settings.rs` reference it, retiring the
duplicated inline literals.

---

## 2. Applying the accent (egui `Visuals`)

The window currently sets **no** custom `Visuals` — it renders egui default dark and the
only colour with personality is the status green. Introduce the accent by starting from
`Visuals::dark()` and overriding the accent-bearing fields, applied once in the
`run_native` creation closure (`settings.rs`, where `cc.egui_ctx` is already published):

```
ctx.set_visuals(theme::configure_visuals());  // = Visuals::dark() + accent overrides
```

Fields to override (minimal, surgical — "one accent"):
- `hyperlink_color` → `ACCENT` (was `#5AAAFF` blue).
- `selection.bg_fill` → `ACCENT_FILL` (was `#005C80` teal); `selection.stroke` → `ACCENT`.
- `widgets.hovered.bg_stroke` / `widgets.active.bg_stroke` → a thin `ACCENT` stroke so
  focus/active reads lavender **in addition to** egui's shape change (never colour-only —
  keeps the P3 accessibility note satisfied).
- Leave `panel_fill` / `extreme_bg_color` / text colours at egui-dark defaults (the palette
  was calculated against exactly those).

**D-U1 — force dark.** `set_visuals(dark + accent)` fixes the window to dark regardless of the
system light theme. M7 mandates "dark, dense, quiet"; the meters/status chrome already assume a
dark ground. The existing theme-adaptive reads (`extreme_bg_color`, `strong_text_color()`)
keep working — they now read the forced dark visuals. Reversible (drop the call → default dark).

The one hand-painted accent: `draw_status_bar`'s buffer-fill green → `ACCENT`. `meter_color`,
`state_display`, `OK_GREEN`, `ERR_RED` stay semantic.

---

## 3. The tray glyph (procedural "last-slice")

Replace `tray.rs::icon_rgba`'s solid fill with a hand-rasterized glyph. Keep the module's
existing seam intact — `icon_for(state)` stays the one entry point; only the pixel producer
changes, so there is no call-site churn (as the module doc already promised).

**Glyph:** a rounded chip (the state colour) with the "last-slice" mark knocked/painted into
it — a thin horizontal track carved out of the chip, the **kept tail** (right ~40%) painted
back in the state colour, and a 1-px **playhead** at the live edge. Supersample 4× and box-
downsample to `ICON_SIZE` for clean edges (pure integer math, no dep).

**State colours — brand-forward (`state_color`):**

| State | Colour | |
|-------|--------|--|
| Buffering (healthy) | `ACCENT #A78BFA` | brand-forward: lavender = "here, quietly working" |
| Paused | `AMBER #C99A24` | |
| Warning | `WARN #E68A00` | |
| Error | `BAD #D03B2F` | |

**Tests** (the pure rasteriser stays unit-testable, no Win32/GDI):
- `each_state_has_a_distinct_colour` — keep, updated for the brand-forward palette.
- Replace `icon_rgba_is_a_full_solid_fill` with: buffer length is `ICON_SIZE²·4`; the chip
  body contains the state colour; the carved track region differs from the chip body (the
  glyph is actually drawn, not a flat fill).

**Beta scope.** At 16 px the playhead knob is barely legible — **accepted as placeholder art
for the friends beta** (orchestrator, this session). The official mark becomes a real **SVG**
and an embedded `.exe` `.ico` at **M10** (needs a build-dependency → out of scope now).

---

## 4. UX fixes (from the audit)

Ranked; P1s are in-scope for this pass, P2s are cheap and worth bundling, P3s are noted only.

- **U-P1a · VU meters first.** Reorder `settings.rs::ui()` so **Audio levels** render directly
  under the header, above Status. The meters are "the single highest-value UI element"
  (`08-FEATURE-COMPLETE.md` M7) — the "is my mic even recording?" answer must not sit below the
  fold on cold-open. Pure reordering; no behaviour change.
- **U-P1b · "Needs restart" shown inline.** Today only `clear_after_save` hot-applies; every
  other edit needs an epoch/encoder rebuild and the requirement surfaces only *after* Save. Hold
  an `applied: Config` snapshot in `Editor` (the config the running engine started from) and, per
  restart-bearing field, show a small lavender **"restart"** chip when the draft differs from
  `applied`. The post-save banner keeps naming the changed set. Sets the expectation *before*
  Save, killing the "I changed quality and nothing happened" confusion.
- **U-P2a · Section cards + primary Save.** Wrap each section (Status / Audio / Settings / Recent)
  in a quiet `egui::Frame` group instead of bare heading+separator, and promote **Save** to a
  filled-`ACCENT` button (the one primary action). Keep it "dense, quiet" — framing, not chrome.
- **U-P2b · First-run orientation.** A one-line strip at the top of the window: *"clipd is
  buffering. Press `<save hotkey>` to save the last N min."* Read the live hotkey + buffer length
  from the same status/config the window already holds. (The quick-start text already exists in
  `just dist`; this surfaces one sentence of it in-app.)
- **U-P3 (noted, not done)** · colour-only signals are already mitigated (state dot has a text
  label, meters have a dB readout) — preserve that discipline. Recent-clips button affordances
  (Open/Folder/Copy) can be revisited with the card treatment later.

---

## 5. Task breakdown (branch per item; local-green then merge, per `07-DEVFLOW.md`)

| # | Task | Change surface | Done when |
|---|------|----------------|-----------|
| **U1** | `theme.rs` + accent `Visuals` | new `src/ui/theme.rs`; `set_visuals` in the `run_native` closure | window renders lavender selection/links/focus; `just check` green |
| **U2** | Recolour the hand-painted accent | `settings.rs::draw_status_bar` green → `ACCENT`; retire duplicated inline literals via `theme.rs` (semantic stays) | buffer bar is lavender; meters/state/OK/ERR unchanged |
| **U3** | Tray glyph + brand-forward states | `tray.rs::icon_rgba`/`state_color`; update the two tray tests | glyph renders per state; buffering = lavender; tests pass |
| **U4** | VU-meters-first reorder | `settings.rs::ui()` ordering | meters render above Status |
| **U5** | Inline "needs restart" chips | `Editor` gains `applied` snapshot + per-field diff; draw chips | changing a restart field shows a chip before Save |
| **U6** | Section cards + primary Save + first-run line | `settings.rs` layout; `Editor::draw` Save button | sections framed; Save is a filled lavender button; orientation line present |

U1–U4 are low-risk (theme + reorder + a pure rasteriser). U5–U6 touch the editor layout and
should be `rust-reviewer`'d before merge (they alter the A5 write path's surrounding UI, though
not the `write_atomic` path itself). Bundling U1–U4 into one branch and U5–U6 into a second is
reasonable.

---

## 6. Acceptance / testing

- `just check` (fmt + clippy `-D warnings`) and `just test` (nextest) green — the existing tray
  tests are updated, not removed; no logic-module tests are touched.
- **No new dependency, no new `windows` feature gate, no new `unsafe`.** The glyph rasteriser is
  pure safe integer math; `theme.rs` is pure.
- **Binary size still < 10 MB** — no image decoder linked (`just release` prints the size).
- **Cold-open still < 300 ms** (M7 acceptance) — reordering + a `set_visuals` call are cheap; the
  existing cold-open latency log confirms it on the Nitro.
- **Manual visual pass on the Nitro** (04-TEST-MACHINE): screenshot the window (selection/links/
  focus/progress are lavender; meters green/amber/red; Save is a filled lavender button; meters
  are first; a restart chip appears when quality is changed) and the tray glyph across a forced
  state change. This is a look-and-feel check, not a spec gate.
- **Still owed from M7, unchanged by this pass:** the 2 h open-window soak (zero engine stalls
  attributable to the UI thread) — fold into a longer session before M6 sign-off.

---

## 7. Decisions carried by this plan (logged in `DECISIONS.md` 2026-07-08)

- **D-U1** Force dark theme + lavender accent via `set_visuals` (§2). Reversible.
- **D-U2** UI colours live in `src/ui/theme.rs`, **not** `spec_constants.rs` (that file is
  AV-SYNC-spec-only). 
- **D-U3** Tray glyph is procedural (hand-rasterized, zero-dep) for the friends beta; the
  official SVG logo + embedded `.exe` `.ico` (needs a build-dep) is **M10**.
- **D-U4** Brand-forward tray: healthy/buffering = lavender; warm colours reserved for
  attention states. (Orchestrator, this session.)
- **Name deferred to M10** — `clipd` retained (reads as "get clip'd"); research recorded in
  `DECISIONS.md` "2026-07-08 — Name deferred to M10".

---

## 8. Out of scope (ratchet)

No new UI crate or webview; no tabs / redesign / theme switcher; no editor features; no clip
trimming; SVG logo, `.exe` icon embedding, code signing, winget/installer/Steam packaging are
all **M10** (`08-FEATURE-COMPLETE.md`). The satellite law and the single `Config::write_atomic`
write path are invariant — this pass changes presentation only.
