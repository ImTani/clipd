# Project Plan: Lightweight Replay-Buffer Clipper (Rust, Windows)

Working name: pick something short — the binary will live in the tray, not on a website.

---

## 1. Product definition

**What it is:** A single native background process that continuously captures the screen (or focused fullscreen/borderless app) into an in-memory ring buffer of *compressed* video+audio packets. One hotkey saves the last N seconds to an MP4. A second mode records the next N minutes straight to disk. Nothing else.

**Non-goals (write these into the README on day one, they are load-bearing):**
- No overlay, no injection into game processes (anti-cheat safety + zero risk of crashing games)
- No editor, no uploader, no accounts, no telemetry, no auto-update phoning home
- No streaming
- No webcam
- No game detection / auto-clip AI
- No cross-platform in v1 (Windows 10 1903+ / Windows 11 only; WGC requires 1903 for window capture without borders and modern features)

**Hard budgets (treat as CI-enforceable requirements, not aspirations):**
- Idle-but-buffering CPU: < 2% on a mid-range CPU
- GPU 3D engine usage attributable to us: ~0% (encode must live on the dedicated encoder block)
- RAM: ring buffer size + < 75 MB overhead
- Binary: < 10 MB, zero runtime dependencies (no FFmpeg DLLs, no VC++ redist surprises, no WebView)
- Save-clip latency: file exists and is playable < 1 s after hotkey

---

## 2. Overall architecture

One process, four long-lived threads plus the main/tray thread. All communication over bounded channels (crossbeam or std mpsc). No async runtime — this is a small fixed set of threads with real-time-ish duties; tokio buys nothing and costs binary size and unpredictable scheduling.

```
main thread ──── tray icon, hotkey events, config, watchdog UI
     │
     ├── capture thread ──── WGC frame pool callbacks → GPU color convert (BGRA→NV12) → encoder input
     │
     ├── encode thread ───── Media Foundation H.264/HEVC transform (NVENC/AMF/QSV underneath)
     │                        emits compressed samples w/ timestamps + keyframe flags
     │
     ├── audio thread ────── WASAPI loopback (desktop) + WASAPI capture (mic)
     │                        resample → 48kHz float → AAC encode → compressed samples
     │
     └── buffer/mux thread ─ ring buffer of packets; on save: slice from keyframe, mux MP4, fsync
```

### Data-flow rules (these ARE the architecture)
1. **Video pixels never touch system RAM.** WGC hands you a D3D11 texture; you convert BGRA→NV12 with the GPU video processor (ID3D11VideoProcessor) or a trivial compute shader; the NV12 texture goes into the MF encoder as a D3D-backed sample. Any design where a frame gets `memcpy`'d has already failed the resource budget.
2. **The ring buffer stores compressed packets only.** ~50 Mbit/s H.264 at 1440p60 → a 5-minute buffer ≈ 1.9 GB; a 2-minute buffer ≈ 750 MB; HEVC/AV1 roughly halves that. Cap by BOTH duration and bytes.
3. **One clock to rule them all.** QueryPerformanceCounter is the master clock. Video frames are stamped with the QPC time WGC delivered them; audio packets are stamped by converting the WASAPI device position + stream latency to QPC. Sync is a property of correct timestamps, not of "starting both streams at the same time."
4. **Everything is crash-isolated at the save path.** The one unforgivable failure is "user pressed the button and got nothing." The mux path gets its own error handling, its own logging, and writes fragmented MP4 so even a mid-write crash yields a playable file.

### Component decisions and rationale

