# Milestone Tracker (working copy — check items off here, source of truth for gating)

Rule: an item closes only on a measurement from the Nitro V15 (or noted external hardware), never on agent claim. Record the number/date next to each closed item.


**Milestone 0 — spikes (throwaway code)**
- [ ] MF async hardware encoder: synthetic frames → playable .h264/.mp4
- [ ] WGC: capture primary monitor, count fps, verify texture format on SDR + HDR display
- [ ] WASAPI loopback + mic: dump both to WAV, inspect timestamps during silence and device unplug
- [ ] Decision recorded: Sink Writer vs hand-rolled fMP4

**Milestone 1 — dumb recorder (no buffer yet)**
- [ ] Monitor → BGRA→NV12 (video processor) → H.264 CQP → MP4 on disk
- [ ] Correct colors (BT.709 limited) verified vs reference screenshot
- [ ] CFR maintained when screen is static (last-frame resubmit)
- [ ] GPUView session proving encode is on the encoder block; PresentMon before/after numbers
- [ ] Survives: monitor sleep, lock screen, sleep/resume (pipeline rebuild path exists)

**Milestone 2 — audio**
- [ ] Desktop loopback + mic captured, resampled to 48 kHz, AAC-encoded, muxed as two tracks
- [ ] Silence-gap synthesis (loopback goes quiet ≠ desync)
- [ ] Device-change handling: unplug mic mid-record, switch default output mid-record — recording continues, gap is silence, log lines written
- [ ] A/V offset measured with click/flash tool: within ±1 frame @ 60 fps for a 10-minute recording (proves no drift)

**Milestone 3 — the ring buffer (the product)**
- [ ] Compressed-packet ring with duration+byte caps, whole-GOP eviction
- [ ] Global hotkey save: keyframe walk-back, timestamp rebase, atomic write-then-rename, < 1 s
- [ ] Re-entrant/debounced saves; optional buffer clear after save
- [ ] ffprobe assertion script green on 50 consecutive saved clips
- [ ] 24-hour soak test: RAM flat, no handle leaks, clip saved at hour 24 is perfect

**Milestone 4 — window mode + timed recording**
- [ ] Capture focused window (borderless/windowed); monitor fallback for exclusive fullscreen, documented
- [ ] Window resize/close mid-buffer handled (segment cut, no crash)
- [ ] "Record next N minutes" mode sharing the same pipeline with a disk sink
- [ ] Second hotkey set for timed record start/stop

**Milestone 5 — shell & trust**
- [ ] Tray icon with states + minimal menu (Save clip, Pause, Record N min, Open folder, Quit)
- [ ] TOML config, versioned, never silently rewritten, --check-config
- [ ] Rotating file log; every save attempt logged with outcome
- [ ] Watchdog: encoder stall / starvation detection → tray warning
- [ ] Start-with-Windows (registry Run key, off by default)
- [ ] README: honest limitations list (exclusive fullscreen, DRM black frames, HDR tone-mapped, hotkeys in some exclusive-FS games)

**Milestone 6 — hardware matrix before calling it 1.0**
- [ ] NVIDIA (NVENC), AMD (AMF), Intel (QSV) each: 1080p60 + 1440p60, 30-min buffer session, 10 saves, ffprobe-clean
- [ ] Hybrid-graphics laptop
- [ ] Win10 22H2 and Win11
- [ ] Encoder-contention test: Discord screenshare + buffer simultaneously
- [ ] 144 Hz/240 Hz monitor with 60 fps capture (frame pacing correctness)


**External-hardware items (cannot close on the Nitro V15):** AMD/AMF row of Milestone 6 · physical Windows 10 WGC behavior · desktop dGPU-drives-display topology (unless MUX switch present, see 04-TEST-MACHINE.md).

---

# Feature-Complete extension (M7–M10) — full detail in 08-FEATURE-COMPLETE.md

**Milestone 7 — Settings & Status UI (egui satellite)**
- [ ] Status strip (state, fill, target, codec/vendor, last save, counters)
- [ ] Full settings editor writing through versioned TOML
- [ ] Live audio level meters (both streams) — ships first within M7
- [ ] Recent-clips list (open / open folder / copy path)
- [ ] Hotkey press-to-bind with conflict detection
- [ ] Cold-open < 300 ms; 2 h open-window soak with zero engine stalls

**Milestone 8 — Audio power**
- [ ] Process-loopback include/exclude capture (runtime-probed, Win10 2004+)
- [ ] Optional mixed third track (sum, -3 dB headroom, soft clip)
- [ ] Mic mute-in-clips hotkey (track muted, meters live)
- [ ] AV-1..AV-5 re-pass on process-loopback; Discord exclusion verified

**Milestone 9 — Codec & display breadth**
- [ ] AV1 (probe + fallback with toast) — testable on the RTX 4050
- [ ] HDR passthrough HEVC Main10 opt-in; SDR tone-map default; metadata verified
- [ ] Per-vendor CQ defaults finalized from M6 data (spec §6.1 rule executed)
- [ ] 120 fps mode validated on the 144/165 Hz panel

**Milestone 10 — QoL, privacy, release**
- [ ] Multi-length clip hotkeys
- [ ] buffer_when = always | fullscreen-app | manual (tray ring reflects pause)
- [ ] Filename templates ({date}{time}{app}{monitor})
- [ ] Post-save hook (off by default)
- [ ] Save sound toggle
- [ ] Signed binaries, winget, portable zip, installer, Steam depot script
- [ ] User guide + limitations + "why didn't my clip save" pages
