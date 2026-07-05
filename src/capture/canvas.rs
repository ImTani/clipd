//! `capture::canvas` — the fixed output canvas and letterbox geometry for window
//! mode (M4-2 amendment, DECISIONS 2026-07-05).
//!
//! A *window resize* must not truncate replay clips (`§0` forbids a clip spanning
//! epochs, so an epoch-per-resize clamps every save to since-the-resize). Instead
//! the encoded resolution is **fixed at buffer start** (pitfall 11): the video
//! processor rescales the resized window content into that fixed **canvas**, so the
//! encoder never changes, no epoch starts, and a clip spans resizes.
//!
//! - [`canvas_size`] — the canvas: the capture monitor's resolution capped at a
//!   configured encode-height ceiling, rounded to even (NV12/H.264 need even).
//! - [`letterbox_rect`] — where the (possibly different-aspect) content is placed
//!   inside the canvas: scaled-to-fit and centered (letterbox / pillarbox), never
//!   stretched. The rest of the canvas is filled with black.
//!
//! Pure integer geometry — 100% safe, no COM, unit-tested (`CLAUDE.md`).

/// Round `v` down to the nearest even value, floored at 2 (NV12 4:2:0 needs even
/// width and height, and a zero dimension is invalid).
fn even_floor(v: u32) -> u32 {
    (v & !1).max(2)
}

/// The fixed output canvas for a capture monitor of `monitor` (w, h): the monitor
/// resolution, scaled down uniformly to fit within `max_height` if it is taller,
/// with both dimensions rounded to even. Aspect is preserved so a full-monitor
/// capture fills the canvas exactly (no bars); a window is letterboxed within it via
/// [`letterbox_rect`].
pub fn canvas_size(monitor: (u32, u32), max_height: u32) -> (u32, u32) {
    let (mw, mh) = (monitor.0.max(1), monitor.1.max(1));
    let cap = max_height.max(2);
    if mh <= cap {
        (even_floor(mw), even_floor(mh))
    } else {
        // Scale to `cap` height, preserving aspect (128-bit-free: u64 intermediate).
        let w = (mw as u64 * cap as u64 / mh as u64) as u32;
        (even_floor(w), even_floor(cap))
    }
}

/// The destination rectangle `(left, top, right, bottom)` for placing `input`
/// content, scaled-to-fit and centered, inside a `canvas`-sized output — the video
/// processor's stream dest rect. Aspect is preserved (letterbox top/bottom or
/// pillarbox left/right); the uncovered canvas is black. All edges are even so the
/// rect aligns to NV12 chroma.
pub fn letterbox_rect(input: (u32, u32), canvas: (u32, u32)) -> (i32, i32, i32, i32) {
    let (iw, ih) = (input.0.max(1) as u64, input.1.max(1) as u64);
    let (cw, ch) = (canvas.0.max(2) as u64, canvas.1.max(2) as u64);

    // Compare aspects by cross-multiplication (no float): input narrower-or-equal
    // than the canvas ⇒ fit to canvas height (pillarbox); else fit to width.
    let (fit_w, fit_h) = if iw * ch <= ih * cw {
        (iw * ch / ih, ch)
    } else {
        (cw, ih * cw / iw)
    };
    let fit_w = even_floor(fit_w as u32).min(canvas.0);
    let fit_h = even_floor(fit_h as u32).min(canvas.1);
    let x = even_floor_pos(canvas.0 - fit_w);
    let y = even_floor_pos(canvas.1 - fit_h);
    (x as i32, y as i32, (x + fit_w) as i32, (y + fit_h) as i32)
}

/// Half of `span`, rounded down to even (the centered offset for a letterbox bar).
fn even_floor_pos(span: u32) -> u32 {
    (span / 2) & !1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canvas_is_monitor_res_when_under_ceiling() {
        assert_eq!(canvas_size((1920, 1080), 2160), (1920, 1080));
        assert_eq!(canvas_size((1920, 1080), 1080), (1920, 1080)); // exactly at cap
        assert_eq!(canvas_size((2560, 1440), 2160), (2560, 1440));
    }

    #[test]
    fn canvas_scales_down_to_ceiling_preserving_aspect() {
        // 4K capped at 1080 → 1920x1080 (16:9 preserved).
        assert_eq!(canvas_size((3840, 2160), 1080), (1920, 1080));
        // 2560x1440 capped at 1080 → 1920x1080.
        assert_eq!(canvas_size((2560, 1440), 1080), (1920, 1080));
    }

    #[test]
    fn canvas_dimensions_are_even() {
        // An odd monitor width (unusual, but be safe) rounds down to even.
        let (w, h) = canvas_size((1921, 1081), 4320);
        assert_eq!((w % 2, h % 2), (0, 0));
        assert_eq!((w, h), (1920, 1080));
    }

    #[test]
    fn same_aspect_content_fills_the_canvas_no_bars() {
        // A full-monitor (or same-aspect window) fills the canvas exactly.
        assert_eq!(
            letterbox_rect((1920, 1080), (1920, 1080)),
            (0, 0, 1920, 1080)
        );
        // Half-size 16:9 window in a 16:9 canvas → centered, no bars beyond fit.
        assert_eq!(letterbox_rect((960, 540), (1920, 1080)), (0, 0, 1920, 1080));
    }

    #[test]
    fn narrow_window_pillarboxes_left_right() {
        // 4:3 (800x600) into 16:9 (1920x1080): fit to height 1080 → w = 1440,
        // centered x = (1920-1440)/2 = 240.
        assert_eq!(
            letterbox_rect((800, 600), (1920, 1080)),
            (240, 0, 1680, 1080)
        );
    }

    #[test]
    fn wide_window_letterboxes_top_bottom() {
        // 21:9 (2560x1080) into 16:9 (1920x1080): fit to width 1920 → h = 810,
        // centered y = (1080-810)/2 = 135 → even_floor 134.
        let (l, t, r, b) = letterbox_rect((2560, 1080), (1920, 1080));
        assert_eq!((l, r), (0, 1920));
        assert_eq!(b - t, 810);
        assert_eq!(t % 2, 0); // even-aligned bar
    }

    #[test]
    fn letterbox_edges_are_even() {
        for input in [(801, 601), (1000, 999), (333, 777), (1919, 1079)] {
            let (l, t, r, b) = letterbox_rect(input, (1920, 1080));
            assert_eq!(l % 2, 0);
            assert_eq!(t % 2, 0);
            assert_eq!((r - l) % 2, 0, "width even for {input:?}");
            assert_eq!((b - t) % 2, 0, "height even for {input:?}");
            // Stays within the canvas.
            assert!(r <= 1920 && b <= 1080 && l >= 0 && t >= 0);
        }
    }
}
