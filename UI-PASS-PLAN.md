# UI Pass Plan — "the final UI pass before the friends beta" (M7 close-out)

**Written:** 2026-07-08 (UI/brand research pass), **expanded:** 2026-07-08 into a
*comprehensive* pre-friends-beta UI audit. This is the last UI cleanup pass before
friends run the app on their own machines, so it must account for **every** user-facing
surface — from the parent window down to each container, label, and button — and make
the whole thing render correctly and read as friendly to a first-time, non-technical
tester. It implements the orchestrator decisions in `DECISIONS.md` "2026-07-08 — UI/brand
cleanup pass" and extends them with the robustness/UX work surfaced by the second audit
(window sizing, auto-restart, per-element polish).

> **Normativity.** `CLAUDE.md` + the devpack (`clipper-devpack/devpack/`) are normative.
> `08-FEATURE-COMPLETE.md` M7 fixes the UI's visual language — *"dark, dense, quiet;
> egui default dark + one accent"* — and the **satellite law**: the settings window is
> lazily created from the tray, talks to the engine only over existing channels, and the
> engine must run fully if the window never opens. `ui` depends on engine types, never the
> reverse. This pass adds **no** new dependency and **no** new UI crate (egui/eframe only),
> keeps the < 10 MB binary budget (no image decoder — the glyph is hand-rasterized).

> **Scope boundary of this pass (updated).** The original brand pass touched *only*
> `src/ui/* + theme.rs`. This expanded pass keeps the visual work there, and adds the
> well-contained boundary crossings that the new requirements inherently need:
> 1. **`src/main.rs`** — `run_buffer` re-launches the process on a restart request (the
>    only place that owns process lifecycle). The `ui` layer only *signals intent*; it
>    never spawns the process itself (keeps the satellite law intact — see §7).
> 2. **`src/engine.rs`** returns a restart-vs-quit outcome from `Shell::run` (a return-type
>    change) and gains the **recording-state / save-complete** signalling the two trust
>    indicators need (§8) — a status atomic + a `ShellSignal` variant. These are *additive*
>    engine→ui signals; no engine code depends on the UI.
> 3. **A new `src/ui/folder_dialog.rs`** — a small COM wrapper around the `windows` crate's
>    `IFileOpenDialog` for the Output-folder **Browse…** button (§8.3), adding one
>    `Win32_UI_Shell` feature gate and one `unsafe` COM module (per `CLAUDE.md`, `unsafe`
>    confined to a COM wrapper with a `// SAFETY:` note).
>
> All of §8 (recording feedback, save toast, folder picker) is **now in-scope** per the
> orchestrator (this session), not flagged. Still **no new crate** — the toast and picker
> use the existing `windows` dep; no `rfd`, no notification crate.

---

## 0. What this pass delivers (end-state)

**Brand / visual language (carried from the brand pass):**
1. **A single lavender accent**, contrast-calculated against egui's real dark surfaces
   (§1), replacing the incidental status-green that was the app's only non-semantic colour.
2. **A real tray glyph** — a procedurally-drawn "last-slice" mark, brand-forward (lavender
   when healthy), replacing the solid square (§3). Zero-dependency placeholder art for
   the beta; the SVG logo / embedded `.ico` is an M10 job.
3. **Force dark theme + accented `Visuals`** applied once at window creation (§2).
4. **A value-harmonised semantic palette** — the VU-meter / state green, amber, orange, and
   red are retuned to share the accent's HSV **value** (brightness) so the whole window reads
   as one colour system instead of the accent floating over darker, heavier traffic-lights
   (§1.1).

**Robustness / "everything renders" (the nitpicks + the audit):**
4. **A minimum window size and content that respects the window** (§6) — no more cut-off
   widgets when the window is small; bars and fields flex to the available width.
5. **A persistent auto-restart banner** (§7) — after saving any setting that needs a
   restart, a fixed banner offers a one-click **Restart now** that relaunches the app to
   apply the change. Non-modal, so the user can make several changes and restart once.

**Per-element UX polish (comprehensive go-through, §4/§5):**
6. VU meters lifted above the fold; inline "needs restart" chips as a field changes;
   section cards + a promoted primary Save; a first-run orientation line; hover tooltips
   on every setting; human-readable recent-clip labels; de-emphasised diagnostics; a native
   **Browse…** folder picker for the Output folder.

**Trust feedback — the "did my clip actually save?" gaps (now in-scope, §8):**
7. **Recording on/off feedback** in the tray + status (our analogue of ShadowPlay's
   persistent instant-replay icon), and a **save-complete / save-failed tray balloon** (the
   native, no-overlay analogue of Medal's and ShadowPlay's corner "Clip Saved" toast) — the
   two biggest confirmation gaps for a tester with no settings window open.

**Non-goals for this pass** (ratchet — `08-FEATURE-COMPLETE.md` REJECTED + M7 scope): no
tabs / window redesign, no theme switcher, no editor/trim features, no SVG/`.ico`/signing
(all M10). The **name stays `clipd`** (rename deferred to M10). New UI crates (a native
file-dialog crate, a toast crate) are **out** — anything added must use the existing
`windows`/egui deps or be deferred.

---

## 1. The palette (the single source of truth)

Computed per WCAG 2.1 relative-luminance against egui 0.35 dark `panel_fill`
`#1B1B1B` (`from_gray(27)`) and `extreme_bg_color` `#0A0A0A` (`from_gray(10)`).