| Concern | Decision | Why / alternative rejected |
|---|---|---|
| Display+window capture | Windows Graphics Capture (WGC) via `windows` crate | Cross-GPU (hybrid laptops), native window capture, no game hooking. DXGI Desktop Duplication kept as fallback for display capture on Win10 versions where WGC lacks border removal. Rejected: game-hook injection (anti-cheat, fragility). |
| Video encode | Media Foundation hardware MFT (async, D3D11-aware) | One API → NVENC, AMF, QuickSync. No vendored SDKs, no FFmpeg DLLs. Rejected: direct vendor SDKs for v1 (3× integration work; revisit only if MF rate-control is limiting). Rejected: linking FFmpeg (binary size, licensing, DLL hell — the exact "big dependency" disease). |
| Rate control | CQP/quality mode, keyframe interval 1–2 s | Constant quality is what a buffer wants; CBR wastes bits on static scenes and starves action scenes. Short GOP = tight clip start points and small "walk-back-to-keyframe" slack. |
| Audio capture | Raw WASAPI via `wasapi` crate (NOT cpal) | Need loopback + event-driven mode + IMMNotificationClient device-change callbacks. cpal's loopback has been added/removed/re-added historically and hides the device APIs we must control. |
| Audio encode | MF AAC encoder MFT, 48 kHz, two tracks | Mic and desktop as SEPARATE tracks in the container. Cheap to do, top user request, makes "my voice was clipping" recoverable in post. |
| Resampling | `rubato` crate (or MF resampler DMO) | Devices ship at 44.1/48/96/192 kHz; normalize everything to 48 kHz internally so mismatches are structurally impossible. |
| Ring buffer | VecDeque<Packet> per stream, dual caps (secs+bytes), keyframe index | Boring on purpose. Save = binary search keyframe ≤ (now − clip_len), slice, mux. |
| Muxing | MF Sink Writer OR small hand-rolled fMP4 writer | fMP4 preferred: crash-safe, trivial to append moof/mdat pairs from packet slices. Evaluate Sink Writer first; if it forces re-timestamping pain, hand-roll (fMP4 is a well-documented, small format). |
| Hotkeys | `global-hotkey` crate (RegisterHotKey) | No keyboard hooks (AV heuristics flag low-level hooks; RegisterHotKey is the polite API). Known limit: some exclusive-fullscreen games swallow RegisterHotKey → document borderless as recommended; add Raw Input fallback later if demanded. |
| Tray/UI | `tray-icon` + native menu; config = TOML in %APPDATA% | No UI framework at all in v1. A text config that never gets silently rewritten is a FEATURE relative to the incumbents. |
| Logging | `tracing` → rotating file in %LOCALAPPDATA% | Every save attempt, every device change, every encoder stall gets a line. When a user says "it didn't save," the log must answer why. |

---

## 3. Pitfalls catalogue — the bugs you WILL hit

Ordered roughly by how much of your life each will consume.

### A/V sync and audio (60% of the pain lives here)

