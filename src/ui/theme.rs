//! `ui::theme` — the single source of truth for the settings-window + tray colours,
//! plus the procedural "last-slice" glyph shared by the tray icon and the window icon.
//!
//! ## Why here and not `spec_constants.rs` (D-U2)
//! `spec_constants.rs` is reserved for `02-AV-SYNC-SPEC.md` numbers. These are **UI**
//! constants — the accent + the value-harmonised semantic palette — so they live in
//! one place under `ui`, mirroring how `tray.rs::state_color` was already documented as
//! "the single place to re-theme." `tray.rs` (as `[u8; 4]`) and `settings.rs` both
//! reference these, retiring the duplicated inline literals.
//!
//! ## The palette (UI-PASS-PLAN §1 / §1.1)
//! The accent is contrast-calculated per WCAG 2.1 relative luminance against egui
//! 0.35's real dark surfaces — `panel_fill` `#1B1B1B` and `extreme_bg_color` `#0A0A0A`.
//! The four semantic colours are retuned to share the accent's HSV **value** (~0.98) so
//! the whole window reads as one soft, bright system instead of a pastel accent floating
//! over darker traffic-lights; each keeps its hue (green / amber / orange / red) and red
//! keeps more saturation so it still reads as *danger*. The WCAG bars (graphical ≥ 3:1 on
//! the meter track, text ≥ 4.5:1 for the two that are also text) are asserted in tests,
//! not eyeballed.
//!
//! Everything here is pure safe math — no Win32/GDI — so the palette and the glyph
//! rasteriser are unit-testable.

use eframe::egui::{self, Color32};

// ── Accent (D-U1). Contrast-calculated vs egui 0.35 dark `panel_fill` `#1B1B1B` and
//    `extreme_bg_color` `#0A0A0A`. ────────────────────────────────────────────────

/// Primary lavender accent — links, focus stroke, progress fill, active toggle,
/// selection stroke, the healthy tray state, the restart banner. 6.3:1 on `#1B1B1B`,
/// 7.3:1 on `#0A0A0A` (AA text + graphical).
pub const ACCENT: Color32 = Color32::from_rgb(0xA7, 0x8B, 0xFA);
/// Hovered link / bright emphasis / peak tip. 9.3:1 on `#1B1B1B`.
pub const ACCENT_HOVER: Color32 = Color32::from_rgb(0xC4, 0xB5, 0xFD);
/// Selection **background** + filled-button fill (light text on top). Fill only,
/// never text (fg-on-bg 2.4:1); light text on it is 4.8:1 (AA).
pub const ACCENT_FILL: Color32 = Color32::from_rgb(0x5B, 0x4B, 0x9E);

// ── Value-harmonised semantic palette (D-U11, §1.1). Hue preserved, V ≈ 0.98, S pulled
//    into the accent's band; red kept more saturated so it still says "stop". ────────

/// Nominal / OK / healthy — the VU meter's green band, the "buffering" state dot, and
/// the editor's save-OK line. green (H 129°, S 0.50, V 0.98).
pub const GOOD: Color32 = Color32::from_rgb(0x7D, 0xFA, 0x8F);
/// Paused / hot — the "paused" state and the VU meter's near-clip amber band.
/// amber (H 43°, S 0.50, V 0.98).
pub const AMBER: Color32 = Color32::from_rgb(0xFA, 0xD6, 0x7D);
/// Warning — the `§6.3` watchdog "warning" state. orange (H 36°, S 0.50, V 0.98);
/// kept ~7° off `AMBER` so the two stay distinguishable in the 16-px tray icon.
pub const WARN: Color32 = Color32::from_rgb(0xFA, 0xC8, 0x7D);
/// Error / near-clip / save-failed — the "error" state, the VU meter's clip band, and
/// the editor's error line. red (H 5°, S 0.62, V 0.98) — more saturated than the rest
/// so it still reads as *danger* at V 0.98.
pub const BAD: Color32 = Color32::from_rgb(0xFA, 0x6D, 0x5F);