| Constant | Hex | Role | Contrast |
|----------|-----|------|----------|
| `ACCENT` | `#A78BFA` | primary — links, focus stroke, progress fill, active toggle, selection stroke, the healthy tray state, the restart banner | 6.3:1 on `#1B1B1B`, 7.3:1 on `#0A0A0A` — AA text + graphical |
| `ACCENT_HOVER` | `#C4B5FD` | hovered link / bright emphasis / peak tip | 9.3:1 |
| `ACCENT_FILL` | `#5B4B9E` | selection **background** + filled-button fill (light text on top) | text-on 4.8:1 (AA), fg-on-bg 2.4:1 → **fill only, never text** |

**Semantic colours keep their *meaning* but are value-harmonised** (see §1.1). They still
encode state (green/amber/orange/red), and still live in one place — today inline in
`tray.rs::state_color`, `settings.rs::state_display`, `meter_color`, and `OK_GREEN/ERR_RED`,
after this pass in `theme.rs`.

## 1.1 Value-harmonising the semantic palette

**The problem the user flagged.** The accent `#A78BFA` is a light, softly-saturated lavender
(HSV ≈ H 255°, **S 0.44**, **V 0.98**). The current semantic colours are darker and more
saturated — e.g. `GOOD #3FB950` (V 0.73), `AMBER #C99A24` (V 0.79, S 0.82), `BAD #D03B2F`
(V 0.82) — so beside the accent the meters/state dots read as heavier, punchier "traffic
lights" while the accent floats above them. The colours don't feel like one system.

**The fix.** Retune the four semantic colours to share the **accent's HSV value (~0.98)**,
keeping each hue so they stay unmistakably green / amber / orange / red, and pulling
saturation toward the accent's band so the family reads as one soft, bright system rather
than one pastel + four punchy dots. Concretely, per colour: hold H, set V = 0.98, and lower S
into a harmonised range — with the hard constraints below deciding exactly how far.

**Hard constraints (a plan requirement, verified at implementation, not eyeballed):**
- **WCAG AA preserved.** Each retuned colour must still clear its contrast bar: ≥ 3:1
  graphical on the meter track `extreme_bg_color #0A0A0A`, and for the two that are also
  *text* (`OK_GREEN`, `ERR_RED`) ≥ 4.5:1 on `panel_fill #1B1B1B`. Raising V *helps* here.
- **Red stays danger.** At V 0.98 with low S, red drifts to salmon/pink and stops reading as
  "clip / error." Red keeps more saturation than the others (target S ≈ 0.6–0.7) so it still
  says *stop*. Green/amber/orange can go softer (S ≈ 0.5).
- **Tray states stay distinguishable.** Buffering is now the lavender accent (D-U4); paused
  (amber), warning (orange), and error (red) must remain telling-apart at a glance in the
  16-px tray icon. Harmonising *value* while preserving *hue* separation keeps this; verify
  amber vs orange don't collapse (they are only ~7° apart today — keep that gap).

**Candidate starting values** (V ≈ 0.98; implementation validates + fine-tunes with the
contrast check — the dataviz skill's palette validator is the tool for this):
`GOOD ≈ #7DFA8F`, `AMBER ≈ #FAD67D`, `WARN ≈ #FAC87D`, `BAD ≈ #FA6D5F` (red kept more
saturated than the S-0.5 `#FA867D` so it still reads as danger). These are a **direction, not
frozen numbers** — the constraints above are the source of truth; if a candidate fails
contrast or distinguishability, adjust S/V and re-validate.

All four (plus the accent) live as `Color32` consts in `theme.rs`; `meter_color`,
`state_display`, `state_color` (as `[u8;4]`), `OK_GREEN`, and `ERR_RED` all reference them,
so the harmonised set is defined once.

**Where these constants live.** A new `src/ui/theme.rs` — the one place UI colours are
defined, mirroring how `tray.rs::state_color` is already documented as "the single place to
re-theme." These are **UI** constants and do **not** belong in `spec_constants.rs` (that file
is reserved for `02-AV-SYNC-SPEC.md` numbers). `theme.rs` exports the accent/semantic
`Color32`s + a `configure_visuals()` helper (§2), and both `tray.rs` (as `[u8;4]`) and
`settings.rs` reference it, retiring the duplicated inline literals.

---

## 2. Applying the accent (egui `Visuals`)

The window currently sets **no** custom `Visuals` — it renders egui default dark and the
only colour with personality is the status green. Introduce the accent by starting from
`Visuals::dark()` and overriding the accent-bearing fields, applied once in the
`run_native` creation closure (`settings.rs::run_window`, where `cc.egui_ctx` is already
published):

```
ctx.set_visuals(theme::configure_visuals());  // = Visuals::dark() + accent overrides
```

Fields to override (minimal, surgical — "one accent"):
- `hyperlink_color` → `ACCENT`.
- `selection.bg_fill` → `ACCENT_FILL`; `selection.stroke` → `ACCENT`.
- `widgets.hovered.bg_stroke` / `widgets.active.bg_stroke` → a thin `ACCENT` stroke so
  focus/active reads lavender **in addition to** egui's shape change (never colour-only).
- Leave `panel_fill` / `extreme_bg_color` / text colours at egui-dark defaults (the palette
  was calculated against exactly those).

**D-U1 — force dark.** `set_visuals(dark + accent)` fixes the window to dark regardless of
the system light theme. M7 mandates "dark, dense, quiet"; the meters/status chrome already
assume a dark ground. The existing theme-adaptive reads (`extreme_bg_color`,
`strong_text_color()`) keep working — they now read the forced dark visuals. Reversible.

The one hand-painted accent: `draw_status_bar`'s buffer-fill green → `ACCENT`. `meter_color`,
`state_display`, `OK_GREEN`, `ERR_RED` stay semantic.

**Window icon (new, cheap).** The viewport currently sets no icon, so the taskbar / Alt-Tab /
title-bar show egui's generic default. Feed the same procedural glyph (§3) into
`ViewportBuilder::with_window_icon` at buffer-state colour so the window is identifiably
`clipd`. Reuses the §3 rasteriser (an `egui::IconData { rgba, width, height }`); zero new dep.