1. **Audio clock ≠ video clock ≠ wall clock.** Audio devices run on their own crystal; over a 5-minute buffer a 100 ppm drift is 30 ms — audible lip-sync error. Fix: timestamp audio from IAudioCaptureClient's QPC position field each packet (it hands you a QPC-correlated position), never by counting samples and assuming the nominal rate.
2. **WASAPI loopback silence gaps.** Loopback capture delivers NOTHING when no audio is playing on the endpoint (and buffer flags may signal silence). If you naively append packets, audio duration < video duration and everything after a quiet moment desyncs. Fix: detect timestamp gaps and synthesize encoded silence (or feed zeroed PCM) to keep the AAC track continuous. This is a classic "clips are fine until the game goes quiet" bug.
3. **Default-device changes mid-session.** User plugs in a headset, Windows switches default endpoint, your stream keeps recording the old (now dead or wrong) device. This is precisely the ShadowPlay "recorded everyone but me" failure. Fix: register IMMNotificationClient; on default change or device removal, tear down + rebuild the stream inside the same session, stamping silence over the gap. Test this by yanking a USB mic mid-buffer — it must not crash, desync, or silently record nothing forever.
4. **Virtual audio devices** (Voicemod, VB-Cable, NVIDIA Broadcast, Discord's device). They report odd formats (e.g., 2-channel 44.1 float when the physical device is 48k), disappear when their host app closes, and are involved in a disproportionate share of incumbent bug reports. Fix: format-negotiate defensively, resample always, and treat AUDCLNT_E_DEVICE_INVALIDATED as a rebuild trigger, not an error.
5. **Sample-rate mismatch distortion/drift** — the thing OBS only warns about in logs. You resample everything to 48 kHz internally; a mismatch becomes impossible rather than a support ticket.
6. **Exclusive-mode apps** stealing the endpoint. Rare but real; handle AUDCLNT_E_DEVICE_IN_USE by retrying in shared mode and logging.
7. **AAC encoder priming/delay.** AAC adds ~1024 samples of encoder delay; if you ignore it, audio leads video by ~21 ms in every clip. Write the edit-list/priming metadata or trim.

### Capture

8. **Fullscreen exclusive games + WGC.** True exclusive fullscreen can't be window-captured (and alt-tab destroys the surface). Modern reality: most "fullscreen" is borderless, and Win10+ DXGI often gives you eFSE (fullscreen optimizations) which WGC handles. Plan: window capture for borderless/windowed; for stubborn exclusive-FS titles, fall back to capturing the monitor. Say so in docs instead of pretending.
9. **The yellow capture border.** WGC draws a border around captured content; `IsBorderRequired = false` needs Windows 10 2104+/Win11 and a capability check. On older builds users WILL report "yellow line around my game" as a bug — detect and document.
10. **Cursor.** Decide once: WGC's `IsCursorCaptureEnabled` (composited for you) — expose as config, default on for desktop, off for game window capture.
11. **Resolution/display-mode changes mid-buffer** (game changes res, DPI change, monitor sleep, HDR toggle). The frame pool must be recreated with the new size, and the encoder either restarts (flush old segment into buffer, start a new encoded segment — clips spanning the change get cut at the boundary) or you scale to a fixed canvas. V1: fixed output resolution chosen at buffer start; recreate pipeline on change; a clip cannot span the boundary. Document it.
12. **HDR.** WGC hands you FP16/BGRA10 on HDR displays; naïve BGRA8 assumption = washed-out or garbage clips. V1: detect HDR, tone-map to SDR via the GPU video processor (what ShadowPlay does by default), and log it. HDR-passthrough HEVC is a v2 feature.
13. **Occlusion/minimize** in window mode: frames stop arriving. Your encoder must handle "no new frame for 500 ms" by re-submitting the last frame at the frame interval (or encoding at variable rate), otherwise the muxed clip has VFR weirdness that breaks some players/editors. Decide CFR (resubmit last frame) — editors hate VFR.
14. **Secondary GPU / hybrid laptops.** Capture happens on the iGPU driving the display while the game renders on the dGPU. WGC handles the cross-adapter copy, but your encoder device selection matters: encode on the adapter that has the texture, or you pay a PCIe round trip. Enumerate adapters and co-locate encoder with capture device.

### Encoding

15. **Encoder session limits.** Consumer NVIDIA driver caps concurrent NVENC sessions (the cap has moved: 3 → 5 → 8 across driver versions). If the user runs Discord streaming + your buffer, sessions are consumed. Handle "encoder open failed" with a visible tray warning + one retry path, never a silent dead buffer.
16. **Encoder starvation under GPU load** — the OBS "diashow" failure: game eats 100% of the 3D engine, and while NVENC silicon is separate, your color-convert pass and frame copies still queue behind the game. Mitigations: (a) do the BGRA→NV12 on the video processor engine, not a compute shader on the 3D queue; (b) set your process GPU scheduling priority via D3DKMTSetProcessSchedulingPriorityClass; (c) watchdog: if encode input queue depth grows or frames-in vs frames-out diverges for > 2 s, drop frames (keep timestamps honest) and flash the tray icon. Detect and surface, never silently produce a slideshow.
17. **MF async MFT state machine.** The hardware encoder MFT is asynchronous: METransformNeedInput/METransformHaveOutput events, format negotiation dance, D3D device manager plumbing. It is finicky and under-documented; budget real time here. Deadlocks come from feeding input without draining output. This is the "two weeks of pain" component — schedule it early (milestone 1), not late.
18. **Vendor rate-control quirks.** AMF trails NVENC in compression efficiency at the same nominal quality; QSV differs again. Ship per-vendor default CQ values (e.g., NVENC 23, AMF 21, QSV 22 — tune empirically) rather than one global default.
19. **Keyframe on demand.** When timed-recording mode starts, force an IDR immediately; when a clip save lands mid-GOP you accept up to one GOP of pre-roll slack. Verify your MFT honors CODECAPI_AVEncVideoForceKeyFrame.

### Ring buffer & muxing

20. **Buffer memory accounting.** Bitrate spikes (confetti, smoke, foliage) can 3× your average momentarily. Cap by bytes as well as seconds and evict oldest GOPs whole (never split a GOP or you orphan P-frames → corrupt clip starts).
21. **Timestamp rebasing at mux time.** Packets in the ring carry absolute QPC-derived PTS; MP4 wants zero-based. Rebase both tracks against the SAME origin (the video keyframe you cut at), not each track's own first packet — the classic subtle desync bug.
22. **The save path must be re-entrant.** User double-taps the hotkey; a save is in flight. Queue it or debounce it, but never corrupt the in-progress file and never drop the second request silently.
23. **Buffer-clear-on-save policy.** Incumbents annoy users with overlapping consecutive clips. Make it a config option (`clear_after_save = true|false`), default true.
24. **Disk full / slow disk / OneDrive-synced Videos folder.** Write to a temp name in the target dir, fsync, rename — atomic-ish and prevents half-files being indexed. If the write blocks (cloud-sync stub dirs are famous for this), it must not stall the buffer thread — the mux runs on its own thread by design.

### System integration

25. **Sleep/resume and lock screen.** DXGI/WGC sessions and the D3D device can be lost across sleep (DXGI_ERROR_DEVICE_REMOVED). Auto-rebuild the whole pipeline on resume. OBS users literally install a plugin because the buffer prevents/breaks sleep — get this right natively.
26. **Driver crashes / TDR.** GPU driver resets invalidate everything. Same rebuild path as sleep. The rebuild path is therefore not an edge case — it's a core, well-tested subsystem (device removal, sleep, res change, driver update all funnel into it).
27. **DRM-protected content** (Netflix in browser w/ hardware DRM, some launchers). Capture yields black frames by design. Not a bug; document it, and if you get patch requests to bypass protected-content flags, that's out of scope (and legally radioactive — an existing GitHub "patcher" project does this to ShadowPlay; don't be that).
28. **Antivirus false positives.** A background process with global hotkeys that records the screen pattern-matches to a screen-grabbing RAT. Mitigations: RegisterHotKey not hooks, signed releases eventually, reproducible builds, no packers/UPX, submit to Defender if flagged.
29. **Privacy defaults.** You are a "records everything" daemon. Defaults: buffer only while a fullscreen/borderless app is focused is worth considering (config), a clear tray indicator when buffering, pause-buffer hotkey. This is a differentiator, not a compliance chore.
30. **Config integrity.** Versioned schema (`config_version = 1`), unknown keys preserved on rewrite, file only rewritten on explicit user change, and a `--check-config` flag that validates and prints the effective config. Silent resets are the #1 incumbent trust-killer.
31. **Multiple monitors, mixed refresh rates, portrait displays.** Choose capture target explicitly in config (`monitor = "primary" | index | "focused-window"`); never guess.

