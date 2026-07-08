# UI Research — how the incumbents present capture settings (2026-07-08)

Research pass (3 parallel agents) into **Medal.tv/Outplayed**, **NVIDIA ShadowPlay /
NVIDIA App** (+ **AMD ReLive** contrast), and **UX/interaction-design principles**, to
inform a presentation + information-architecture redesign of clipd's settings window. The
current window mixes an engineering-flavoured settings form (quality tier · resolution ·
fps · **buffer length (seconds)** · folder · clear-after-save · desktop audio · mic ·
hotkeys) with a live **Status** strip (engine state · capture target · **buffer-fill %** ·
**frame counters** · last-save time). The user's critique: too advanced, too dense, status
doesn't belong in Settings, terminology is for people who already know.

This doc is the durable record; citations inline. Convergent findings first, then the
mapped redesign, then the open decisions.

---

## 1. The four findings all three sources agree on

### F1 — "Buffer" is jargon; nobody in this category uses it
No consumer capture tool aimed at gamers uses "buffer" in its primary UI. The universal
lay framing is **time-of-the-past-kept, attached to the save action**:
- **NVIDIA ShadowPlay / NVIDIA App:** a slider labeled **"Instant Replay length"**, range
  **15 s – 20 min**, common default **5 min** ([EaseUS](https://recorder.easeus.com/screen-recording-resource/how-to-use-nvidia-shadowplay.html), [techguides.yt](https://techguides.yt/guides/geforce-experience-best-recording-quality-settings/)).
- **AMD ReLive:** slider **"Instant Replay Duration"**, 30-s steps to 20 min, default **1 min** ([AMD DH-023](https://www.amd.com/en/resources/support-articles/faqs/DH-023.html)).
- **Xbox Game Bar / Windows:** **"Record the last ___"** dropdown (15 s / 30 s / 1 / 3 / 5 /
  10 min); copy: *"capture the moment before it's gone forever"* ([Xbox support](https://support.xbox.com/en-US/help/games-apps/game-setup-and-play/adjust-capture-settings-windows-10)).
- **Medal:** reframes it entirely as a **per-hotkey clip length** (15 s – 5 min per key) —
  *"only the length of your clip is stored"* — the buffer is presented as *the clip you get*,
  not a memory setting ([Medal: How to Make Clips](https://support.medal.tv/support/solutions/articles/48001157618-how-to-make-clips)).
- Principle: Nielsen heuristic **#2 Match between system and the real world** — "speak the
  users' language… avoid technical jargon" ([NN/g](https://www.nngroup.com/articles/match-system-real-world/)).

→ **Rename "buffer length (seconds)" → "Instant Replay length" / "Keep the last ___".**
"Instant Replay" is the most recognised gamer term for exactly clipd's core feature.

### F2 — Bitrate is the least-understood knob; hide it behind a Quality preset. Resolution/fps are fine to show
- Bitrate "is often confused with resolution… the average person is more familiar with the
  latter" ([VdoCipher](https://www.vdocipher.com/blog/2020/09/video-quality-bitrate-pixels/)); consumer tools "handle bitrate optimization behind the scenes."
- **OBS Simple mode** replaces the raw bitrate field with a **named quality-vs-file-size
  preset**: "High Quality, Medium File Size" / "Indistinguishable Quality, Large File Size" /
  "Lossless Quality, Tremendously Large File Size" ([OBS KB](https://obsproject.com/kb/standard-recording-output-guide)); the Mbps field only appears in **Advanced** ([OBS Advanced](https://obsproject.com/kb/advanced-recording-settings-guide)).
- **NVIDIA App:** **Quality preset Low / Medium / High** (19.2 / 24 / 28.8 Mbps); a Custom
  **Mbps slider** appears only under "Custom" ([TechSpot](https://www.techspot.com/guides/3131-nvidia-app/)).
- **Medal:** **Low (360p) / Standard (720p) / High (1080p) / Custom**; raw bitrate (3M–100M)
  only under Custom ([Medal Quality Guide](https://support.medal.tv/support/solutions/articles/48001159618-video-quality-settings-guide)).
- **Streamlabs:** a one-click **"Auto Optimize"** so the lay user never types a number ([Streamlabs](https://streamlabs.com/content-hub/post/how-to-optimize-your-settings-for-streamlabs-desktop)).
- Resolution (720p/1080p/4K) and framerate (30/60) are the vocabulary consumers already own
  and every tool exposes them as picker chips/dropdowns; both incumbents **default resolution
  to "In-Game / Source"** and **fps to 60** so the first-timer needn't choose.

→ clipd already hides bitrate behind a **Quality tier** (Efficient/Default/High/Max) — keep
that, never surface Mbps. Default resolution to **Source (native)**, fps to **60**, and demote
both to Advanced. Consider an **"Auto / Recommended"** quality default.

### F3 — Status/telemetry does NOT belong in the Settings body (the strongest, most unanimous signal)
Neither incumbent puts engine state, buffer-fill %, or frame counters anywhere in the config
UI. "Settings you set" and "status you monitor" are separate surfaces:
- **NVIDIA:** an ambient **corner status dot** ("it's buffering") that is itself an optional
  overlay element, + a **corner toast** on save ("a notification… letting you know a recording
  has been saved"); **no** buffer %, **no** frame counters, **no** last-save timestamp anywhere
  ([HowToGeek](https://www.howtogeek.com/271199/how-to-hide-the-nvidia-geforce-experiences-in-game-overlay-icons/), [NVIDIA Highlights](https://www.nvidia.com/en-us/geforce/news/shadowplay-highlights-tutorial/)).
- **Medal:** engine state = a **game-name / "Waiting For Game"** line in the app top bar; save
  confirmation via **overlay alert**; zero telemetry in Settings ([Medal: Getting Started](https://support.medal.tv/support/solutions/articles/48000959661-getting-started-with-medal-on-windows)).
- **Principle — Nielsen #1 Visibility of system status:** status is *ambient, continuous
  awareness*, not something you open a config screen to read ([NN/g](https://www.nngroup.com/articles/visibility-system-status/)).
- **Microsoft Win32 UX (decisive):** status bars are *"easy to overlook… many users don't
  notice status bars at all. Don't use status bars for crucial information"*, and *"inexperienced
  users are generally unaware of status bars"*; **"Is the status relevant when users are actively
  using other programs? If so, use a notification-area [tray] icon."** ([Microsoft: Status Bars](https://learn.microsoft.com/en-us/windows/win32/uxguide/ctrl-status-bars)).

→ **Remove the Status section from the Settings window.** The crucial signals already have the
right homes in clipd: the **tray glyph** (armed/recording, U3/U8), the **recording indicator**
(U8), and the **save-complete/-failed toast** (U9). Placement ranking from the research:
**tray + toasts (primary — already built) > a separate Status/Dashboard surface (secondary) >
a bottom status bar (weakest — novices ignore it and it's invisible in-game)**. Detailed
diagnostics (frames/buffer %) belong at most in a **collapsed "Diagnostics"** disclosure for
power users, never the default view.

### F4 — Progressive disclosure + smart defaults: the default screen should be ~4 controls
Both incumbents lead with **presets** and defer every technical knob to a **Custom/Advanced**
reveal; a first-timer's whole interaction is "turn it on + maybe pick a folder," everything
else ships on working defaults ([NN/g Progressive Disclosure](https://www.nngroup.com/articles/progressive-disclosure/), [TechSpot](https://www.techspot.com/guides/3131-nvidia-app/), [Medal Quality Guide](https://support.medal.tv/support/solutions/articles/48001159618-video-quality-settings-guide)).

→ Default clipd Settings shows **Instant-Replay length · Quality preset · Microphone · Save
folder + the save hotkey**. Everything else (resolution, fps, desktop-audio toggle,
clear-after-save, hotkey rebinds beyond save, temp path) goes under **Advanced**, collapsed.

---

## 2. Gamer-aesthetic style cues (concrete, from the research)
- **Four-layer dark system**, not flat gray: deep near-black base · slightly-lighter panel
  layer (cards) · muted secondary text · **one vivid accent used ONLY for the primary action +
  the recording-active state** (an accent "absent elsewhere in the base layers")
  ([ColorArchive: Game UI palette](https://colorarchive.org/guides/game-ui-color-palette/)). NVIDIA = green, Discord/Medal = electric
  purple, Steam = cyan-on-slate — a **single** saturated accent reads "pro"; many brights read
  "toy."
- **Push contrast** past generic apps — ~**7:1 for critical info** (gamers scan UI in
  200–400 ms) ([ColorArchive](https://colorarchive.org/guides/game-ui-color-palette/)).
- **Typography:** a **bold, geometric/angular display face** for titles/wordmark (gaming fonts
  "emphasize bold, geometric, angular characteristics") + a clean neutral sans for body +
  **tabular/monospace numerals** for every live readout (timer, dB, %, file size) so digits
  don't jitter ([Typefactory: gaming fonts](https://typefactory.co/gaming-fonts/)).
- **Card/tile-forward, low density** over dense forms (NVIDIA App's thumbnail panels; Medal's
  Bento widgets) ([NVIDIA App](https://www.nvidia.com/en-us/geforce/news/nvidia-app-download-and-features/)).
- **A large, high-contrast accent "hero" button** for the primary action; secondary/config
  controls rendered quiet (panel fills, muted text).

---

## 3. Mapped redesign for clipd (what changes)
1. **Rip the Status section out of the Settings body** (F3). Rely on the already-built tray
   glyph + recording indicator + save toast. Move the diagnostic numbers into a collapsed
   **"Diagnostics"** expander (off by default) at the bottom, or drop them from the window.
2. **Two-tier layout (F4):** an "Essentials" default (Instant-Replay length · Quality preset ·
   Microphone · Save folder + Browse · the save hotkey) + a collapsed **"Advanced"** section
   (resolution · fps · desktop audio · clear-after-save · record hotkey · [enthusiast: temp
   path]).
3. **Content pass (F1/F2):** "Buffer length" → **"Instant Replay length"** ("Keep the last
   ___"); keep Quality as the preset (no Mbps); label resolution/fps with a one-line helper
   ("Resolution = sharpness · FPS = smoothness"); friendlier microcopy throughout.
4. **VU meters:** left-align the labels, calm the ballistics (F: incumbents don't even show
   meters — ours is a bonus "is my mic live?" signal, so keep it but quiet it), and shift the
   nominal range to the accent, reserving amber="hot"/red="clip".
5. **Visual system (§2):** four-layer dark surfaces, one accent for the hero action + recording
   state, higher contrast, bigger primary button, monospace numerals, more breathing room, fix
   the card right-margin. Typography upgrade pending a font decision (offline constraint below).

---

## 4. Open decisions (need orchestrator direction)
- **Status: remove vs. relocate.** Research says remove from Settings + rely on tray/toasts
  (built). Do we (a) drop it entirely, (b) keep a collapsed Diagnostics expander for power
  users, or (c) build a separate minimal Status/Dashboard surface later? Recommend (b) for the
  beta.
- **Typography / "gamer" font.** A distinctive display face is the single biggest "for-gamers"
  lever, but this environment is **offline — I cannot download a font**. Either (a) refine
  within egui's bundled font now (type scale, weight-via-RichText, monospace numerals, spacing)
  + wire a drop-in font slot, or (b) the orchestrator drops a redistributable OFL `.ttf`
  (e.g. Orbitron/Rajdhani/Chakra Petch) into `assets/` and I embed it.
- **Simplification depth.** Progressive disclosure (collapsed Advanced) is the research-backed
  default; a full Simple/Advanced *mode* toggle is more work for marginal gain. Recommend
  progressive disclosure.

## 5. Unverified / caveats
Exact current NVIDIA App preset Mbps values and default replay length (third-party guides, not
official pages); Medal's exact default bitrate/clip length; Discord's verbatim current quality
labels; precise incumbent hex/typeface specs (brand knowledge, not quoted). None of these
change the direction above.
