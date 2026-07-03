# Spike: MF async hardware H.264 encoder (Milestone 0, #1)

**Throwaway** (CLAUDE.md `/spikes` rule — never linked into `clipd`). Standalone
crate: its own `[workspace]` + `target/`, so the core build and CI never touch
it. Proves the highest-risk component (01-PROJECT-PLAN.md §5.1, pitfall 17): the
**asynchronous Media Foundation Transform state machine** and the **D3D11 device
manager** plumbing, feeding GPU-resident NV12 textures and getting a decodable
H.264 stream back.

## What it does
Synthetic NV12 frames (moving luma gradient, neutral-grey chroma) → hardware
H.264 encoder MFT → `%TEMP%\clipd_spike_mf_h264.h264` (Annex-B elementary
stream). 1280×720, 60 fps grid, 120 frames (2 s), 8 Mbps CBR.

## How to run
```powershell
just spike mf_h264_encoder
# or directly:
cargo run --manifest-path spikes/mf_h264_encoder/Cargo.toml
```
`RUST_LOG=debug` for more detail. Output path is printed on the last line.

## Validate the output (this is the gate — 00-README §4)
```powershell
# 1. Frame count, codec, geometry — the assertion that matters:
ffprobe -v error -count_frames -select_streams v:0 `
  -show_entries stream=codec_name,profile,width,height,pix_fmt,nb_read_frames `
  -of default=noprint_wrappers=1 "$env:TEMP\clipd_spike_mf_h264.h264"

# 2. Full decode must produce ZERO errors:
ffmpeg -v error -i "$env:TEMP\clipd_spike_mf_h264.h264" -f null -
```

## Expected result (measured on the Nitro V15 / RTX 4050, 2026-07-03)
| Check | Expected |
|---|---|
| Startup log | `adapter=NVIDIA GeForce RTX 4050 Laptop GPU`, `encoder=NVIDIA H.264 Encoder MFT` |
| Enumerated MFTs | `count` ≥ 1 hardware encoder |
| `provides_samples` | `true` (NVENC allocates its own output samples) |
| `frames_in` / `frames_out` | **120 / 120** (drain complete, no lost tail) |
| ffprobe `codec_name` / `profile` | `h264` / `Main` |
| ffprobe `width`×`height` / `pix_fmt` | `1280`×`720` / `yuv420p` |
| ffprobe `nb_read_frames` | **120** |
| `ffmpeg … -f null -` | exits 0, **no error lines** |

Actual first run: 120/120 frames, 27 767 bytes, full decode clean. ✅

## Known non-goals of THIS spike (deliberate — do not "fix" here)
- **Raw `.h264` has no container frame rate** → ffprobe shows a default
  `25/1 avg_frame_rate`. Real timing/CFR is the muxer's job (spike #4 / M1). The
  true rate is in the SPS the stream carries.
- **Colours are grey.** Chroma is neutral 128; BT.709-limited correctness is a
  Milestone-1 concern (pitfall on BT.601-vs-709 + range).
- **Bitrate rate-control**, not CQP. Spec wants CQP (§6.1); the spike uses a
  plain average-bitrate target to prove the path. CQP/CODECAPI tuning is M1+.
- **No `.mp4`.** Muxing (Sink Writer vs hand-rolled fMP4) is Milestone-0 spike
  #4, tracked separately.
- **Per-frame DEFAULT texture allocation** and a CPU-filled STAGING upload are
  spike conveniences. The product path hands the encoder the WGC capture texture
  directly (zero system-RAM copy).

## If it FAILS on other hardware
- No MFT enumerated → the machine has no hardware H.264 encoder for NV12, or the
  driver is too old. (AMD/Intel each expose their own MFT; the enum is
  vendor-agnostic.)
- `MFCreateDXGISurfaceBuffer` / `ProcessInput` errors → try creating the NV12
  input texture with `BindFlags = D3D11_BIND_RENDER_TARGET` (some drivers want
  it); noted here rather than pre-applied to keep the spike minimal.
- Empty/short output, or `ffmpeg` reports "no frame!" → the encoder emitted
  length-prefixed (AVCC) NALs instead of Annex-B; prepend the
  `MF_MT_MPEG_SEQUENCE_HEADER` blob and/or convert. Not needed on NVENC.
