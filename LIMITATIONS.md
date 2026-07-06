# clipd — honest limitations

The load-bearing list (`01-PROJECT-PLAN.md §6 M5`). Seeded during M4 (window mode),
grown in M5 (shell & trust). The full settings UI is M7; the standalone
troubleshooting page is M10.

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

## Shell / tray (M5)

- **Pause stops *retaining* new footage but keeps capturing.** When you pick **Pause**,
  clipd stops adding new frames to the replay buffer (so nothing during the paused span
  can ever be saved) and **keeps the existing buffer** — a save while paused still writes
  your pre-pause footage. But capture and hardware encode keep running, so pause does
  **not** drop CPU/GPU usage to zero; it is a privacy/retention control, not a power
  toggle. (A true "suspend capture" mode is a later `buffer_when` policy.) You also can't
  start a recording while paused. Resuming leaves a gap in the buffer across the pause.
- **Global hotkeys use `RegisterHotKey`; some exclusive-fullscreen games swallow them.**
  If your save/record hotkey does nothing while a game is focused, the game has grabbed
  the key at a lower level. Use the **tray menu** (Save clip / Record) instead, run the
  game **borderless**, or rebind to a combo the game doesn't use.

## Sync / saves

- **"Why didn't my clip save?" — read the log.** Every save attempt writes a line with
  its outcome to the rotating log at **`%LOCALAPPDATA%\clipd\logs\`** (daily-rolled).
  A missing clip has a reason there: `clip saved` (with the path and write time),
  `clip saved (slow write — disk suspect)`, `clip save FAILED` (with the error), or
  `save skipped` (buffer not ready / paused span). A `§6.3` divergence turns the tray
  icon to its **warning** colour and logs `encoder/mux falling behind`.
- **Early-save mic alignment is handled.** A clip saved in the first moments of a fresh
  buffer used to carry a few tens of ms of silent lead on the mic track (WASAPI's
  first-audio latency). clipd now synthesizes the leading silence so late-starting tracks
  begin at the clip origin within ≤ 1 AAC frame. (Not a limitation anymore — noted so the
  old behaviour isn't mistaken for a regression.)

## Platform / content

- **DRM-protected content captures as black frames** by design (Netflix in a browser
  with hardware DRM, some launchers). Not a bug; not something clipd will bypass.
- **HDR is tone-mapped to SDR** (HDR passthrough is a later milestone).
- **Windows 10 1903+ / Windows 11 only.** No cross-platform build in v1.
- **A hardware encoder is required** (NVENC / QSV / AMF via Media Foundation). There is
  no software-encode fallback; a machine with no hardware encoder gets a clear error.
