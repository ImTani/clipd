# Spike: MF Sink Writer viability — the mux decision (Milestone 0, #4)

**Throwaway**, standalone crate. Answers 01-PROJECT-PLAN §5.2: *can the MF Sink
Writer take pre-encoded H.264 samples with our timestamps and mux them without
re-encoding or fighting us?* Reuses spike #1's NVENC path, but routes each
encoded `IMFSample` into an `IMFSinkWriter` in **passthrough** (sink input media
type == output media type == H.264 ⇒ no encoder MFT inserted) → `.mp4`.

## How to run
```powershell
just spike sinkwriter_mux_spike
cargo run --manifest-path spikes/sinkwriter_mux_spike/Cargo.toml
```

## Validate
```powershell
ffprobe -v error -show_entries format=format_name,duration,bit_rate `
  -show_entries stream=codec_name,profile,codec_tag_string,r_frame_rate,avg_frame_rate,nb_frames `
  -of default=noprint_wrappers=1 "$env:TEMP\clipd_spike_sinkwriter.mp4"
ffmpeg -v error -i "$env:TEMP\clipd_spike_sinkwriter.mp4" -f null -
```

## Measured (Nitro V15 / RTX 4050, 2026-07-03)
| Check | Result |
|---|---|
| container | `mp4`, `codec_tag_string=avc1` (proper `avcC`/SPS-PPS box) |
| codec / profile | `h264` / `Main`, 1280×720 yuv420p |
| **r_frame_rate / avg_frame_rate** | **`60/1` / `60/1`** — exact CFR, our grid honored |
| **duration** | **`2.000000`** s = 120 / 60 — timestamps preserved |
| nb_frames / nb_read_frames | 120 / 120 |
| bit_rate | ~116 kbps ≈ spike #1's raw stream → **no re-encode** (passthrough) |
| ffmpeg decode | 0 errors |

## Finding
**The Sink Writer is viable for correctness**: it accepts pre-encoded H.264 in
passthrough, does not re-encode (bitrate preserved), and honors our QPC-grid
timestamps to an exact 60 fps CFR / 2.000 s MP4 with a valid `avcC`. That's a
useful de-risking result — MF will not fight us on timestamps.

## Decision: **hand-rolled fragmented MP4**, not the Sink Writer
02-AV-SYNC-SPEC §4 is **frozen** and overrides the plan. It requires:
- **Crash-safety**: one `moof`/`mdat` fragment per 1 s so a crash mid-write still
  yields a playable file (§4.6). The Sink Writer's default MP4 writes `moov` at
  `Finalize()` — a crash before that = an unplayable file, which is *exactly* the
  "user pressed the button and got nothing" failure this product exists to kill.
- **Atomic write** `name.mp4.part` → `FlushFileBuffers` → rename (§4.7).
- **Explicit timestamp rebasing** against the cut keyframe's origin (§4.2), on
  slices pulled from the packet ring — full control the Sink Writer's owned
  timing pipeline does not give.

So `mux/fmp4.rs` (hand-rolled) is the v1 path, per the frozen spec. The Sink
Writer stays a documented **fallback** (proven to passthrough-mux correctly here)
if the hand-rolled writer ever hits a wall. Recorded in DECISIONS.md.

## Not this spike's job
Fragment/atomic-write/rebasing logic (that's the M3 `mux/` + `save.rs`
deliverables); audio track muxing; the ring buffer.