---

## 4. Development environment

**Machine/OS:**
- Windows 11 primary dev box; a Windows 10 21H2/22H2 VM or spare machine for down-level WGC behavior (border removal, API availability).
- Ideally access to all three GPU vendors before 1.0. Realistic path: develop on whatever you own; validate AMF and QSV via friends/second-hand cheap cards (a used low-end Intel Arc or an AMD APU covers QSV/AMF cheaply). MF hides most of it, but rate control and format negotiation differ per vendor and only real hardware reveals it.

**Toolchain:**
- Rust stable, MSVC toolchain (`x86_64-pc-windows-msvc`) — you're binding COM/D3D; the GNU toolchain buys nothing.
- Crates: `windows` (WGC, D3D11, MF, DXGI, COM), `wasapi`, `rubato`, `global-hotkey`, `tray-icon`, `serde`+`toml`, `tracing`+`tracing-appender`, `crossbeam-channel`. Nothing else in the core.
- `cargo-deny` (license/dep audit — keeps the dependency tree honest), `clippy` + `rustfmt` in CI from day 1.

**Debugging/analysis kit (install all of these before writing code):**
- **MFTrace / mftrace viewer** (Windows SDK) — the only way to see inside Media Foundation's async event soup. Non-negotiable for milestone 1.
- **GPUView + Windows Performance Recorder** — proves where your GPU time goes; how you verify the "0% 3D engine" budget and diagnose encoder starvation.
- **PresentMon** — measure the game's frametime impact with buffer on/off; this number goes in your README.
- **RenderDoc** — debugging the BGRA→NV12 conversion when colors come out wrong (they will: BT.601 vs BT.709 matrix + limited-vs-full range is a guaranteed first-week bug; decide BT.709 limited-range and test against reference).
- **ffprobe + MediaInfo** — validate muxer output: track durations equal? edit lists sane? PTS monotonic? Write a script that asserts these on every test clip.
- **Process Explorer / Task Manager GPU engine columns** — quick encoder-block vs 3D-engine sanity checks.
- A **test-signal toolkit**: a small app or web page that plays a metronome click synced to a visual flash → frame-step saved clips to measure A/V offset objectively (±1 frame @ 60 fps target). Build this in week 1; you will use it hundreds of times.

