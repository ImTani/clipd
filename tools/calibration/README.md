# tools/calibration

Development-only calibration harnesses. Not linked into `clipd`; run manually on the
test machine. Windows-only (they drive the real hardware encoder).

## `t0_sweep.ps1` — encoder rate-control probe (T0)

Reproducible evidence behind the **§6.1 amendment** recorded in
[`DECISIONS.md`](../../docs/DECISIONS.md) ("2026-07-07 — T0 resolution") and
[`M7-M8-PLAN.md`](../../docs/plans/M7-M8-PLAN.md) §1. Keep it: re-run it on any new GPU to
re-confirm the rate-control behaviour before trusting the shipping defaults there.

### What it establishes

The frozen spec (`02-AV-SYNC-SPEC.md` §6.1) mandates **constant-QP (CQP)** rate control.
On the Nitro's NVENC H.264 MFT (Media Foundation, the only encode path allowed by
`CLAUDE.md` — no FFmpeg/vendor SDK) that is **unreachable**:

- `AVEncCommonQuality` (0–100) is **accepted but a no-op** — bitrate is flat as it sweeps
  55→85 in both `Quality` and `UnconstrainedVBR` modes.
- `AVEncVideoEncodeQP` (true CQP) is **rejected** (`E_INVALIDARG`) in every mode.
- `MF_MT_AVG_BITRATE` / `AVEncCommonMeanBitRate` is the **only** lever that moves output,
  and it tracks the target precisely (16 Mbps → 16.4; 60 Mbps → 60.4).

So the shipping encoder targets a bitrate via **PeakConstrainedVBR** (average = the §6.2
table, peak = 1.5×). Measured content-adaptivity at the 16 Mbps 1080p default: mandelbrot
16.4 / testsrc2 15.5 / static desktop 6.0 Mbps.

### How it works

Plays deterministic ffmpeg `lavfi` content (`mandelbrot` = worst case, `testsrc2` =
moderate) in a **borderless window** — *not* exclusive `-fs`, which bypasses DWM, starves
WGC monitor capture, and hangs the encoder — while `clipd record` captures the primary
monitor through hidden `--encode-*` hooks. Each run has a hard timeout that force-kills
`clipd`, so a bad config can never stall the sweep. Results (video bitrate per config) are
printed and written to `%TEMP%\clipd_t0out\probe_results.csv`.

### Hidden `--encode-*` calibration hooks

`record` and `buffer` accept these (absent from `--help`; all-absent = the shipping path).
They map to `EncoderOverrides` in `src/encode/mft_h264.rs` and are reused by Slice A's
quality-tier work:

| Flag | Effect |
|------|--------|
| `--encode-rc-mode <cbr\|pcvbr\|uvbr\|quality\|ldvbr\|gvbr>` | `eAVEncCommonRateControlMode` |
| `--encode-quality <0-100>` | `AVEncCommonQuality` (measured no-op) |
| `--encode-qp <n>` | `AVEncVideoEncodeQP` constant QP (rejected on NVENC-MF) |
| `--encode-avg-bitrate <bps>` | `MF_MT_AVG_BITRATE` + `AVEncCommonMeanBitRate` |
| `--encode-max-bitrate <bps>` | `AVEncCommonMaxBitRate` peak cap |

### Run

```powershell
powershell -ExecutionPolicy Bypass -File tools\calibration\t0_sweep.ps1
```

Prereqs: `ffmpeg`/`ffplay`/`ffprobe` on PATH, a hardware H.264 encoder. Commandeers the
primary display for ~4 min. Requires a release build (it builds one).
