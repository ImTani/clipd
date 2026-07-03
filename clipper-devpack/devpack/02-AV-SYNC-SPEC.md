# A/V Timestamp & Synchronization Specification — v1.0 (FROZEN)

Status: normative. Every rule below is a MUST unless marked "tunable." Tunables ship
with the stated default and a stated adjustment rule, so the pipeline can be built
in one pass without iterative re-specification. Where a value depends on hardware
that cannot be known in advance, this spec dictates the measurement procedure, the
expected range, and the fallback constant.

---

## 0. Definitions and units

- **Tick**: 100 nanoseconds. ALL timestamps in the pipeline are `i64` ticks
  (identical to Windows MFTIME / REFERENCE_TIME). 1 ms = 10,000 ticks.
  1 s = 10,000,000 ticks.
- **QPC**: QueryPerformanceCounter. On any Windows 10+ machine this is
  invariant-TSC backed, monotonic, and typically runs at 10 MHz (1 QPC unit =
  100 ns = 1 tick exactly). DO NOT assume the 10 MHz frequency: at process start,
  read QueryPerformanceFrequency once and build the conversion
  `ticks = qpc * 10_000_000 / qpf` using 128-bit intermediate math
  (`i128` mul-then-div) to avoid overflow. Cache qpf; it never changes.
- **PTS**: presentation timestamp of a packet, in ticks, in the master domain.
- **Epoch**: a contiguous pipeline configuration period. Any of: resolution change,
  HDR toggle, encoder rebuild, device-loss rebuild, capture-target change starts a
  new epoch (`epoch_id += 1`). Packets carry `epoch_id`. A saved clip MUST NOT
  span epochs.
- **Master domain**: raw QPC converted to ticks. There is exactly one clock domain
  in the entire program. No component may keep time by counting samples or frames.

Monotonicity guard: any producer emitting a packet with `pts <= previous_pts` of
the same stream MUST bump it to `previous_pts + 1` tick and increment a
`ts_violation` counter (logged every 60 s if nonzero). This is a diagnostic
canary, not a fix — the counter staying at 0 is the expected steady state.

---

## 1. Video timestamping

### 1.1 Source of truth
WGC delivers each frame with `SystemRelativeTime` — already QPC-based ticks.
Use it verbatim. NEVER stamp frames with "time of callback arrival": callback
dispatch jitter is 0.5–4 ms under load and would inject that jitter straight
into pacing decisions.

### 1.2 CFR grid (the pacing algorithm)
Output is constant frame rate. Default `fps = 60` (tunable: 30/60/120;
120 only exposed after Milestone 6 validation).

- Frame duration `D = 10_000_000 / fps` ticks, kept as an exact rational
  (numerator 10_000_000, denominator fps) — never a rounded float.
  At 60 fps, D = 166,666.67 ticks; slot N boundary = `base + N*10_000_000/60`
  computed as integer `base + (N*10_000_000)/60` each time (no accumulation of
  a rounded D — accumulation of 166,667 drifts +20 ms/hour).
- `base` = SystemRelativeTime of the first captured frame of the epoch.
- Each arriving frame maps to slot `N = (t - base + D/2) / D` (round to nearest).
- **Duplicate policy**: two arrivals in one slot → keep the later one (fresher).
- **Gap policy**: at slot deadline + grace, if the slot is empty, resubmit the
  previous frame's texture with the new slot's PTS.
  Grace = 0.5 × D (8.3 ms @ 60 fps), tunable range 0.25–0.75 D.
  Rationale: WGC delivers on vsync of the source; a 59.94 Hz source feeding a
  60.00 grid produces one legitimate gap every ~16.7 s — grace absorbs delivery
  jitter without adding a full frame of latency.
- **Static screen**: WGC stops delivering when nothing changes. The resubmit
  rule above covers this automatically; the encoder therefore always receives
  exactly `fps` frames per second per epoch. This is what makes the output
  strictly CFR and editor-friendly.
