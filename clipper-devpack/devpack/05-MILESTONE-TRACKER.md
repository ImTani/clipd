# Milestone Tracker (working copy — check items off here, source of truth for gating)

Rule: an item closes only on a measurement from the Nitro V15 (or noted external hardware), never on agent claim. Record the number/date next to each closed item.


**Milestone 0 — spikes (throwaway code)**
- [x] MF async hardware encoder: synthetic frames → playable .h264/.mp4
      — 2026-07-03, Nitro V15 / RTX 4050: NVIDIA H.264 Encoder MFT, 120/120
      frames, drain clean; ffprobe h264/Main/1280×720/yuv420p, nb_read_frames=120,
      ffmpeg full decode 0 errors. (.mp4 mux deferred to spike #4.)
      Spike: `spikes/mf_h264_encoder/`.
- [x] WGC: capture primary monitor, count fps, verify texture format on SDR + HDR display
      — done 2026-07-03, Nitro V15 / RTX 4050: WGC IsSupported, item 1920×1080,
      first-frame DXGI_FORMAT=87 (BGRA8) matches SDR expectation, ~28 fps static.
      Note: default D3D device landed on dGPU, WGC delivered cross-adapter (pitfall 14).
      HDR path is code-correct (auto-selects R16G16B16A16F) but UNTESTABLE on this
      hardware — the panel is not HDR-capable. Re-run on an HDR display when available.
      Spike: `spikes/wgc_capture_spike/`.
- [x] WASAPI loopback + mic: dump both to WAV, inspect timestamps during silence and device unplug
      — done 2026-07-03, Nitro V15: loopback (Realtek) + mic (FIFINE) both to
      48k/f32 WAV; per-packet QPC monotonic (~100k ticks/10ms per 480-frame packet,
      §2.2), 0 timestamp_errors, QPC span == captured duration. Mic-unplug run
      caught + fixed a real overflow-panic bug → confirmed: unplug now logs
      AUDCLNT_E_DEVICE_INVALIDATED, ends stream cleanly (device_lost), exit 0, no
      crash (pitfall 3). Silence run: NO gap on this HW (loopback stays continuous,
      unflagged, aligned — modern-Win11 mitigation); probe ready to catch it
      elsewhere. Auto-recover on reconnect is M2 (§7). Spike: `spikes/wasapi_audio_spike/`.
- [x] Decision recorded: Sink Writer vs hand-rolled fMP4
      — 2026-07-03: Sink Writer PROVEN viable (passthrough H.264, no re-encode,
      exact 60fps CFR / 2.000s / avc1 MP4, our timestamps honored). Decision =
      hand-rolled fMP4 per frozen spec §4 (crash-safe moof/mdat + atomic rename +
      rebasing control the Sink Writer's owned pipeline can't give). Sink Writer
      kept as documented fallback. Spike: `spikes/sinkwriter_mux_spike/`; see DECISIONS.md.

**Milestone 1 — dumb recorder (no buffer yet)**
- [ ] Monitor → BGRA→NV12 (video processor) → H.264 CQP → MP4 on disk
- [ ] Correct colors (BT.709 limited) verified vs reference screenshot
- [ ] CFR maintained when screen is static (last-frame resubmit)
- [ ] GPUView session proving encode is on the encoder block; PresentMon before/after numbers
- [ ] Survives: monitor sleep, lock screen, sleep/resume (pipeline rebuild path exists)

**Milestone 2 — audio**  (branch `m2-audio`, 17 commits; all four criteria met — HW-validated on the Nitro V15, 2026-07-04)
- [x] Desktop loopback + mic captured, resampled to 48 kHz, AAC-encoded, muxed as two tracks
      — 2026-07-04, Nitro V15: `record` → ffprobe shows 3 streams (1 h264 1080p60 +
      2 aac-LC 48 kHz stereo 159 kb/s); plays with both desktop + mic audible.
- [x] Silence-gap synthesis (loopback goes quiet ≠ desync)
      — 2026-07-04 (AV-3): two flash bursts around a ~60 s true-silence gap; clicks
      pair before & after with no offset jump, audio track length ≈ video (silence
      filled, not dropped). No `silence gap exceeds fill cap`.
- [x] Device-change handling: unplug mic mid-record, switch default output mid-record — recording continues, gap is silence, log lines written
      — 2026-07-04 (AV-4): FIFINE mic unplug/replug mid-`record` → no crash, clip
      finalizes & plays, mic track has a silence gap over the unplug, audio in sync
      after recovery; `rebuilding stream (§7)` logged. Video + desktop unaffected.
- [x] A/V offset measured with click/flash tool: within ±1 frame @ 60 fps for a 10-minute recording (proves no drift)
      — 2026-07-04 (AV-2): 306 paired events over 10 min via `tools/avrig`; drift
      **−1.92 ms** (minute-1 vs minute-10, §5) ≤ 5 ms — PASS. (AV-1 absolute offset
      is rig-latency-limited, not a valid gate with this rig; AV-5 under 100 % GPU
      load: recorded without crash/desync — sync-under-load precision is rig-fuzzy,
      full load matrix is an M6 item.)

**Milestone 3 — the ring buffer (the product)**  (branch `m3-buffer`; HW-validated on the Nitro V15, 2026-07-04)
- [x] Compressed-packet ring with duration+byte caps, whole-GOP eviction
      — 2026-07-04: `ring.rs` (`§3`/§6.2), unit-tested + exercised live over the
      ~12 h soak (RAM bounded, hourly clear-after-save dips). `Arc<[u8]>` packets so
      a save snapshots by handle-clone (RAM budget).
- [x] Global hotkey save: keyframe walk-back, timestamp rebase, atomic write-then-rename, < 1 s
      — 2026-07-04, Nitro: `Ctrl+Alt+S` → `clip saved` in **64–67 ms**; `just verify`
      confirms video@0 rebase (`§4.3`), exact 60/1 CFR, atomic write. Validated across
      two audio device configs (Realtek+FIFINE, Realtek+NVIDIA Broadcast).
- [x] Re-entrant/debounced saves; optional buffer clear after save
      — 2026-07-04: 250 ms debounce coalesces double-taps; `clear_after_save` empties
      the ring (visible as the hourly RAM dips to the ~30 MB floor in the soak).
- [x] ffprobe assertion script green on 50 consecutive saved clips
      — 2026-07-04, Nitro: **73/73** saved clips pass all 8 `just verify` checks
      (`tools/verify`) — exceeds the 50 bar.
- [~] 24-hour soak test: RAM flat, no handle leaks, clip saved at hour 24 is perfect
      — PARTIAL 2026-07-04: **~12 h** run clean (`ram.csv`): RAM trend **+0.22 MB/h**
      (flat), 30–66 MB band, all 13 accumulated clips perfect. **RECLASSIFIED
      2026-07-05** (DECISIONS): the literal 24 h + Private-Bytes/HandleCount run is now
      a pre-1.0 acceptance item (run against a release-candidate binary alongside the
      M6 matrix), NOT a milestone blocker — the 12 h result is sufficient evidence of
      no leak. M3 treated as effectively met; M4 unblocked.

**Milestone 4 — window mode + timed recording**
- [x] Capture focused window (borderless/windowed); monitor fallback for exclusive fullscreen, documented
      — 2026-07-05, Nitro: `focused-window` captures the window (odd sizes handled via
      the even canvas), across monitors, with the primary-monitor fallback; verified
      clips clean. `LIMITATIONS.md` documents the fallback + letterbox.
- [x] Window resize mid-buffer → FIXED CANVAS (letterboxed rescale, no epoch); a clip
      spans resizes at one resolution (pitfall 11; DECISIONS 2026-07-05 amendment)
      — 2026-07-05, Nitro: resize (grow/shrink/aspect) + cross-monitor drag rescale into
      the fixed canvas; saved clips span the resizes at one resolution, `just verify` green.
- [x] Capture-target handled, no crash: window close → primary-monitor fallback that
      SPANS the clip (Amendment 2), and device-loss epoch restart (buffer retained, §7)
      — 2026-07-05, Nitro: closing the window keeps the buffer alive on the monitor and a
      save retains the pre-close window footage (no cut); device-loss restart HW-verified
      via `--simulate-device-loss` (post-restart clip clean).
- [x] "Record next N minutes" mode sharing the same pipeline with a disk sink
      — 2026-07-05, Nitro: the ring thread tees each MuxItem to the mux worker (D1
      tee-off-ring), which writes a live fMP4; self-verified via `--record-secs 8` → an
      8 s clip PASSES all 8 `just verify` checks (single 1920×1080; §4-clean edges via
      prebuffer-audio-to-first-IDR + audio-drain-at-stop).
- [x] Second hotkey set for timed record start/stop
      — 2026-07-05: `[hotkeys].record_toggle` (default Ctrl+Alt+R) registered alongside
      save_clip; hotkey registration made tolerant (a combo taken by another app warns,
      does not kill buffer mode). NB: Ctrl+Alt+R is taken on the Nitro — recommend a
      different default / user override.

**Milestone 5 — shell & trust**  (branch merges `m5-*`; HW-validated on the Nitro V15, 2026-07-06)
- [x] Tray icon with states + minimal menu (Save clip, Pause, Record N min, Open folder, Quit)
      — 2026-07-06, Nitro: `buffer` runs the tray shell (`ui.rs`); every menu item worked
      (Save → clip; Pause → amber icon + save-while-paused writes retained footage; Start/stop
      recording; Open clips folder; Start with Windows; Quit → clean stop, `bad_qpc=0
      ts_violations=0`). Save + record HOTKEYS still fire with the tray live. A tray-saved
      clip spanning a pause passes all 8 `just verify` checks. Solid-colour state icons behind
      a swappable `icon_for` seam. NB: dropped `common-controls-v6` (v6 comctl32 imports need a
      manifest → the binary failed to load; DECISIONS "M5 T2 fixup") — classic menu styling.
- [x] TOML config, versioned, never silently rewritten, --check-config
      — logic/CI (no HW step): `config.rs` is versioned + validated; M5 writes nothing to
      `config.toml` (start-with-Windows uses the registry), so "never rewritten" holds by
      construction. `--check-config` is now exercised by `tests/smoke.rs` on the built binary.
      Unknown-key preservation-on-rewrite stays deferred to the M7 settings pen.
- [x] Rotating file log; every save attempt logged with outcome
      — 2026-07-06, Nitro: `%LOCALAPPDATA%\clipd\logs\clipd.log.<date>` (daily-rolled,
      non-blocking) written with one outcome line per save (`clip saved` + path + ms), plus
      pause/resume, record start/finalize, autostart toggle, and `shutdown` lines. Save-outcome
      logging already covered slow-write WARN + FAILED + skipped branches.
- [~] Watchdog: encoder stall / starvation detection → tray warning
      — PARTIAL 2026-07-06: the engine→tray state pipeline (`ShellSignal` → icon/tooltip) is
      HW-proven by the Pause amber-flip; the `§6.3` divergence → WARNING decision logic + the
      `is_diverged` threshold are unit-tested. The LIVE Warning trigger needs genuine
      encoder/mux starvation (GPU pegged), which is not cleanly inducible on demand — folded
      into the **M6 load matrix** (Discord-screenshare / 100 %-GPU rows). Dead-worker → Error
      is wired (`any_worker_finished`). Not an M5 blocker.
- [x] Start-with-Windows (registry Run key, off by default)
      — 2026-07-06, Nitro: the checkable tray item toggled the `HKCU\…\Run` `clipd` value on
      (`RegSetValueExW` → Ok) and off (`RegDeleteValueW` → Ok); `reg query` confirms the value
      is absent after disabling. Off by default (absent = off). The one permitted registry write.
- [x] README: honest limitations list (exclusive fullscreen, DRM black frames, HDR tone-mapped, hotkeys in some exclusive-FS games)
      — 2026-07-06: grew `LIMITATIONS.md` (pause-keeps-capturing, hotkey-swallow, letterbox,
      why-didn't-my-clip-save log pointer) + un-staled the README status/limitations (doc-only).

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