/// Text/foreground drawn on top of an `ACCENT_FILL` filled button (the promoted Save).
/// Near-white for a comfortable 4.8:1 on the fill.
pub const ON_FILL: Color32 = Color32::from_rgb(0xF5, 0xF3, 0xFF);

/// egui default dark surfaces the palette was calculated against — used by the glyph
/// rasteriser for the carved-track shading and kept here so the reference lives beside
/// the numbers that assume it.
const PANEL_FILL: Color32 = Color32::from_rgb(0x1B, 0x1B, 0x1B);

/// Build the window's [`egui::Visuals`]: egui `dark()` plus the minimal, surgical
/// accent overrides ("one accent"). Applied once at window creation (D-U1: forces dark
/// regardless of the system light theme — M7 mandates "dark, dense, quiet", and the
/// meters/status chrome assume a dark ground). The theme-adaptive reads elsewhere
/// (`extreme_bg_color`, `strong_text_color()`) keep working against the forced dark
/// visuals. Reversible (drop the `set_visuals` call → egui default dark).
pub fn configure_visuals() -> egui::Visuals {
    let mut v = egui::Visuals::dark();
    let accent_stroke = egui::Stroke::new(1.0, ACCENT);
    v.hyperlink_color = ACCENT;
    v.selection.bg_fill = ACCENT_FILL;
    v.selection.stroke = accent_stroke;
    // Focus/active reads lavender IN ADDITION to egui's shape change — never colour-only.
    v.widgets.hovered.bg_stroke = accent_stroke;
    v.widgets.active.bg_stroke = accent_stroke;
    v
}

// ── The procedural "last-slice" glyph (D-U3) ──────────────────────────────────────

/// Supersample factor: the glyph is drawn at `SUPERSAMPLE`× the target edge and
/// box-downsampled (alpha-weighted) for clean edges. Pure integer/float math, no dep.
const SUPERSAMPLE: u32 = 4;

/// Render the "last-slice" glyph at `size`×`size` in straight RGBA8, tinted with the
/// opaque state colour `chip` (`[r, g, b, a]`).
///
/// A rounded chip filled with the state colour, a thin horizontal track carved out of
/// it (the elapsed buffer — knocked through to transparency), the **kept tail** (the
/// right ~40% of the track) painted back in the chip colour, and a bright **playhead**
/// at the live edge. Supersampled + alpha-weighted downsampled so the carved edges and
/// chip corners antialias without a dark halo. Pure — unit-testable (no Win32/GDI).
///
/// This is the one pixel producer behind both the tray icon ([`super::tray`]) and the
/// window icon ([`window_icon`]); switching to designed SVG/`.ico` art at M10 replaces
/// only this function.
pub fn glyph_rgba(chip: [u8; 4], size: u32) -> Vec<u8> {
    let n = size * SUPERSAMPLE;
    let nf = n as f32;

    // Chip: a rounded rect inset a little from the edge.
    let m = nf * 0.08;
    let radius = (nf - 2.0 * m) * 0.22;
    // Track: a thin horizontal band across the chip.
    let cy = nf * 0.5;
    let th = nf * 0.11;
    let (ty0, ty1) = (cy - th, cy + th);
    let tx0 = m + nf * 0.06;
    let tx1 = nf - m - nf * 0.06;
    // Kept tail starts 60% along the track (right ~40% is kept); the playhead sits there.
    let xk = tx0 + (tx1 - tx0) * 0.60;
    let ph_half = SUPERSAMPLE as f32 * 0.9; // ≈ 1 output px wide

    // The carved track shows a hint of the panel behind rather than pure transparency,
    // so the slice reads even when the tray composites the icon over a light taskbar.
    let carved = premul_over_transparent(PANEL_FILL);
    let playhead = [0xF5u8, 0xF3, 0xFF, 0xFF]; // bright live edge

    let mut hi = vec![0u8; (n * n * 4) as usize];
    for y in 0..n {
        for x in 0..n {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let px = if !in_rounded_rect(fx, fy, m, m, nf - m, nf - m, radius) {
                [0, 0, 0, 0]
            } else {
                let in_track = fy >= ty0 && fy <= ty1 && fx >= tx0 && fx <= tx1;
                if in_track && (fx - xk).abs() <= ph_half {
                    playhead
                } else if in_track && fx < xk {
                    carved
                } else {
                    chip
                }
            };
            let o = ((y * n + x) * 4) as usize;
            hi[o..o + 4].copy_from_slice(&px);
        }
    }
    downsample(&hi, n, SUPERSAMPLE, size)
}

