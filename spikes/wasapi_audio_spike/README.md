# Spike: WASAPI loopback + mic capture (Milestone 0, #3)

**Throwaway**, standalone crate. De-risks the audio-clock story (02-AV-SYNC-SPEC
§2), where "60% of the pain lives" (01-PROJECT-PLAN §3). Uses the whitelisted
`wasapi` crate (not cpal) + `hound` (free dev-dep) for WAV.

## What it does
Two concurrent capture threads → two 48 kHz stereo f32 WAVs in `%TEMP%`:
- **desktop-loopback** — default **Render** endpoint opened in loopback.
- **mic** — default **Capture** endpoint.

Per packet it records `BufferInfo { index, timestamp, flags }`, where
`timestamp` is the **QPC-correlated position in 100 ns ticks** — the exact stamp
§2.2 mandates (never sample-count × nominal rate). Reports per stream: packet /
frame counts, captured vs QPC-span seconds, and counts of SILENT /
DATA_DISCONTINUITY / TIMESTAMP_ERROR packets, event-wait timeouts, and the max
timestamp gap.

## How to run
```powershell
just spike wasapi_audio_spike
# or:
cargo run --manifest-path spikes/wasapi_audio_spike/Cargo.toml
```
Runs ~6 s. **During the window:**
1. **Play audio, then let it go fully SILENT** partway through → watch the
   loopback `event_timeouts` rise and `max_gap_ms` jump (pitfall 2: WASAPI
   loopback delivers *nothing* during silence — the "clips desync after the game
   goes quiet" bug). M2 fills that hole with synthesized silence.
2. **Speak into the mic** so its WAV isn't empty.
3. **Optionally UNPLUG the mic** mid-run → the spike must not crash; the stream
   errors/ends and is logged (pitfall 3; full rebuild is §7 / Milestone 2).

## Validate the WAVs
```powershell
ffprobe -v error -show_entries stream=codec_name,sample_rate,channels,sample_fmt,duration `
  -of default=noprint_wrappers=1 "$env:TEMP\clipd_spike_audio_mic.wav"
```

## Measured (Nitro V15, 2026-07-03 — audio was playing, no manual silence yet)
| Stream | Device | Packets | Captured s | QPC span s | silent | disc. | ts_err | timeouts | max_gap ms |
|---|---|---|---|---|---|---|---|---|---|
| desktop-loopback | Headphones (Realtek) | 597 | 5.97 | 5.96 | 0 | 1 | 0 | 0 | 0.5 |
| mic | FIFINE Microphone | 593 | 5.93 | 5.92 | 0 | 1 | 0 | 0 | 0.1 |

- Both WAVs: `pcm_f32le` / 48000 / 2ch, ~6 s. ✅
- Per-packet QPC monotonic, ~100 000 ticks (10 ms) per 480-frame packet — matches
  §2.2. The single `discontinuity` per stream is the expected first-packet flag.
- The `index` (device frame position) increments by exactly the frames delivered
  → no lost data this run.

## Mic-unplug run (2026-07-03) — found + fixed a real bug
Yanking the mic mid-capture **crashed** the first cut: on invalidation the device
handed back a packet with a non-monotonic / garbage `timestamp`, and the gap
arithmetic (`i64` subtraction) panicked with *attempt to subtract with overflow*
— the exact "unplug must not crash" failure pitfall 3 is about. Fixed:
- Device read errors now **end the stream cleanly** (`device_lost=true`, logged),
  keeping the partial WAV — the stream "ends and is logged" as the plan requires.
- Gap math is `i128` + clamped, and a **non-monotonic timestamp is treated as a
  device event** (`non_monotonic` counter, §0 monotonicity), never a silence gap.

**Confirmed on hardware (2026-07-03):** unplug → `error=0x88890004`
(`AUDCLNT_E_DEVICE_INVALIDATED`) → logged, `device_lost=true`, partial WAV kept
(49 pkts / 0.49 s), desktop-loopback stream unaffected, exit 0. No crash. The mic
does **not** auto-recover on reconnect — that teardown+rebuild is the §7
IMMNotificationClient state machine, a **Milestone 2** deliverable; the spike
only proves the unplug is survivable.

## Silence run (2026-07-03) — no gap on this machine (benign)
Played → silent → played across the 6 s window. Desktop-loopback stayed
continuous: `frames=287520` (full 6 s), `event_timeouts=0`, `silent_packets=0`,
`max_gap_ms=0.7`, time-aligned with the mic. **Finding:** on this hardware/OS
(Win11 + Realtek) desktop loopback does *not* drop packets during silence within
a session — the audio engine stays warm and delivers continuous unflagged
near-zero PCM. The classic pitfall-2 gap (loopback delivers nothing when quiet)
is a modern-Windows-mitigated / fully-idle-engine case; it did not reproduce
here. The probe is instrumented to catch it (`event_timeouts` / `max_gap_ms` /
`silent`) if it occurs on other hardware, and M2 keeps the defensive
silence-synthesis path regardless (§2.3).

## Not this spike's job (Milestone 2)
Silence *synthesis*, drift controller (§2.4), AAC encode, device *rebuild*
(§7), IMMNotificationClient. This only proves capture + that the timestamps and
flags carry what §2's math needs.
