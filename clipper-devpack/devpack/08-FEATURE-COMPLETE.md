# Feature-Complete Definition (v1.0) — What "Done" Means

The MVP (Milestones 0–6) proves the engine. Feature-Complete is the version
that gets a public release page, a Steam-candidate build, and the claim
"a complete ShadowPlay replacement for the things ShadowPlay should have been."
Everything here is scoped now so that no mid-development "wouldn't it be nice"
decisions occur. Milestones M7–M10 extend the tracker.

## M7 — Settings & Status UI (the egui satellite)
Framework: egui/eframe. Architectural law: the window is a SATELLITE — lazily
created from the tray, communicates with the engine exclusively over the same
channels as every other component, and the engine must run forever without it
ever opening. The UI process-lifetime-wise cannot be load-bearing.

- [ ] Status strip: buffering/paused state, buffer fill (seconds held vs
      configured), active capture target, resolution/fps/codec/encoder vendor
      in use, last save result + timestamp, current epoch's dropped-frame and
      watchdog counters.
- [ ] Settings editor covering the full config schema; writes go through the
      same versioned TOML (file remains source of truth and hand-editable; UI
      is a friendly pen). Invalid edits show the same errors as --check-config.
- [ ] Live audio level meters for both streams (two VU bars). This feature
      preempts the entire "was my mic even recording?" incumbent grief class
      and is the single highest-value UI element — it ships before anything
      cosmetic.
- [ ] Recent clips: a plain list of the last 20 saves — open file, open
      folder, copy path. NOT an editor, NOT thumbnails-with-scrubbing.
- [ ] Hotkey capture widget (press-to-bind) with conflict detection.
- [ ] Visual language: dark, dense, quiet; egui default dark + one accent.
      Acceptance: window cold-open < 300 ms; zero engine-thread stalls
      attributable to UI (verified: soak test with window open 2 h).

## M8 — Audio power features
- [ ] Per-application audio capture/exclusion via process-loopback
      (ActivateAudioInterfaceAsync, Win10 2004+): include-list ("game only")
      and exclude-list ("everything except Discord") modes. THE killer
      feature no incumbent does well; API availability probed at runtime,
      feature hidden on unsupported builds.
- [ ] Optional third mixed track (desktop+mic pre-mixed) for
      share-without-editing workflows; default off. Mix is a simple sum with
      -3 dB headroom and a soft clipper; no AGC, no filters (DSP rabbit holes
      are out of scope permanently).
- [ ] Mic mute-in-clips toggle hotkey (mutes the TRACK content, meters still
      live so the user can see it's muted — deliberate, logged).
- [ ] Acceptance: AV-1..AV-5 pass with process-loopback streams; exclusion
      verified against Discord + a game simultaneously.

## M9 — Codec & display breadth
- [ ] AV1 encode where hardware supports it (the RTX 4050 test machine does).
      Config `codec = "h264" | "hevc" | "av1"`, capability-probed, falls back
      with a log+toast, never a silent switch.
- [ ] HDR passthrough: HEVC Main10 with correct color metadata
      (BT.2020/PQ passthrough) as opt-in; SDR tone-map remains default.
      Acceptance: HDR clip verified on an HDR display + MediaInfo metadata
      check; SDR players show reasonable (not neon) colors for tone-mapped
      output.
- [ ] Per-vendor tuned CQ defaults finalized from Milestone-6 measurements
      (spec §6.1 adjustment rule executed and the table updated in-repo).
- [ ] 120 fps capture mode unlocked (spec §1.2 tunable) after grid-pacing
      validation at 120 on the 144/165 Hz panel.

## M10 — QoL, privacy, release engineering
- [ ] Multiple clip lengths on separate hotkeys (e.g. F8=30 s, F9=2 min,
      F10=5 min) — same buffer, different walk-back.
- [ ] Auto-pause policies: `buffer_when = "always" | "fullscreen-app" |
      "manual"`; default "always", "fullscreen-app" documented as the privacy
      mode. Tray ring reflects paused state unmistakably.
- [ ] Filename templates: {date} {time} {app} {monitor} tokens; {app} from
      the foreground window's process name at save time.
- [ ] Post-save hook: single user-configured command receiving the clip path,
      default off, output logged. (Power users get automation; we don't build
      it for them.)
- [ ] Save sound (one toggle, one bundled wav, replaceable by path).
- [ ] Release engineering: code-signed binaries, winget manifest, portable
      zip + optional installer (silent-capable), Steam depot build scripted.
      Reproducible-build notes in repo. AV-vendor false-positive submissions
      process documented.
- [ ] Docs: user guide (one page), limitations page (honest), and a
      "why your clip didn't save" troubleshooting page mapping every tray
      warning to its log line.

## Considered and REJECTED for feature-complete (do not reopen without a
written orchestrator decision)
- Any clip trimming/editing — even keyframe-aligned lossless trim. It is the
  top of the slippery slope to being Medal; every OS ships a basic trimmer.
- Webcam, overlays, streaming, cloud, accounts, telemetry, auto-highlights,
  game-detection database, Linux/macOS ports (Linux is GPU Screen Recorder's
  ground; revisit only post-1.0 as a separate decision).
- Software (x264) fallback encoding: machines without any hardware encoder
  are not the audience; a clear "no hardware encoder found" error is correct.

## The 1.0 bar, in one sentence
A user with any 2018+ GPU installs one small signed binary, sees their mic
levels move, plays for a month, presses one key after every great moment, and
never once thinks about the software — and if anything ever does go wrong, the
tray and the log tell them exactly what and why.
