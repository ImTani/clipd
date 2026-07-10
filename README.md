# clipd

A single-binary, native **Windows** replay-buffer clipper, written in Rust.

`clipd` runs quietly in the tray and continuously captures your screen (a monitor
or the focused window) into an in-memory ring buffer of **compressed** video +
audio. One hotkey saves the last *N* seconds to an MP4. A second mode records the
next *N* minutes straight to disk. That is the whole product.

> **Status: alpha.** The full engine — WGC capture (monitor or focused window),
> D3D11 VideoProcessor colour convert, Media Foundation H.264 + AAC hardware
> encode, the compressed ring buffer, hotkey save, and record-to-disk — is built
> and hardware-validated on the test machine through Milestone 4. Milestone 5
> (shell & trust) adds the tray icon + menu, a rotating file log, the watchdog →
> tray warning, and the start-with-Windows toggle. Milestones are tracked in
> [`clipper-devpack/devpack/05-MILESTONE-TRACKER.md`](clipper-devpack/devpack/05-MILESTONE-TRACKER.md).

## Non-goals (load-bearing — this list is the design)

These are deliberate, permanent exclusions, not v1 shortcuts:

- **No overlay, no injection** into game processes (anti-cheat safety + zero risk
  of crashing games).
- **No editor, no uploader, no accounts, no telemetry, no auto-update** phoning
  home.
- **No streaming.**
- **No webcam.**
- **No game detection / auto-clip AI.**
- **No cross-platform in v1** — Windows 10 1903+ / Windows 11 only (Windows
  Graphics Capture requires 1903 for borderless window capture).

## Design in one breath

Four long-lived threads plus the tray thread, communicating over bounded
channels — no async runtime. Pixels stay on the GPU (WGC texture → D3D11
VideoProcessor BGRA→NV12 → Media Foundation hardware H.264, never copied to
system RAM). One master clock: QueryPerformanceCounter, in 100 ns ticks. The ring
holds compressed packets only, capped by **both** duration and bytes. A save
walks back to a keyframe, rebases both tracks against one origin, and writes a
crash-safe fragmented MP4 atomically. No FFmpeg, no vendor SDKs — Media
Foundation only.

The normative specifications live in [`clipper-devpack/devpack/`](clipper-devpack/devpack/);
[`02-AV-SYNC-SPEC.md`](clipper-devpack/devpack/02-AV-SYNC-SPEC.md) (the frozen
timestamp/sync spec) overrides everything.

## Honest limitations (by design)

- **Exclusive-fullscreen games** can't be window-captured; `clipd` falls back to
  capturing the monitor. Borderless is recommended.
- **DRM-protected content** (e.g. Netflix with hardware DRM) captures as black
  frames — by design, not a bug.
- **HDR** is tone-mapped to SDR in v1; HDR passthrough is a later milestone.
- **Global hotkeys** use `RegisterHotKey`; some exclusive-fullscreen titles
  swallow it — use the tray menu, or run the game borderless.
- **Resized captured windows are letterboxed** into a fixed canvas (a clip can
  span a resize at one resolution instead of being cut).
- **Pause** stops *retaining* new footage but keeps capturing, so it is a privacy
  control, not a way to drop CPU/GPU usage to zero.
- **Multi-track audio** (`separate_tracks = true`) splits system audio into Game /
  Voice-chat / Other-system tracks, but **in-game voice can't be separated**, the
  Other-system track **double-counts** a detected voice app, and uploads/players hear
  only the **Mix** (track 1). Per-app tracks need Windows 10 2004+.

The full list is in [`LIMITATIONS.md`](LIMITATIONS.md).

## Why didn't my clip save?

Every save attempt is logged — with its outcome — to a daily-rolled file under
`%LOCALAPPDATA%\clipd\logs\`. If a clip is missing, the reason is there (`clip
saved`, `clip save FAILED`, `save skipped`, or a slow-write warning), and a
pipeline stall turns the tray icon to its warning colour. This is the trust
model: the log always answers *why*.

## Building

Requires the Rust MSVC toolchain (pinned in `rust-toolchain.toml`) and
[`just`](https://github.com/casey/just).

```sh
just check    # cargo check + clippy -D warnings + fmt --check
just test     # unit tests (nextest, or cargo test)
just release  # locked, stripped release build; prints size vs the 10 MB budget
```

`clipd --check-config` validates and prints the effective configuration.

## License

[GPL-3.0-only](LICENSE). The source is free software; the compiled binary may be
sold. See [`docs/DECISIONS.md`](docs/DECISIONS.md) for the rationale.
