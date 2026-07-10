# clipd

A single-binary, native **Windows** replay-buffer clipper, written in Rust.

[![CI](https://github.com/ImTani/clipd/actions/workflows/ci.yml/badge.svg)](https://github.com/ImTani/clipd/actions/workflows/ci.yml)
[![License: GPL-3.0-only](https://img.shields.io/badge/license-GPL--3.0--only-blue.svg)](LICENSE)
![Platform: Windows 10 1903+ or 11](https://img.shields.io/badge/platform-Windows%2010%201903%2B%20%7C%2011-0078D6)
![Rust 1.95+](https://img.shields.io/badge/rust-1.95%2B-orange)

`clipd` runs quietly in the tray and continuously captures your screen (a monitor
or the focused window) into an in-memory ring buffer of **compressed** video +
audio. One hotkey saves the last *N* seconds to an MP4. A second mode records the
next *N* minutes straight to disk. That is the whole product — a single ~9 MB
binary with **zero runtime dependencies**: no FFmpeg DLLs, no VC++ redist, no
WebView, no account, no telemetry.

<!-- TODO(screenshot): add a tray + saved-clip demo here once captured, e.g.
       ![clipd in the tray](assets/screenshot.png)
     A 60–90s hotkey→file screen-recording is the single highest-value asset. -->

## Status & roadmap

> **Alpha, preparing for a private beta.** The capture → encode → ring → save
> engine is built and hardware-validated on the test machine (RTX 4050 laptop,
> hybrid graphics) through the shell & trust milestone. The settings/status UI
> and multi-track audio are built and merged; their hardware-acceptance pass is
> underway. A multi-vendor hardware matrix (AMD, Intel, Windows 10) comes next,
> supplied by the beta.

**Done & hardware-validated** — WGC capture (monitor + focused window), D3D11
VideoProcessor BGRA→NV12, Media Foundation H.264 + dual-AAC hardware encode, the
compressed dual-capped ring buffer, sub-100 ms hotkey save with keyframe
walk-back + atomic write, record-to-disk mode, and the tray shell (state icons,
menu, rotating log, watchdog → tray warning, start-with-Windows).

**Built, hardware-acceptance in progress** — the egui settings & live-status
window (a *satellite*: the engine runs forever whether or not it opens), live
audio meters, a recent-clips list, press-to-bind hotkeys; and multi-track /
per-app audio (game / voice-chat / other-system / mic).

**Planned** — the multi-vendor hardware matrix (AMD AMF, Intel QSV, Windows 10),
AV1 + HDR passthrough + 120 fps, and release engineering (code-signing, winget,
installer). Full detail in the
[milestone tracker](clipper-devpack/devpack/05-MILESTONE-TRACKER.md).

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
timestamp/sync spec) overrides everything. Internal engineering notes and the
decision log are under [`docs/`](docs/).

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

## Get it & build

clipd is distributed as **source** (GPL-3.0) — clone the repository or download a
source archive from the [tags/releases](https://github.com/ImTani/clipd/tags),
then build it yourself. Building needs the Rust **MSVC** toolchain (the exact
version is pinned in [`rust-toolchain.toml`](rust-toolchain.toml) and installed
by rustup automatically) and [`just`](https://github.com/casey/just):

```sh
just release  # locked, stripped release build → target/release/clipd.exe
              # (prints the binary size against the 10 MB budget)
```

First-run help and a starting config are in [`dist/`](dist/)
([`QUICKSTART.txt`](dist/QUICKSTART.txt) and
[`config.template.toml`](dist/config.template.toml)). `clipd --check-config`
validates and prints the effective configuration.

Working on clipd? The dev loop:

```sh
just check    # cargo check + clippy -D warnings + fmt --check   (the gate)
just test     # unit tests (nextest, or cargo test)
just run      # debug build + run with dev config + verbose tracing
```

See [`CONTRIBUTING.md`](CONTRIBUTING.md) before opening a PR.

## License

[GPL-3.0-only](LICENSE). The source is free software; the compiled binary may be
sold. See [`docs/DECISIONS.md`](docs/DECISIONS.md) for the rationale.