**CI:** GitHub Actions windows-latest: build, clippy, unit tests (ring buffer, timestamp math, config parsing are all pure-logic testable), plus an artifact build per commit. Real capture/encode can't run on CI runners (no GPU encoder) — those are manual test-matrix items.

---

## 5. Things to settle BEFORE writing code

1. **Spike the MF async encoder in isolation** (a throwaway repo: synthetic NV12 frames → H.264 file). If this takes two days, great; if two weeks, you've learned it early and can still pivot to vendor SDKs or a minimal FFmpeg static link without sunk cost. Highest-risk component, so it goes first.
2. **Decide the fMP4 question** with a one-day spike of the MF Sink Writer: can you feed it pre-encoded samples with your timestamps without it re-encoding or fighting you? If yes, use it for v1. If it fights, commit to hand-rolled fMP4.
3. **Write the timestamp spec** as an actual document before implementing: units (100 ns MF ticks), clock origin, how audio device position maps to QPC, rebasing rule at mux. Most sync bugs are spec bugs.
4. **Choose license** (GPL-3.0 or MIT/Apache-2.0 dual). Note: staying off FFmpeg/x264 sidesteps the LGPL/GPL binary-distribution questions entirely — another argument for the MF-only stack. H.264/HEVC patent licensing for the *encoder* is the GPU vendor's/OS's problem since you use their encoders; don't ship a software x264 fallback in v1.
5. **Name the failure UX now:** tray icon states (buffering / paused / warning / error), a single sounds-on-save toggle, and the rule "any dropped save writes a log line AND flashes the tray." Decide it now so every component knows where errors go.
6. **Pick the config schema v1** and freeze it: capture target, resolution/fps, codec, quality, buffer length, clip length(s) (support multiple hotkeys → 30 s / 2 min / 5 min), audio devices (default-follow vs pinned), separate-tracks toggle, output dir, filename template, clear-after-save.

## 6. MVP checklist

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

## 7. Post-MVP candidates (explicitly deferred, resist until 1.0 ships)
Multiple simultaneous clip lengths on separate hotkeys · AV1 on capable GPUs · HDR passthrough (HEVC main10) · per-app audio capture via ActivateAudioInterfaceAsync process loopback (Win10 2004+; lets you exclude Discord from clips — a genuinely great feature, but v1.1) · replay-buffer auto-pause when no fullscreen app focused · Linux port (pipewire + VAAPI — effectively a second project; GPU Screen Recorder already owns that ground).