---

## 3. The tray glyph (procedural "last-slice")

Replace `tray.rs::icon_rgba`'s solid fill with a hand-rasterized glyph. Keep the module's
existing seam intact — `icon_for(state)` stays the one entry point; only the pixel producer
changes, so there is no call-site churn.

**Glyph:** a rounded chip (the state colour) with the "last-slice" mark knocked/painted into
it — a thin horizontal track carved out of the chip, the **kept tail** (right ~40%) painted
back in the state colour, and a 1-px **playhead** at the live edge. Supersample 4× and box-
downsample to `ICON_SIZE` for clean edges (pure integer math, no dep). The same rasteriser
feeds the window icon (§2).

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
  body contains the state colour; the carved track region differs from the chip body.

**Beta scope.** At 16 px the playhead knob is barely legible — **accepted placeholder art
for the friends beta**. The official mark becomes a real **SVG** + embedded `.exe` `.ico` at
**M10** (needs a build-dependency → out of scope now).

---

## 4. Comprehensive UI inventory (every surface, current → issue → action)

The full walk-through the audit demanded. **P** = priority; **§** points to the section that
implements it. "engine" in the change column marks the trust items whose small additive
engine/`windows` touch is described in §8.

### 4.1 Parent window (`settings.rs::run_window` / `SettingsApp::ui`)

| Surface | Current | Issue | Action | P | § |
|---|---|---|---|---|---|
| Inner size | opens 560×440 | fine | keep first-open size | — | — |
| **Min size** | none set | shrinking clips widgets; vertical-only scroll can't recover horizontal overflow | add `with_min_inner_size` | **P1** | 6 |
| **Content width** | fixed-width bars/fields | don't grow/shrink with the window | make bars/fields flex to `available_width` | **P1** | 6 |
| Theme | egui default dark, no accent | status-green does double duty; follows system light theme | force dark + accent | P1 | 2 |
| Window icon | none (generic egui) | not identifiably `clipd` in taskbar/Alt-Tab | set from the glyph | P2 | 2 |
| Scroll | one `ScrollArea::vertical` wrapping everything | banner would scroll away with content | pin banner outside the scroll (`show_inside` panels) | P1 | 7 |

### 4.2 Header

| Surface | Current | Action | P | § |
|---|---|---|---|---|
| `clipd settings` heading + `version X` | present | keep; tighten spacing under the cards treatment | P2 | 5 |
| First-run orientation | none | one-line "clipd is buffering — press `<hotkey>` to save the last N s" | P2 | 5 |

### 4.3 Status section (`draw_status`)

| Surface | Current | Issue | Action | P | § |
|---|---|---|---|---|---|
| State dot + label | green/amber/orange/red dot + text | fine (has a text label — not colour-only) | recolour dot via `theme.rs`; keep semantic | P2 | 2 |
| Capture line | `target · W×H @ fps · H.264` | good | keep | — | — |
| Encoder GPU | shown when known | good | keep | — | — |
| Buffer fill + bar | line + `draw_status_bar` | bar width clamped `(80,320)` — doesn't track window | recolour to `ACCENT`; make width responsive | P1 | 2,6 |
| **Frames counters** | `captured·encoded·muxed·dropped` | very developer-y for a non-technical tester | de-emphasise (`weak`) or move under a `CollapsingHeader` "Diagnostics" | P2 | 5 |
| Last save line | outcome + relative time | good — keep as the in-window save confirmation | keep | — | — |
| **Recording state** | *not surfaced at all* | tester can't tell if a timed recording is running | show "● Recording" (engine status atomic) | **P1** | 8 |

### 4.4 Audio levels (`draw_meter`)

| Surface | Current | Issue | Action | P | § |
|---|---|---|---|---|---|
| Position | rendered *below* Status | "is my mic recording?" is the highest-value answer and sits below the fold | move directly under the header, above Status | **P1** | 5 |
| Meter row | 90px label + bar + 64px dB readout | fixed label + `bar_w = available-64` can overflow a narrow window | keep readout reserve; guarantee fit via min-size; let bar flex | P1 | 6 |
| **Bar colours** | green/amber/red at 0.8/0.95 | darker/punchier than the accent — the whole window reads inconsistent | value-harmonise to the accent | **P1** | 1.1 |
| Empty state | "No audio streams are enabled." | fine | keep | — | — |
| dB readout / peak tick | monospace dB, bright tick | good, not colour-only | keep | — | — |

### 4.5 Settings editor fields (`draw_fields`)

| Field | Widget | Issue | Action | P | § |
|---|---|---|---|---|---|
| Quality / Resolution / Frame rate | combo boxes | no explanation of what tiers mean | `on_hover_text` tooltip per row | P2 | 5 |
| Buffer length | `DragValue 1..=MAX s` | drag-only isn't discoverable; RAM cost not obvious at the field | tooltip ("drag or type; ~X MiB RAM"); the estimate line already helps | P2 | 5 |
| **Output folder** | singleline `TextEdit` | typing a path is error-prone for a tester; no picker | keep field + tooltip/hint; add a native **Browse…** button (`windows` `IFileOpenDialog` COM wrapper) | **P1** | 8 |
| Clear after save / Desktop audio | checkboxes | fine | tooltips | P3 | 5 |
| Microphone | enumerated dropdown (B3.5) | good; `Unavailable: <id>` preserved | keep; tooltip | P3 | 5 |
| Estimate line | `≈ Mbps · buffer ≈ s / MiB` | good, informative | keep | — | — |
| **Save button** | plain `ui.button` | not visually the primary action | promote to filled-`ACCENT` button | P2 | 5 |
| Save result line | green/red colored label | good | keep semantic | — | — |
| **Restart-required feedback** | only named in the post-save line | user doesn't see it coming, and can't act on it | inline chips (pre-save) **+** persistent banner with a Restart button (post-save) | **P1** | 5,7 |