- Resubmission requires holding the last frame: keep exactly 2 textures
  (last-delivered, in-flight). GPU memory cost at 1440p BGRA: 14.7 MB per
  texture — negligible.

### 1.3 Encode PTS
The PTS fed to the encoder for slot N is the slot boundary time, NOT the
frame's arrival time. Arrival time chooses the slot; the grid defines the PTS.
Consequence: video PTS sequence is perfectly regular; ALL sync error is pushed
into the audio path where it is measurable and correctable.

### 1.4 High-refresh sources
A 240 Hz game on a 60 fps grid: 4 arrivals per slot, keep-latest wins, 3 are
dropped before color conversion (drop = release the WGC frame; do not convert).
Conversion work is therefore bounded by output fps, not source fps. This is the
rule that keeps the GPU budget flat regardless of game frame rate.

---

## 2. Audio timestamping

### 2.1 Capture configuration (both streams: loopback + mic)
- WASAPI shared mode, event-driven.
- Internal canonical format: 48,000 Hz, f32, stereo (mic mono→stereo by
  channel duplication at capture, before any DSP).
- Period: request the device default (10 ms on virtually all hardware
  = 480 frames @ 48 kHz). Do NOT request smaller periods — this is capture,
  not a synth; 10 ms periods cost ~0.1% CPU per stream and are universally
  stable, including on virtual devices.
