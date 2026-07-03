# Spike: WGC primary-monitor capture (Milestone 0, #2)

**Throwaway**, standalone crate (own `[workspace]` + `target/`, never linked into
`clipd`). Proves the Windows.Graphics.Capture path: `GraphicsCaptureItem` for the
primary monitor → free-threaded `Direct3D11CaptureFramePool` → reach the backing
`ID3D11Texture2D`. **Pixels stay on the GPU** — we read only the texture
*descriptor* (format/size), never map its bytes.

## What it does / reports
- `GraphicsCaptureSession::IsSupported()`, adapter.
- Primary output colour space → **SDR vs HDR**, and the pool pixel format chosen
  from it (`B8G8R8A8UIntNormalized` 87 vs `R16G16B16A16Float` 10).
- **Actual `DXGI_FORMAT` + size** of the first captured texture, asserted against
  the colour space's prediction (pitfall 12).
- **Measured fps** over ~3 s (reflects on-screen activity — WGC delivers a frame
  per DWM present).

## How to run
```powershell
just spike wgc_capture_spike
# or:
cargo run --manifest-path spikes/wgc_capture_spike/Cargo.toml
```
**Wiggle the mouse or play a video during the 3 s window** or fps reads low (a
static desktop presents rarely). fps here is a liveness signal, not a CFR
measurement (that's the pacing grid, M1).

## Two runs required — SDR and HDR
The tracker item wants both. This machine's panel is SDR by default:
1. **SDR run** (default): expect `hdr=false`, format **87** (BGRA8).
2. **HDR run**: toggle HDR on (**Win+Alt+B**, or Settings → System → Display →
   Use HDR), then re-run. Expect `hdr=true`, `bits_per_color≥10`, and first-frame
   format **10** (`R16G16B16A16_FLOAT`). Toggle HDR back off afterward.

## Expected / measured (Nitro V15 / RTX 4050)
| Check | SDR (measured 2026-07-03) | HDR (expected) |
|---|---|---|
| `IsSupported()` | true | true |
| `hdr` | `false` | `true` |
| `color_space` | `0` (RGB_FULL_G22_P709) | `12` (RGB_FULL_G2084_P2020) |
| `bits_per_color` | `8` | `10` (or more) |
| requested pool format | `87` | `10` |
| first-frame `actual_format` | **`87`** | **`10`** |
| `matches` | **true** ✅ | true |
| size | `1920×1080` | `1920×1080` |
| fps (static screen) | ~28 (85 frames/3 s) | up to panel refresh w/ motion |

**HDR run is still OUTSTANDING** — run step 2 above and paste the line to close
the tracker item fully.

## Notes for later milestones (not this spike's job)
- **Hybrid graphics (pitfall 14):** the default D3D11 hardware device landed on
  the **RTX 4050 (dGPU)** and WGC still delivered BGRA8 textures for the
  (normally iGPU-driven) 1080p panel — the cross-adapter copy WGC does for you.
  M1 must *deliberately* enumerate + co-locate the encoder with the capture
  texture's adapter (04-TEST-MACHINE.md adapter-topology task); don't rely on the
  default pick.
- No colour conversion / NV12 / encode here — spike #1 (encoder) and M1 own that.
- `IsBorderRequired`/cursor toggles (pitfalls 9–10) are config surface for M1+,
  not exercised here.