### 4.6 Hotkeys (`draw_hotkeys` / `hotkey_row`)

| Surface | Current | Issue | Action | P | § |
|---|---|---|---|---|---|
| Rebind + editable field | 150px field + Rebind + live availability | this is the **widest fixed row** (field+button+"⚠ in use by another app") — first to clip on a narrow window | min-size must cover it; let the availability note wrap below on tight widths | P1 | 6 |
| Availability badges | ✓ available / ⚠ in use / same-as | good, informative | recolour via `theme.rs` (stays semantic) | P3 | 2 |
| Capturing prompt | "press a combo… (Esc cancels)" + hint | good | tooltip on Rebind explaining OS-claimed combos | P3 | 5 |

### 4.7 Recent clips (`recent.rs`)

| Surface | Current | Issue | Action | P | § |
|---|---|---|---|---|---|
| Filename label | raw `clipd_1700000000000.mp4` (monospace) | epoch-ms name is unreadable | show a friendly relative time ("3 min ago") from the mtime we already read; keep the raw name weak/secondary | P2 | 5 |
| Row actions | Open / Folder / Copy path (×20) | fine, if dense | keep; optionally tighten under the card treatment | P3 | 5 |
| Empty / refresh | "No clips yet…" + Refresh | fine | keep | — | — |

### 4.8 Tray (`tray.rs`)

| Surface | Current | Issue | Action | P | § |
|---|---|---|---|---|---|
| Icon | solid colour square | no brand, no legibility | procedural glyph | P1 | 3 |
| Tooltip | `clipd — <state>` | good | keep | — | — |
| Menu items | Save / Pause(✓) / Start-stop recording / Settings / Open folder / Start-with-Windows(✓) / Quit | **recording has no on/off indication** — the label is static whether or not a recording is running | reflect recording state in the label + tray tooltip/glyph | **P1** | 8 |
| Save feedback | none from the tray | with no window open, pressing the save hotkey gives **zero** visible confirmation | save-complete / -failed tray balloon (native, no-overlay) | **P1** | 8 |

---

## 5. Per-element UX polish (in-scope, `src/ui` only)

Ranked; all are pure-presentation changes confined to `src/ui/*`.

- **U-P1a · VU meters first.** Reorder `SettingsApp::ui()` so **Audio levels** render directly
  under the header, above Status. The meters are "the single highest-value UI element"
  (`08-FEATURE-COMPLETE.md` M7). Pure reordering; no behaviour change.
- **U-P1b · "Needs restart" shown inline.** Today only `clear_after_save` hot-applies; every
  other edit needs an epoch/encoder rebuild and the requirement surfaces only *after* Save.
  Hold an `applied: Config` snapshot in `Editor` (the config the running engine started from)
  and, per restart-bearing field, show a small lavender **"restart"** chip when the draft
  differs from `applied`. This `applied` snapshot is **also** the basis for the §7 banner, so
  U-P1b and §7 share one field. Sets the expectation *before* Save.
- **U-P2a · Section cards + primary Save.** Wrap each section (Audio / Status / Settings /
  Recent) in a quiet `egui::Frame` group instead of bare heading+separator, and promote
  **Save** to a filled-`ACCENT` button (the one primary action). Framing, not chrome.
- **U-P2b · First-run orientation.** A one-line strip at the top: *"clipd is buffering. Press
  `<save hotkey>` to save the last N min."* Read the live hotkey + buffer length from the
  status/config the window already holds.
- **U-P2c · Hover tooltips on every setting.** `.on_hover_text` on each editor row + the
  Rebind button. Zero-dep, high value for a non-technical tester who doesn't know what
  "Efficient" quality or a buffer length means. Text is short and lives beside the widget.
- **U-P2d · Friendly recent-clip labels.** Format each clip's mtime into a relative time
  ("just now", "3 min ago", "yesterday") reusing `status::format_elapsed`; keep the raw file
  name as weak secondary text. No new dep (we already read the mtime).
- **U-P2e · De-emphasise diagnostics.** Render the frames `captured·encoded·muxed·dropped`
  line as `weak()` text, or tuck it under a collapsed `egui::CollapsingHeader("Diagnostics")`.
  Keeps the trust signal available without dominating a friend's first look.
- **U-P3 (noted)** · colour-only signals are already mitigated (state dot has a text label,
  meters have a dB readout) — preserve that discipline as cards/recolours land.

---

## 6. Window sizing & responsiveness (the "everything renders" fixes)

**The two nitpicks, root-caused:**

1. **No minimum size.** `run_window` sets `.with_inner_size(WINDOW_SIZE)` but no minimum, so
   the window can be dragged smaller than its content. Because the whole page is wrapped in a
   **vertical-only** `ScrollArea`, horizontal overflow has nowhere to go and is simply clipped
   ("cuts things off").
2. **Fixed-width content.** Several widgets don't track the window: `draw_status_bar` clamps
   its width to `(80, 320)`; `draw_meter` reserves a fixed 90px label + 64px readout; the
   hotkey field is a fixed `desired_width(150)`. On a narrow window these overflow; on a wide
   one the bars stop growing.

**Fix — set a floor, then let content flex:**