- Buffer size: 4 × period (40 ms). Overrun headroom for scheduling hiccups;
  costs 40 ms of RAM (30 KB), zero latency (we timestamp, we don't monitor).

### 2.2 Source of truth
`IAudioCaptureClient::GetBuffer` returns `QPCPosition` (u64, in QPC units —
convert with the same qpf math) for each packet: the QPC time of the FIRST
sample in the packet. This is the packet's PTS. Full stop.

- Never derive audio PTS by `first_pts + samples_seen/48000`: that reintroduces
  the device-clock-vs-QPC drift this spec exists to kill.
- If a driver reports the `AUDCLNT_BUFFERFLAGS_TIMESTAMP_ERROR` flag or a
  QPCPosition of 0 (rare, buggy drivers): fall back for THAT PACKET ONLY to
  `prev_pts + prev_frames * 10_000_000 / 48_000`, increment `bad_qpc` counter.
  If `bad_qpc` exceeds 100 packets/minute, the device is declared
  timestamp-unreliable and the whole stream switches permanently (this session)
  to sample counting anchored at the last good QPC value, with drift correction
  (§2.4) doing the honest work. Expected: virtual devices (VB-Cable, Voicemod)
  land here; physical endpoints don't.

### 2.3 Loopback silence gaps
Loopback delivers nothing while the endpoint is silent. Gap detection, per
packet: `gap = pts - (prev_pts + prev_frames * 10_000_000 / 48_000)`.

- `gap <= +/- 20_000 ticks` (2 ms): normal jitter, ignore.
- `gap > 20_000 ticks`: synthesize `round(gap * 48_000 / 10_000_000)` frames of
  digital silence, stamped to fill the hole exactly, then admit the real packet.
  Synthesized silence goes through the same AAC encoder (it compresses to
  ~nothing).
- `gap < -20_000 ticks` (overlap — device replayed time): drop the overlapped
  leading samples of the new packet; log.
Threshold rationale: one 10 ms period of true silence is the minimum observable
gap; 2 ms discriminates jitter from silence with 5× margin.

Mic streams do not gap when silent (they deliver zeros), but the same logic
stays enabled — USB mics stall for 1–2 periods during hub power events and the
filler keeps the track continuous.

### 2.4 Drift measurement and correction
Even with QPC stamping, the SAMPLES arrive at the device's crystal rate. Over a
long buffer the sample count and the QPC span disagree; consumer audio clocks
are off by 20–200 ppm (100 ppm = 6 ms/min = 30 ms over a 5-min buffer —
comfortably audible as lip-sync error if uncorrected).

Correction is by micro-resampling (rubato, sinc, fixed-in/variable-ratio):

- Every `W = 10 s`, compute `err_ppm = (samples_received / 48_000 -
  qpc_span_seconds) / qpc_span_seconds * 1e6` over a sliding 30 s window.
- Controller: proportional only. `ratio_adjust = -err_ppm`, clamped to
  ±300 ppm total, slew-limited to 10 ppm per second of adjustment change.
  (P-only is sufficient: the plant is a constant offset; integral action just
  adds overshoot risk. 10 ppm/s slew = a full 300 ppm swing takes 30 s and is
  inaudible; instantaneous ratio steps > ~50 ppm are audible as pitch flicker.)
- Expected steady state: correction converges within 60 s of stream start and
  the residual A/V error contribution from audio is < 2 ms indefinitely.
- If |err_ppm| > 1000 (0.1% — device is resampling badly or lying), stop
  chasing: clamp at 300 ppm, set tray warning state, log. A device this bad is
  a user-visible hardware problem, not something to silently paper over.

### 2.5 Mixing and track layout
- Two AAC tracks in the container: track 1 = desktop, track 2 = mic. No mixed
  track in v1 (players default to track 1; editors take both).
- The two capture streams are NOT mixed, so they need no mutual alignment
  beyond both being QPC-stamped. Their relative sync falls out of the design.

### 2.6 AAC framing and encoder delay
- AAC-LC, 48 kHz, stereo, 160 kbps CBR per track (tunable 96–256).
  Packet = 1024 samples = 213,333.3 ticks = 21.33 ms.
- PTS of an AAC output packet = PTS of its first INPUT sample minus nothing —
  i.e., we do the delay compensation ourselves:
- **Encoder delay**: AAC encoders prepend priming samples. Expected value for
  the MF AAC encoder: 1024 samples; some encoders use 2112. Milestone-0
  measurement procedure (mandatory, one-time, result compiled in as a constant
  with a runtime assert): encode a single-sample impulse at a known input PTS,
  decode with ffmpeg, measure impulse position → `delay_samples`. Compensation:
  subtract `delay_samples * 10_000_000 / 48_000` ticks from every output
  packet's PTS and drop any output packet whose entire content is priming.
  Fallback if measurement is skipped: assume 1024 (21.33 ms) — an error here is
  a CONSTANT offset, which the §5 acceptance test will catch immediately.

---

## 3. Ring buffer timestamps and eviction

- Per-stream `VecDeque<Packet { pts, dur, epoch_id, keyframe, bytes }>`.
- Caps (both enforced): `buffer_seconds` (default 120, max 600) and
  `buffer_bytes` = `buffer_seconds × est_bitrate × 1.5` headroom, hard ceiling
  from the table in §6.
- Video eviction: whole GOP only (pop from the front until the front packet is
  an IDR). Audio eviction: pop packets with `pts < video_front_pts − 500 ms`.
  The 500 ms slack guarantees audio always fully covers any video range that
  survives in the buffer.
- GOP: closed GOPs, IDR every 2 s (`gop_frames = 2 × fps`), no B-frames in v1.
  Rationale: B-frames buy ~5–10% bitrate at these CQ levels but complicate
  PTS≠DTS handling in the muxer and add reorder latency; the entire dual-PTS/DTS
  machinery is deleted by this one choice. `precise_mode` tunable sets GOP to
  1 s at ~+10% bitrate for tighter clip starts.

---

## 4. Save-path rebasing (the mux contract)

Given a save request at master time `T_req` for clip length `L`:

1. `target = T_req − L` (in ticks, master domain).
2. Choose `origin` = PTS of the newest video IDR with `pts <= target`, same
   epoch as the newest packet. If the epoch boundary is newer than `target`,
   `origin` = first IDR of the current epoch (clip is shorter than requested;
   log + toast). Worst-case pre-roll slack = one GOP = 2 s.
3. Video samples: all packets with `pts >= origin`. Output PTS =
   `pts − origin`.
4. Audio samples (per track): first packet = first with `pts >= origin`
   (max 21.33 ms of head silence — imperceptible; accepted by design instead of
   partial-AAC-frame surgery). Output PTS = `pts − origin`. Include trailing
   packets until `pts >= last_video_pts + D`.
5. Container numbers (fMP4):
   - movie timescale 1000.
   - video track timescale = `fps × 1000` (60,000 at 60 fps; sample_delta a
     constant 1000 — exact CFR, and 59.94-family rates remain representable).
   - audio track timescale = 48,000; sample_delta = 1024.
   - Convert tick PTS to track timescale with round-half-even; because video
     PTS are exact grid multiples, video conversion is exact; audio rounding
     error is ≤ 10 µs and non-accumulating (each packet converts independently
     from ticks).
6. Fragmenting: one moof/mdat pair per 1 s of content. A crash mid-write loses
   at most the final fragment; everything prior plays.
7. Atomicity: write `name.mp4.part`, FlushFileBuffers, rename to `name.mp4`.

---

## 5. Sync budget and acceptance criteria (numbers that define "correct")

Error budget, end-to-end, steady state:

| Source                         | Bound (ms)    | Mechanism that bounds it            |
|--------------------------------|---------------|-------------------------------------|
| Video grid quantization        | ±8.3 @60fps   | §1.2 round-to-nearest slot          |
| Audio QPC stamp accuracy       | ±0.5          | driver-provided QPCPosition         |
| Residual drift after control   | ±2.0          | §2.4 controller residual            |
| AAC delay compensation error   | 0 (or const)  | §2.6 measured constant              |
| Muxer rounding                 | ±0.01         | §4.5                                |
| **Total (RSS, worst-ish)**     | **≈ ±9 ms**   |                                     |

Acceptance tests (all automated against the click/flash rig; the rig plays an
audible click exactly on a full-screen white flash):

- **AV-1**: 30 s clip, offset of click vs flash ≤ ±16.7 ms (one frame @ 60).
  Expected result per budget: ≤ 10 ms.
- **AV-2 (drift)**: 10-minute timed recording; offset measured in minute 1 vs
  minute 10 differs by ≤ 5 ms. This is THE test that incumbents fail.
- **AV-3 (silence)**: clip containing 60 s of total desktop silence mid-buffer;
  offset after the silent span unchanged (≤ ±16.7 ms) and audio track duration
  within 1 AAC frame of video duration.
- **AV-4 (device chaos)**: unplug/replug default mic during buffering, save a
  clip spanning the event: file plays, gap is silence, no offset change after
  recovery, recovery gap ≤ 750 ms (§7 budget: 250 debounce + 500 rebuild).
- **AV-5 (load)**: run AV-1 while a GPU-saturating game runs (any title pinning
  the 3D engine at 100%). Same threshold. Failures here indict §1.2 grace or
  encoder queueing, not timestamps.

Failing AV-2 by a linear trend = drift controller bug. Failing AV-1 by a
constant = AAC delay constant wrong. The spec is designed so each failure mode
has exactly one suspect.

---

## 6. Dictated tuning tables (estimated real-world numbers)

These are engineering estimates to build against; each row states its
verification hook. Where reality disagrees, the ADJUSTMENT RULE column is
normative — apply it without revisiting this spec.

### 6.1 Encoder quality defaults (CQP mode) and expected average bitrates, H.264

| Preset target      | NVENC CQ | AMF QP | QSV ICQ | Est. avg Mbps | Est. peak Mbps |
|--------------------|----------|--------|---------|----------------|-----------------|
| 1080p60            | 23       | 21     | 22      | 12–20          | 45              |
| 1440p60            | 23       | 21     | 22      | 20–32          | 70              |
| 4K60               | 24       | 22     | 23      | 40–60          | 130             |

Adjustment rule: after Milestone 6, encode the standard 60 s test scene
(fast FPS gameplay w/ smoke) per vendor; if measured avg deviates > 30% from
the table midpoint, shift that vendor's default by 1 QP per 20% deviation
(higher QP = smaller). HEVC: same CQ numbers ≈ same quality at ~0.6× the
bitrate; expose `codec = "h264" | "hevc"`, default h264 (universal edit/share
compatibility beats the size win).

### 6.2 Ring buffer RAM (derived from 6.1; enforce as byte caps)

| Config                    | est_bitrate used | 60 s   | 120 s  | 300 s  |
|---------------------------|------------------|--------|--------|--------|
| 1080p60 H.264 + 2×AAC     | 16 Mbps + 0.4    | 123 MB | 246 MB | 615 MB |
| 1440p60 H.264 + 2×AAC     | 26 Mbps + 0.4    | 198 MB | 396 MB | 990 MB |
| 4K60 H.264 + 2×AAC        | 50 Mbps + 0.4    | 378 MB | 756 MB | 1.9 GB |

Byte cap = table × 1.5. If the byte cap evicts below 90% of `buffer_seconds`
for > 60 s continuously (sustained confetti), raise QP by 1 for the session and
log (`auto_qp_relief = true` tunable, default on). This is the anti-"buffer
silently shrank" rule: the user asked for N seconds; quality bends before
duration does.

### 6.3 Latency/queue thresholds (watchdog triggers)

| Signal                                  | Threshold           | Action                          |
|-----------------------------------------|---------------------|---------------------------------|
| Encoder input queue depth               | > 6 frames          | drop-before-convert, count      |
| frames_in − frames_out divergence       | > 120 (2 s)         | tray WARNING, keep dropping     |
| No WGC frame AND no resubmit possible   | > 1 s               | epoch restart                   |
| No audio event (per stream)             | > 500 ms            | stream rebuild (§7)             |
| Save duration                           | > 1000 ms           | log WARN (disk suspect)         |
| ts_violation counter                    | > 0 per minute      | log WARN                        |
| bad_qpc per minute                      | > 100               | switch mode per §2.2            |

### 6.4 Expected steady-state resource envelope (verification: PresentMon + GPUView, Milestone 1/6)

| Metric                          | Expected     | Budget (fail CI/manual test if over) |
|---------------------------------|--------------|---------------------------------------|
| CPU total (60 fps, 2 audio)     | 0.5–1.5%     | 2%                                     |
| GPU 3D engine                   | ~0% (VP path)| 3%                                     |
| GPU encoder block               | 15–40%       | n/a (dedicated)                        |
| Process RAM beyond ring         | 30–60 MB     | 75 MB                                  |
| Game frametime impact (99th pct)| < 2%         | 4%                                     |

---

## 7. Device-change state machine (timing normative)

States per audio stream: RUNNING → (event) → DRAINING → REBUILDING → RUNNING.

- IMMNotificationClient events are debounced 250 ms (Windows fires bursts of
  3–6 events on a default switch).
- Rebuild budget 500 ms: release client, re-enumerate, initialize, start.
- The gap between last good packet and first new packet is filled with
  synthesized silence via the §2.3 mechanism (it needs no special case — the
  QPC gap math already covers it). Total worst-case hole: 750 ms of silence,
  zero desync, zero crash.
- `AUDCLNT_E_DEVICE_INVALIDATED` from any call = immediate transition to
  REBUILDING (skip debounce).
- Device selection policy (config): `mic = "default-follow"` (default; rebuilds
  chase the Windows default) or a pinned endpoint ID (rebuild only on
  invalidation; if the pinned device is gone, record silence, tray WARNING —
  never silently substitute a different mic; that is the incumbent sin).

Video device loss (DXGI_ERROR_DEVICE_REMOVED/RESET, sleep/resume, driver
update): full pipeline epoch restart, budget 2 s, buffer retained across the
epoch (older epochs remain saveable until evicted normally).

---

## 8. What is deliberately NOT in this spec

- PTS≠DTS handling (deleted by the no-B-frames decision).
- Partial-AAC-frame trimming and MP4 edit lists (deleted by the ≤21.33 ms
  head-silence acceptance).
- VFR output (deleted by the CFR grid).
- Cross-clock NTP-style filtering of QPCPosition (unnecessary: QPCPosition IS
  the master domain already; the only correction loop is sample-rate drift).

Each deletion removes an entire bug class. Resist re-adding them.