/// The window icon (taskbar / Alt-Tab / title bar): the healthy-state glyph in the
/// lavender accent, at a comfortable 32 px. Reuses [`glyph_rgba`]; zero new dep.
pub fn window_icon() -> egui::IconData {
    const ICON: u32 = 32;
    egui::IconData {
        rgba: glyph_rgba(ACCENT.to_array(), ICON),
        width: ICON,
        height: ICON,
    }
}

/// The carved-track fill: the panel colour at reduced opacity, given straight (the
/// downsampler premultiplies by alpha), so the slice reads as a recessed groove rather
/// than a fully transparent hole.
fn premul_over_transparent(c: Color32) -> [u8; 4] {
    let [r, g, b, _] = c.to_array();
    [r, g, b, 0x66] // ~40% opacity
}

/// Whether the point `(px, py)` is inside the rounded rectangle `[x0, x1] × [y0, y1]`
/// with corner radius `r` (distance-to-corner-arc test). Pure.
fn in_rounded_rect(px: f32, py: f32, x0: f32, y0: f32, x1: f32, y1: f32, r: f32) -> bool {
    if px < x0 || px > x1 || py < y0 || py > y1 {
        return false;
    }
    let cxl = x0 + r;
    let cxr = x1 - r;
    let cyt = y0 + r;
    let cyb = y1 - r;
    let dx = if px < cxl {
        cxl - px
    } else if px > cxr {
        px - cxr
    } else {
        0.0
    };
    let dy = if py < cyt {
        cyt - py
    } else if py > cyb {
        py - cyb
    } else {
        0.0
    };
    dx * dx + dy * dy <= r * r
}

