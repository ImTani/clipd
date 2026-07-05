# clipd — honest limitations

The load-bearing list (`01-PROJECT-PLAN.md §6 M5`). Seeded during M4 (window mode);
the tray/settings surface and the full "why didn't my clip save" page are M5/M10.

## Capture

- **Window capture uses a FIXED canvas — resized windows get black bars.** When you
  capture a focused window, the clip is encoded at a fixed resolution (your monitor's
  resolution, capped at `[encode].max_height`, chosen when the buffer starts). If you
  resize the window — or drag it to a monitor with a different DPI — its content is
  **scaled to fit and centered** in that canvas, adding **letterbox/pillarbox black
  bars** when the aspect ratio changes. This is deliberate (it lets a single clip span
  a resize at one resolution, instead of cutting your history every time you resize).
  Restart the buffer to re-base the canvas on the current window size. clipd never
  stretches/distorts the image.
  - **While you are actively dragging a resize**, the vacated area of the canvas can
    briefly show stale/duplicated pixels (WGC composites the resized window into the
    not-yet-recreated frame pool). It self-corrects the moment the resize settles
    (~0.4 s after you stop). Only visible mid-drag; not something you would normally
    be saving.
- **Exclusive-fullscreen games can't be window-captured.** A true exclusive-fullscreen
  title delivers no frames to a window capture; clipd detects this (no frame within
  1 s) and **falls back to capturing the monitor**, logging the switch. Borderless /
  windowed-fullscreen (what most modern games use) capture normally. Recommendation:
  run games borderless.
- **Closing the captured window falls back to the monitor — without cutting the clip.**
  If you close the window clipd is capturing, the buffer switches to the primary
  monitor scaled into the same canvas (no epoch), so a clip **keeps your pre-close
  window footage** and continues with the monitor after it. You may see a content jump
  (and a letterbox change if aspects differ) at the switch, but nothing is lost.
- **`focused-window` captures whatever is foreground when the buffer starts.** It is
  resolved once, at start — launched from a terminal, that is the terminal window. The
  tray (M5) makes this ergonomic. A window that is not capturable (e.g. elevated /
  protected) falls back to the monitor.
- **Cursor:** composited per `[capture].cursor`. Recommended off for game windows, on
  for the desktop/monitor (a per-target auto-default is an M7 settings refinement).

## Sync / saves

- **A clip saved in the first ~N seconds of a fresh buffer may have up to ~60 ms of
  silent lead on the mic track.** The mic (WASAPI) takes a few tens of ms to deliver
  its first audio after launch; a save whose window starts at capture-start reflects
  that. In normal continuous use (buffer always full) this never shows. (Follow-up:
  synthesize leading silence at save time.)

## Platform / content

- **DRM-protected content captures as black frames** by design (Netflix in a browser
  with hardware DRM, some launchers). Not a bug; not something clipd will bypass.
- **HDR is tone-mapped to SDR** (HDR passthrough is a later milestone).
- **Windows 10 1903+ / Windows 11 only.** No cross-platform build in v1.
- **A hardware encoder is required** (NVENC / QSV / AMF via Media Foundation). There is
  no software-encode fallback; a machine with no hardware encoder gets a clear error.
