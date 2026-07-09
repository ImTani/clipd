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

## Multi-track audio (Slice B)

By default clipd records **two audio tracks**: a **Mix** (your desktop audio + mic,
summed) and the **Mic** on its own. Turn on `[audio] separate_tracks = true` and clipd
adds up to three more system tracks — **Game**, **Voice chat**, and **Other system** —
so you can rebalance them in an editor. The honest limits of that split:

- **In-game voice chat can NEVER be separated.** Games that run voice *inside their own
  process* (Vivox/EOS/Steamworks — Valorant, Fortnite, Apex, League, etc.) mix that voice
  into the game's own audio before clipd ever sees it. It lands on the **Game** track with
  the game, and there is no way to pull it out. Only a *separate* voice app (Discord and
  the like) can get its own track.
- **The "Other system" track double-counts voice chat.** "Other system" is *all system
  audio except the game* (clipd excludes the bound game's process tree). Windows' API
  can't express "everything except the game **and** except Discord", so a detected voice
  app still bleeds into **Other system** as well as its own **Voice chat** track. If your
  editor keeps both tracks enabled, that voice is played **twice** (louder). Mute one of
  the two in the editor, or don't enable both. The **Mix** track never has this problem —
  it is summed once and is the right choice for a single-track upload.
- **Voice chat = the whole app, not just speech.** The Voice-chat track carries
  *everything* the voice app plays: other people's mics, join/leave pings, the soundboard,
  a Go-Live/stream you're watching in it. That is by design (clipd captures the app's whole
  audio tree); it is not just the people talking.
- **Voice chat is detected by process, not by any game database.** clipd matches the app's
  executable name against the `[[audio.vc_apps]]` list (Discord and its variants are
  seeded). Browser-based voice (Discord/Teams/Meet in a tab) is **not** captured as a
  separate track — it can't be told apart from the rest of the browser — and lands in
  Mix/Other-system instead. Add other desktop voice apps to that config list yourself.
- **Which game is "the game" is a live guess, and switching it leaves a gap.** In monitor
  capture clipd binds the game track to whatever is **fullscreen/borderless in the
  foreground**; alt-tabbing to a *different* fullscreen app retargets it, and the moment it
  retargets (or a game opens/closes) the Game and Other-system tracks get a **brief silence
  gap** while the capture re-binds. No game-title database is involved (that's a non-goal).
- **Per-app tracks need Windows 10 version 2004 (build 19041) or newer.** They rely on
  process-loopback capture, which older Windows 10 lacks. Below that floor the Game / Voice
  chat / Other-system tracks are silently hidden and you get the Mix + Mic pair — the
  replay buffer is otherwise unaffected.
- **Uploads and most players hear only the Mix.** Discord, browsers, and single-track
  players read **track 1 (Mix)** and ignore the rest — which is exactly why Mix is first
  and always present. The extra tracks are for editors (CapCut, Premiere, DaVinci) that
  import every enabled track.

## Platform / content

- **DRM-protected content captures as black frames** by design (Netflix in a browser
  with hardware DRM, some launchers). Not a bug; not something clipd will bypass.
- **HDR is tone-mapped to SDR** (HDR passthrough is a later milestone).
- **Windows 10 1903+ / Windows 11 only.** No cross-platform build in v1.
- **A hardware encoder is required** (NVENC / QSV / AMF via Media Foundation). There is
  no software-encode fallback; a machine with no hardware encoder gets a clear error.

## Notifications

- **Windows hides the save toast while you're gaming — this is a Windows policy, not a
  clipd bug.** clipd shows a native notification-area balloon on every save (success with
  the clip length, failure with the reason) from its single tray icon. But Windows 11
  **auto-enables Do Not Disturb "when playing a game"**, which suppresses the balloon
  during the exact moment you're most likely to clip — and it's **game detection, not
  fullscreen**, so even a *bordered* game window triggers it. Exclusive-fullscreen and a
  manually-enabled Focus Assist / Do Not Disturb suppress it too. **The save still
  happened either way.** A suppressed toast is not lost — it still lands in the Windows
  **notification center** (Action Center) for later, and clicking it opens the clip
  folder (a failure opens the log folder).
- **What's authoritative in-game:** the **save sound** (a short tone on success — audio is
  the one channel Windows doesn't gate; on by default, turn it off under Settings ▸ "Play
  a sound when saved"), the **tray icon**, and the **log**. The toast is a convenience for
  when you're back on the desktop, never the source of truth.
- **The save sound is captured into later clips.** Because it plays out your default
  speakers/headphones, the tone lands in the desktop-audio track of footage buffered
  *after* it. The built-in tone is short and quiet to keep that mark negligible; if you
  set a custom `.wav`, keep it short and quiet for the same reason.