- **`with_min_inner_size([440.0, 340.0])`** in `run_window`, chosen so the **widest fixed
  row** renders in full: the hotkey row (150px field + Rebind button + the longest
  availability note "⚠ in use by another app") is the binding constraint at ≈ 400px of
  content + panel margins. Height 340 shows the header + the first card without feeling
  cramped. Add both as named `const`s beside `WINDOW_SIZE` with this rationale as a doc
  comment. Reversible (drop the call → today's behaviour).
- **Let the bars flex.** `draw_status_bar`: replace the `(80, 320)` clamp with
  `available_width` up to a comfortable max (never exceeding available, floor low enough to
  survive the min window). `draw_meter`: keep the readout reserve but compute `bar_w` from the
  real available width so the bar grows/shrinks with the window (the min-size floor guarantees
  it never underflows the 80px minimum).
- **Let the tight rows wrap.** In `hotkey_row`, allow the availability note to wrap to the
  next line at narrow widths rather than pushing the row past the edge (egui wraps labels by
  default when width is constrained; ensure the row isn't forced into a single
  `horizontal` that suppresses wrapping — use `horizontal_wrapped` where needed).
- **Optional nicety:** cap the *maximum* content column (e.g. wrap the scroll content in a
  `max_width`-limited column) so an over-wide window doesn't stretch label/field rows into
  awkwardly long lines. Low priority; the min-size + flex fixes are the requirement.

**Acceptance:** at the minimum size every label, bar, field, and button is fully visible (no
horizontal clip); growing the window widens the meters and the buffer bar smoothly; the
vertical scrollbar appears only when the content is taller than the window.

---

## 7. Auto-restart banner (apply settings without a manual relaunch)

**Problem.** Every setting except `clear_after_save` needs the engine to rebuild
(epoch/encoder/audio), which today means the user must quit `clipd` and start it again by
hand. A friend who changes quality and sees nothing happen reads it as "broken." The user's
call: a **persistent, non-modal banner** (not a modal) so they can make several changes and
restart **once**, with a one-click **Restart now**.

### 7.1 Behaviour

- The banner appears once there is **at least one saved-but-not-yet-applied restart-bearing
  change** — i.e. the on-disk/committed config differs from the config the running engine
  started from (`applied`, the §5 U-P1b snapshot).
- It is **fixed** (does not scroll with the page) and names the pending set:
  *"⟳ Restart to apply your changes: quality, frame rate."* with **[Restart now]** and a
  quiet **[Later]** (dismiss until the next save). Accent-filled, using `ACCENT`.
- Making more restart-bearing saves keeps the banner up and **accumulates** the pending set
  (because `applied` only advances on an actual restart). `clear_after_save` never triggers it
  (it hot-applies).

### 7.2 Placement (fixed, not in the scroll)

The page is currently one `ScrollArea::vertical` inside `Frame::central_panel`. Restructure
`SettingsApp::ui` so the banner is a pinned panel and the existing content becomes the
central region:

```
egui::TopBottomPanel::bottom("restart_banner")
    .show_animated_inside(ui, restart_pending, |ui| { /* banner row */ });
egui::CentralPanel::default().show_inside(ui, |ui| {
    egui::ScrollArea::vertical().show(ui, |ui| { /* today's content */ });
});
```

`show_*_inside` operate on the root `Ui` eframe hands us, so no `ctx`-level panel API is
needed. Bottom keeps it out of the way of the header/meters and always visible; top is
equally acceptable — bottom chosen so it doesn't shove the first-run line / meters down.

### 7.3 The restart mechanism (and the one real hazard)

A restart is "**quit the current process, having first launched a fresh one**." The hazard is
that the new instance re-registers the **same global hotkeys** (`RegisterHotKey` via
`global-hotkey`) and re-opens the **same capture/audio devices**; if the old process still
holds them, the new one's `HotkeyPump::spawn` fails — which is *fatal to buffer mode* — and
the new instance dies on launch. So the launch must be **ordered after the old process has
released those resources.**

The current teardown order already gives us that ordering — we just need to spawn *after* it.
In `run_buffer`: `shell.run(&engine)` returns → `engine.stop_and_join()` (releases
capture/audio) → `pump.request_quit(); pump.join()` (releases the hotkeys). So the correct,
race-free sequence is: **signal → tear down → then relaunch.**

**Signalling path (ui, no engine dependency):**
1. `Shared` (the tray↔window struct) gains `restart: AtomicBool`, alongside the existing
   `quit`/`visible`/`rescan_*` atomics.
2. The banner's **Restart now** sets `shared.restart = true` and `request_repaint()`.
3. `SettingsHandle` exposes `restart_requested(&self) -> bool` reading that atomic.
4. The tray loop (`Shell::run`) checks it each poll; when set it does the **same teardown as
   Quit** (`self.settings.shutdown(); cmd_tx.send(Shutdown)`) but returns a **`Restart`**
   outcome instead of a plain quit.

**Relaunch path (main.rs, owns process lifecycle):**
5. `Shell::run` changes its return type from `()` to a small
   `enum ShellOutcome { Quit, Restart }` (lives in `engine.rs` beside `ShellSignal`, or in
   `ui`). No new engine→ui dependency.
6. `run_buffer` matches the outcome: after its normal `stop_and_join` + `pump.join()`
   teardown, on `Restart` it spawns a fresh instance and then returns:
   ```
   std::process::Command::new(std::env::current_exe()?)
       .args(std::env::args().skip(1))   // same argv: `buffer [--seconds N] …`
       .spawn();                          // detached; independent of the exiting parent
   ```
   The child is an independent process on Windows; consider `DETACHED_PROCESS` /
   `CREATE_NEW_PROCESS_GROUP` via `CommandExt::creation_flags` so it fully outlives the parent
   console. Because we spawn **after** `pump.join()`, the hotkeys and devices are already free
   — no retry hack needed.

**Why not spawn from `ui`?** Keeping the spawn in `main.rs` preserves the satellite law: the
UI signals *intent* over shared state; `main` orchestrates the process. `ui` never learns how
the app is launched.

### 7.4 `applied` snapshot (shared with U-P1b) — and its one limitation

`applied` is seeded from the config loaded when the window is first created and only advances
on an actual restart. If a save happened in a *previous* window session without a restart,
the freshly-opened window would seed `applied` from the already-changed on-disk config and
under-report — an accepted minor limitation for the beta (the common path is: open window →
change → save → restart, all in one session). A fully-correct `applied` would require the
engine to publish its started-from config; **not** worth the engine coupling now. Note it in
`DECISIONS.md`.

### 7.5 Tests

- `restart` atomic round-trips through `Shared` / `SettingsHandle::restart_requested`.
- Pending-set computation (`applied` vs committed config) reuses `restart_required_fields`;
  add a unit test that the banner's set matches it and is empty when only `clear_after_save`
  changed.
- The relaunch itself is HW/manual (see §9 acceptance) — it spawns a real process.

---

## 8. Trust feedback — recording state, save confirmation, folder picker (in-scope)

These are the biggest "did it actually work?" gaps for a tester **with no settings window
open**. All three are **in-scope** for this pass (orchestrator, this session). Each needs a
small additive engine/`windows` touch; all are reversible.

### 8.0 How the incumbents confirm it — and our honest, in-scope analogue

Both incumbents answer "did my clip save?" with two things (researched 2026-07-08):
- **A corner on-screen overlay toast** — Medal shows a **"Clip Saved"** alert (and a "Now
  Recording" alert); ShadowPlay shows a **"…saved"** notification in the top-right and a
  **persistent Instant-Replay status icon** in-game while replay is armed.
- **A persistent status icon** while recording / instant-replay is active.

We **cannot** draw an in-game overlay — overlays are a permanent non-goal (`CLAUDE.md` §1,
`01-PROJECT-PLAN.md`). So we map their two mechanisms to what a tray app *can* do natively:

| Incumbent mechanism | Our in-scope analogue |
|---|---|
| Corner "Clip Saved" overlay toast | **Windows tray balloon** (`Shell_NotifyIcon NIF_INFO`) — §8.2 |
| Persistent "recording / replay armed" icon | **Tray icon state + tooltip + menu label** — §8.1 |

This is the correct, honest substitute: the OS notification area is exactly where a
background capture tool belongs, and it needs no overlay, no injection, no extra window.

### 8.1 Recording on/off feedback  *(in-scope)*

Today the tray's "Start / stop recording" item and the status strip give **no** indication
whether a timed recording is running — the tester's analogue of ShadowPlay's persistent
Instant-Replay icon is simply missing. Implementation:
- Add a `recording: AtomicBool` to `EngineStatus` (already `Arc`-shared to the UI), set by
  the engine at the record start/stop sites.
- **Tray:** flip the menu label ("Start recording" ⇄ **"Stop recording"**), and make the
  recording state visible on the icon itself — a small **recording accent** on the glyph (or a
  distinct tooltip suffix "· recording") so it's legible with no menu open. (Recording is
  *orthogonal* to the buffering/paused/warning/error state, so it's an overlay dot on the
  glyph, not a fifth state colour — keep the four state colours meaning what they mean.)
- **Status strip:** show a "● Recording — MM:SS" line while true (the elapsed time is a nice
  reassurance; derive it from a record-start timestamp published alongside the bool).

Touches `status.rs` + `engine.rs` (the record start/stop sites) + `tray.rs` + `settings.rs`.
It's the record-mode analogue of the Pause checkmark the tray already has.

### 8.2 Save-complete / save-failed tray balloon  *(in-scope — required)*

With the window closed, pressing the save hotkey produces **zero** visible confirmation; the
in-window "Last save: OK …" line only helps if the window is open. This is the single most
important trust signal for a tester, and — since we have no overlay — the tray balloon is our
"Clip Saved" toast.
- The engine already records the save outcome to `EngineStatus`; add a
  **`ShellSignal::Saved { ok: bool, seconds: f32 }`** (the tray already consumes `ShellSignal`
  over `try_send`, so a slow/absent shell never blocks the engine — satellite-safe).
- On `Saved { ok: true, .. }` the tray raises a balloon **"Clip saved — 12.0 s"**; on
  `ok: false`, **"Clip didn't save — check the log"** (the failure toast matters *more* than
  the success one — it's the whole "why didn't my clip save" trust model made visible).
- `tray-icon` has no notification API, so the balloon is a
  `Shell_NotifyIcon(NIM_MODIFY, NIF_INFO)` call — a new **`unsafe` Win32** block confined to
  `tray.rs`, which already owns the message-pump `unsafe`, with a `// SAFETY:` note. One
  `Win32_UI_Shell` feature gate (shared with §8.3), added in the same commit that calls it.
- Keep it quiet: one balloon per save, no sound (M10 owns the optional save sound), and it
  respects Windows' own "focus assist / quiet hours" automatically (the OS gates `NIF_INFO`).

### 8.3 Native "Browse…" folder picker for Output folder  *(in-scope — required)*

Typing an output path is error-prone for a non-technical tester. Add a **Browse…** button
beside the Output-folder field that opens the native folder chooser and writes the chosen
path into the draft. Implementation (whitelist-clean — **no `rfd`**):
- A small `src/ui/folder_dialog.rs` COM wrapper over the existing `windows` dep:
  `CoCreateInstance(FileOpenDialog)` → `SetOptions(FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM)` →
  `Show(parent)` → `GetResult` → `GetDisplayName(SIGDN_FILESYSPATH)`. One `unsafe` COM module
  with a `// SAFETY:` note (allowed — `CLAUDE.md` confines `unsafe` to COM wrappers), one
  `Win32_UI_Shell` feature gate (shared with §8.2), added in the commit that calls it.
- Runs on the settings-UI thread, which is already COM-initialised for that thread; the dialog
  is modal to the settings window (pass its `HWND`). A cancel leaves the field unchanged.
- The Save-time `validate_output_dir` (creates/validates, red error on failure) **stays** as
  the backstop for hand-typed or TOML-set paths — Browse… is the friendly front door, not a
  replacement for validation.

This was previously flagged "defer"; the orchestrator has made it a **requirement** for the
beta (a mistyped clips folder is exactly the kind of silent-looking snag the trust model
exists to kill).

---

## 9. Task breakdown (branch per item; local-green then merge, per `07-DEVFLOW.md`)

| # | Task | Change surface | Done when |
|---|------|----------------|-----------|
| **U1** | `theme.rs` + accent `Visuals` + force dark + window icon | new `src/ui/theme.rs`; `set_visuals` + `with_window_icon` in `run_window` | window renders lavender selection/links/focus; taskbar shows the glyph; `just check` green |
| **U2** | Recolour hand-painted accent + **value-harmonise the semantic palette** | `settings.rs::draw_status_bar` green→`ACCENT`; `meter_color`/`state_display`/`state_color`/`OK_GREEN`/`ERR_RED` retuned per §1.1 via `theme.rs`; retire inline literals | buffer bar lavender; green/amber/orange/red share the accent's value + pass WCAG AA; red still reads as danger |
| **U3** | Tray glyph + brand-forward states | `tray.rs::icon_rgba`/`state_color`; update the two tray tests | glyph renders per state; buffering = lavender; tests pass |
| **U4** | VU-meters-first + section cards + primary Save + first-run line + tooltips + friendly recent labels + diagnostics de-emphasis | `settings.rs` layout + `recent.rs` labels | meters above Status; sections framed; Save is filled lavender; tooltips present; recent shows relative times |
| **U5** | Inline "needs restart" chips (`applied` snapshot) | `Editor` gains `applied` + per-field diff; draw chips | changing a restart field shows a chip before Save |
| **U6** | **Window min-size + responsive content** | `run_window` min size; `draw_status_bar`/`draw_meter`/`hotkey_row` widths | at min size nothing clips; bars flex with the window |
| **U7** | **Auto-restart banner + relaunch** | `Shared.restart`; `SettingsHandle::restart_requested`; `Shell::run → ShellOutcome`; banner panel; `run_buffer` spawn-on-restart | Save a restart field → banner appears → Restart now relaunches and the change is live |
| **U8** | **Recording feedback** (§8.1) | `status.rs` `recording` atomic + record-start stamp; `engine.rs` start/stop sites; `tray.rs` label/glyph; `settings.rs` status line | tray label flips + glyph shows recording; status shows "● Recording — MM:SS" |
| **U9** | **Save-complete / -failed tray balloon** (§8.2) | `engine.rs` `ShellSignal::Saved`; `tray.rs` `Shell_NotifyIcon` balloon (`unsafe`, `Win32_UI_Shell`) | window-closed save → a "Clip saved — N s" balloon; a forced failure → a failure balloon |
| **U10** | **Native Browse… folder picker** (§8.3) | new `src/ui/folder_dialog.rs` COM wrapper (`IFileOpenDialog`, `Win32_UI_Shell`); Browse… button in `draw_fields` | Browse… opens the native folder chooser and fills the Output-folder field |

**Bundling / review.** U1–U4 are low-risk (theme + palette retune + reorder + pure rasteriser
+ labels) — one branch; U2's harmonised colours are contrast-validated before merge. U5–U7
touch the editor layout, the `Shared`/`Shell` seam, and `main.rs` process spawn — a second
branch, **`rust-reviewer`'d** (U7 restructures the shell return path and adds a process spawn;
it must not regress the Quit/teardown ordering or the `write_atomic` path). U8–U10 add engine
state, a `ShellSignal`, and two `unsafe` Win32 surfaces (`tray.rs` balloon, `folder_dialog.rs`
COM) — a third branch, **`rust-reviewer`'d**, each `unsafe` block carrying a `// SAFETY:` note
and its feature gate added in the calling commit.

---

## 10. Acceptance / testing

- `just check` (fmt + clippy `-D warnings`) and `just test` (nextest) green — existing tray /
  editor tests updated, not removed; no logic-module tests touched.
- **No new dependency (no `rfd`, no toast crate).** One new `windows` feature gate
  (`Win32_UI_Shell`, shared by the U9 balloon + the U10 folder picker), added in the commits
  that call it. New `unsafe` is confined to `tray.rs` (balloon) and `folder_dialog.rs` (COM),
  each with a `// SAFETY:` note; `theme.rs` and the glyph rasteriser stay pure safe math.
- **Semantic palette (U2):** the retuned green/amber/orange/red each pass their WCAG bar
  (≥ 3:1 graphical on `#0A0A0A`; ≥ 4.5:1 text for `OK_GREEN`/`ERR_RED` on `#1B1B1B`), share
  the accent's HSV value, and stay mutually distinguishable in the 16-px tray icon.
- **Binary size still < 10 MB** (`just release` prints it; currently 9.0 MB).
- **Cold-open still < 300 ms** (M7 acceptance) — reorder + `set_visuals` + a min-size arg are
  cheap; the existing cold-open latency log confirms it on the Nitro.
- **Responsiveness (U6):** on the Nitro, drag the window to its minimum — confirm every label,
  meter, buffer bar, field, and button is fully visible with no horizontal clipping; grow it
  and confirm the meters/bar widen; confirm the vertical scrollbar appears only when needed.
- **Auto-restart (U7):** open Settings, change **Quality**, Save → the banner appears naming
  "quality"; make a second restart-change + Save → the banner accumulates it; click
  **Restart now** → the app relaunches (same argv), comes back buffering, the tray + hotkeys
  work, and the status strip reflects the new quality/bitrate. Confirm **no** "hotkey already
  in use" failure on relaunch (proves the release-before-spawn ordering). Also verify a
  `clear_after_save`-only change never raises the banner.
- **Manual visual pass (04-TEST-MACHINE):** screenshot the window (lavender selection/links/
  focus/progress; meters green/amber/red first; filled lavender Save; a restart chip on a
  changed field; the banner after a restart-save; tooltips on hover; friendly recent-clip
  times) and the tray glyph across a forced state change.
- **Recording (U8):** start/stop a recording via the tray + record hotkey; confirm the menu
  label flips, the glyph shows the recording mark, and the status "● Recording — MM:SS"
  tracks the real state.
- **Save balloon (U9):** press the save hotkey with the window **closed**; confirm a tray
  balloon ("Clip saved — N s"); force a save failure (unwritable folder) and confirm the
  failure balloon ("check the log").
- **Folder picker (U10):** click Browse…; confirm the native folder chooser opens modal to
  the window, and the chosen path lands in the Output-folder field; Cancel leaves it unchanged.
- **Still owed from M7, unchanged by this pass:** the 2 h open-window soak (zero engine stalls
  attributable to the UI thread) — fold into a longer session before M6 sign-off.

---

## 11. Decisions carried by this plan (log in `DECISIONS.md` 2026-07-08)

- **D-U1** Force dark theme + lavender accent via `set_visuals` (§2). Reversible.
- **D-U2** UI colours live in `src/ui/theme.rs`, **not** `spec_constants.rs`.
- **D-U3** Tray glyph is procedural (hand-rasterized, zero-dep) for the beta; the official
  SVG + embedded `.ico` is **M10**. The window icon reuses the same rasteriser.
- **D-U4** Brand-forward tray: healthy/buffering = lavender; warm colours reserved for
  attention states.
- **D-U5 (new)** **Window minimum size + responsive content.** `with_min_inner_size` floor set
  by the widest fixed row (hotkeys); bars/meters/fields flex to `available_width`. Reversible
  (drop the call → today's clip-on-shrink behaviour).
- **D-U6 (new)** **Auto-restart via signal→teardown→relaunch.** The UI sets a `Shared.restart`
  atomic; the tray tears down as for Quit and returns `ShellOutcome::Restart`; **`main.rs`**
  spawns a fresh `current_exe` with the same argv **after** `pump.join()`, so hotkeys/devices
  are released before the new instance grabs them (no registration-retry hack). Keeps process
  spawning out of `ui` (satellite law intact). Reversible (remove the outcome branch → the
  banner just quits, or is hidden).
- **D-U7 (new, limitation)** The restart-pending set is computed against an `applied` snapshot
  seeded at window creation; a save in a prior session without a restart can under-report.
  Accepted for the beta (documented); a fully-correct `applied` would need the engine to
  publish its started-from config — not worth the coupling now.
- **D-U8 (new, IN-SCOPE)** Recording on/off feedback (§8.1) via a `recording` atomic in
  `EngineStatus` set at the engine's record start/stop sites; surfaced as the tray menu label,
  a recording mark on the glyph, and a "● Recording — MM:SS" status line. Our analogue of
  ShadowPlay's persistent Instant-Replay icon. Reversible.
- **D-U9 (new, IN-SCOPE)** Save-complete/-failed **tray balloon** (§8.2) via a new additive
  `ShellSignal::Saved { ok, seconds }` + a `Shell_NotifyIcon(NIF_INFO)` call (`unsafe`,
  confined to `tray.rs`, `Win32_UI_Shell` gate). Since overlays are a permanent non-goal, the
  OS tray balloon is our honest analogue of Medal's / ShadowPlay's corner "Clip Saved" toast
  (research recorded 2026-07-08). Failure toasts included and prioritised. Reversible.
- **D-U10 (new, IN-SCOPE — was defer, now required)** Native **Browse…** folder picker for
  Output folder (§8.3) via the existing `windows` dep's `IFileOpenDialog` (`FOS_PICKFOLDERS`)
  in a new `src/ui/folder_dialog.rs` COM wrapper — **no `rfd`**. The Save-time
  `validate_output_dir` stays as the backstop. Reversible (remove the button + module).
- **D-U11 (new, IN-SCOPE)** **Value-harmonised semantic palette** (§1.1): green/amber/orange/
  red retuned to the accent's HSV value (softer/brighter family), holding hue, red keeping
  more saturation to stay "danger", all re-validated for WCAG AA + tray distinguishability.
  Reversible (revert the four consts in `theme.rs`).
- **Name deferred to M10** — `clipd` retained.

---

## 12. Out of scope (ratchet)

No new UI crate or webview; no tabs / redesign / theme switcher; no editor features; no clip
trimming; **no in-game overlay** (the incumbents' "Clip Saved" is drawn in-game — we
deliberately use a tray balloon instead, §8.0); SVG logo, `.exe` icon embedding, code signing,
winget/installer/Steam packaging, and the optional save *sound* are all **M10**
(`08-FEATURE-COMPLETE.md`). A native file-dialog crate (`rfd`) and a toast crate are **out**
(whitelist) — the §8 balloon and folder picker use the existing `windows` dep only. The
satellite law and the single `Config::write_atomic` write path are invariant — this pass
changes presentation, adds two additive engine→ui signals (recording state, save outcome) and
a signalled relaunch, and never makes the engine depend on the UI.
```