/// Alpha-weighted box downsample of a straight-RGBA `src` (`n`×`n`) to `size`×`size`
/// by averaging `ss`×`ss` blocks. Colour is weighted by each sample's alpha so a fully
/// transparent sample contributes no colour (no dark halo at the carved edges); alpha
/// is a plain area average. Pure.
fn downsample(src: &[u8], n: u32, ss: u32, size: u32) -> Vec<u8> {
    let mut out = vec![0u8; (size * size * 4) as usize];
    let area = ss * ss;
    for oy in 0..size {
        for ox in 0..size {
            let mut rgb = [0u32; 3];
            let mut asum = 0u32;
            for sy in 0..ss {
                for sx in 0..ss {
                    let x = ox * ss + sx;
                    let y = oy * ss + sy;
                    let o = ((y * n + x) * 4) as usize;
                    let a = src[o + 3] as u32;
                    rgb[0] += src[o] as u32 * a;
                    rgb[1] += src[o + 1] as u32 * a;
                    rgb[2] += src[o + 2] as u32 * a;
                    asum += a;
                }
            }
            let o = ((oy * size + ox) * 4) as usize;
            // Colour is alpha-weighted: fully transparent samples contribute no colour
            // (no dark halo). `denom` guards the all-transparent block (rgb is 0 there).
            let denom = asum.max(1);
            out[o] = (rgb[0] / denom) as u8;
            out[o + 1] = (rgb[1] / denom) as u8;
            out[o + 2] = (rgb[2] / denom) as u8;
            out[o + 3] = (asum / area) as u8;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// WCAG 2.1 relative luminance of an sRGB colour.
    fn luminance(c: Color32) -> f64 {
        fn ch(v: u8) -> f64 {
            let c = v as f64 / 255.0;
            if c <= 0.03928 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        }
        let [r, g, b, _] = c.to_array();
        0.2126 * ch(r) + 0.7152 * ch(g) + 0.0722 * ch(b)
    }

    /// WCAG contrast ratio between two colours.
    fn contrast(a: Color32, b: Color32) -> f64 {
        let (la, lb) = (luminance(a), luminance(b));
        let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
        (hi + 0.05) / (lo + 0.05)
    }

    const EXTREME_BG: Color32 = Color32::from_rgb(0x0A, 0x0A, 0x0A);

    #[test]
    fn accent_clears_aa_on_both_dark_surfaces() {
        // The accent is used as text (links) and graphically (strokes/fill) — AA on both.
        assert!(
            contrast(ACCENT, PANEL_FILL) >= 4.5,
            "accent AA text on panel"
        );
        assert!(
            contrast(ACCENT, EXTREME_BG) >= 3.0,
            "accent graphical on extreme"
        );
        assert!(contrast(ACCENT_HOVER, PANEL_FILL) >= 4.5);
    }

    #[test]
    fn semantic_palette_is_value_harmonised_and_passes_wcag() {
        // Graphical AA (≥ 3:1) on the meter track for all four; text AA (≥ 4.5:1) on the
        // panel for the two that are also drawn as text (GOOD save-OK line, BAD error).
        for c in [GOOD, AMBER, WARN, BAD] {
            assert!(
                contrast(c, EXTREME_BG) >= 3.0,
                "{c:?} must clear 3:1 graphical on the meter track"
            );
        }
        assert!(contrast(GOOD, PANEL_FILL) >= 4.5, "GOOD is also text");
        assert!(contrast(BAD, PANEL_FILL) >= 4.5, "BAD is also text");

        // Value-harmonised: each shares the accent's HSV value (~0.98) → max channel high.
        for c in [GOOD, AMBER, WARN, BAD] {
            let v = c.to_array()[..3].iter().copied().max().unwrap();
            assert!(v >= 0xF0, "{c:?} value must be ~0.98 (max channel ≥ 0xF0)");
        }
        // Red keeps more saturation than the softened trio so it still reads as danger.
        let sat = |c: Color32| {
            let [r, g, b, _] = c.to_array();
            let mx = r.max(g).max(b);
            let mn = r.min(g).min(b);
            (mx - mn) as f32 / mx.max(1) as f32
        };
        assert!(sat(BAD) > sat(GOOD) && sat(BAD) > sat(AMBER) && sat(BAD) > sat(WARN));
    }

    #[test]
    fn semantic_colours_are_mutually_distinct() {
        let all = [ACCENT, GOOD, AMBER, WARN, BAD];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a.to_array(), b.to_array(), "{a:?} and {b:?} collide");
            }
        }
    }

    #[test]
    fn glyph_rgba_is_sized_and_not_a_solid_fill() {
        let size = 32u32;
        let chip = ACCENT.to_array();
        let px = glyph_rgba(chip, size);
        assert_eq!(px.len(), (size * size * 4) as usize);

        // A solid interior point above the carved track is exactly the chip colour
        // (every subsample is chip → the average is the chip colour).
        let at = |x: u32, y: u32| {
            let o = ((y * size + x) * 4) as usize;
            [px[o], px[o + 1], px[o + 2], px[o + 3]]
        };
        assert_eq!(
            at(size / 2, size / 4),
            chip,
            "chip body should be solid state colour"
        );

        // A point in the carved (elapsed) portion of the track differs from the chip
        // body — it is knocked through to the low-opacity groove, not the solid chip.
        let carved = at((size as f32 * 0.30) as u32, size / 2);
        assert_ne!(
            carved, chip,
            "the carved track must differ from the chip body"
        );
        assert!(
            carved[3] < chip[3],
            "the carved track is more transparent than the chip"
        );
    }

    #[test]
    fn window_icon_is_32px_rgba() {
        let icon = window_icon();
        assert_eq!(icon.width, 32);
        assert_eq!(icon.height, 32);
        assert_eq!(icon.rgba.len(), (32 * 32 * 4) as usize);
    }
}
