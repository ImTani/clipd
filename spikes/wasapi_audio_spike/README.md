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

**STILL OUTSTANDING (manual, to close the tracker item):**
- **Silence run:** play → go silent → confirm loopback `event_timeouts` /
  `max_gap_ms` rise. This proves pitfall 2 is *observable* (M2 will fix it).
- **Mic-unplug run:** yank the mic mid-capture → confirm no crash + a logged
  error, per pitfall 3.

## Not this spike's job (Milestone 2)
Silence *synthesis*, drift controller (§2.4), AAC encode, device *rebuild*
(§7), IMMNotificationClient. This only proves capture + that the timestamps and
flags carry what §2's math needs.
