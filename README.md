# clipd

**A lightweight game clipper that doesn't suck. Written in Rust.**

You just did something ridiculous in a game and it's already gone. `clipd` sits
in your tray quietly holding the last few minutes of your screen in memory — hit
a hotkey and the last *N* seconds drop onto your disk as an MP4. That's the whole
app.

[![CI](https://github.com/ImTani/clipd/actions/workflows/ci.yml/badge.svg)](https://github.com/ImTani/clipd/actions/workflows/ci.yml)
[![License: GPL-3.0-only](https://img.shields.io/badge/license-GPL--3.0--only-blue.svg)](LICENSE)
![Platform: Windows 10 1903+ or 11](https://img.shields.io/badge/platform-Windows%2010%201903%2B%20%7C%2011-0078D6)
![Rust 1.95+](https://img.shields.io/badge/rust-1.95%2B-orange)

Yeah, ShadowPlay, AMD Radeon, Xbox Game Bar and Medal all technically do this
already. They're also each locked to one GPU brand, buried inside a 300 MB
control panel, mangling your mic audio, or nagging you to make an account and
upload your clips to their site. `clipd` is the ~9 MB version that clips and then
leaves you alone.

<!-- TODO(screenshot): add a tray + saved-clip demo here once captured, e.g.
       ![clipd in the tray](assets/screenshot.png)
     A 60–90s hotkey→file screen-recording is the single highest-value asset. -->

## The good parts

- **It won't cost you frames.** The heavy lifting runs on your GPU's dedicated
  video encoder (NVENC on NVIDIA, AMF on AMD) — the same silicon ShadowPlay and
  OBS lean on — so your game keeps the actual GPU. On the test machine it sits
  under 2% CPU while buffering,
  roughly 0% of the gaming GPU, and the clip is on disk in under a second.
- **Press one key, it's saved.** A global hotkey grabs the last *N* seconds. Want
  the opposite? A second mode records the next *N* minutes straight to disk.
- **It won't get you banned.** No injection, no hooks, no overlay — it just uses
  Windows' own screen-capture API. Nothing for anti-cheat to freak out about, and
  it can't crash the game it's recording.
- **Your mic gets its own track.** So "my voice was blowing out the mic" is a
  post-processing fix, not a ruined clip. Live meters show the mic's actually
  working — unlike a certain built-in Windows recorder.
- **Zero runtime dependencies.** One ~9 MB exe. No FFmpeg DLLs, no VC++ redist, no bundled
  browser, no updater lurking in the background. The config is a plain text file
  it never rewrites behind your back.
- **When something breaks, it says so.** Every save attempt is logged with what
  happened. No clip? The reason's in the log, and the tray icon changes colour the
  second anything goes wrong. No more "wait, was it even recording?"

Not tied to NVIDIA, either — it's built on Windows' own encoder API instead of a
vendor SDK, so it isn't locked to one GPU brand the way ShadowPlay (NVIDIA-only)
and Radeon (AMD-only) are. So far it's run cleanly on both NVIDIA (RTX 4050
laptop) and AMD (Radeon RX 9060 XT); Intel and a wider spread of cards are still
to come.

## Status & roadmap

> **Alpha, preparing for a private beta.** The core capture → save engine is
> built and hardware-validated on the main test machine (RTX 4050 laptop, hybrid
> graphics), and an early test on an AMD Radeon RX 9060 XT ran clean too. The
> settings/status window and multi-track audio are built and merged; their
> hardware-acceptance pass is underway. A wider card spread (more AMD, Intel Arc)
> and Windows 10 come next — that's what the beta is for.

**Working today (hardware-validated on the test machine)** — screen/window
capture, GPU-accelerated H.264 encode with separate mic + game audio, the
in-memory replay buffer, the sub-second hotkey save, record-to-disk mode, and the
tray app (status icons, menu, logs, watchdog warning, start-with-Windows).

**Built, final testing in progress** — the settings & live-status window (which
is optional — the recorder runs perfectly fine whether or not you ever open it),
live audio meters, a recent-clips list, click-to-rebind hotkeys, and per-app
audio splitting (game / voice-chat / other / mic).

**Planned** — broader GPU coverage (more AMD cards, Intel Arc) and Windows 10,
AV1 + HDR + 120 fps, and release polish (code-signed builds, winget, an installer).
Full detail in the [milestone tracker](clipper-devpack/devpack/05-MILESTONE-TRACKER.md).

## What it deliberately does *not* do

These aren't missing features — they're the whole point. `clipd` is the clip
button, not a content suite:

- **No overlay and no game injection** — anti-cheat safety and zero risk of
  crashing your game.
- **No editor and no uploader.** Every OS ships a trimmer; every site wants your
  clips. `clipd` gives you a clean MP4 and gets out of the way.
- **No accounts, no telemetry, no auto-update phoning home.**
- **No streaming, no webcam, no "AI auto-highlights."**
- **Windows 10 1903+ / Windows 11 only** in v1. (Windows' borderless-window
  capture needs 1903.)

## Honest limitations (worth knowing before you rely on it)

- **Exclusive-fullscreen games** can't be captured as a single window; `clipd`
  falls back to recording the whole monitor. **Running the game in borderless is
  recommended** — it also keeps the global hotkey working, since a few
  exclusive-fullscreen titles swallow it.
- **DRM-protected video** (e.g. Netflix with hardware DRM) records as black
  frames. That's Windows protecting the content, not a bug.
- **HDR** is currently converted down to SDR (like ShadowPlay does by default);
  true HDR passthrough is a later milestone.
- **Pause** stops *keeping* new footage but keeps capturing — it's a privacy
  control, not a way to drop resource use to zero.
- **Per-app audio splitting** has real edges: in-game voice can't be separated
  out, and players/uploads only ever hear the combined mix. It needs Windows 10
  2004+.

The full, unvarnished list is in [`LIMITATIONS.md`](LIMITATIONS.md).

## Get it & build

`clipd` is free software distributed as **source** (GPL-3.0). Until signed
release builds land, you build it yourself — clone the repo or grab a source
archive from [tags/releases](https://github.com/ImTani/clipd/tags). You'll need
the Rust **MSVC** toolchain (the exact version is pinned in
[`rust-toolchain.toml`](rust-toolchain.toml) and installed by rustup
automatically) and [`just`](https://github.com/casey/just):

```sh
just release  # locked, stripped release build → target/release/clipd.exe
              # (prints the binary size against the 10 MB budget)
```

First-run help and a starting config are in [`dist/`](dist/)
([`QUICKSTART.txt`](dist/QUICKSTART.txt) and
[`config.template.toml`](dist/config.template.toml)). `clipd --check-config`
validates and prints your effective configuration.

## For developers

`clipd` is a single native binary in Rust — no async runtime, no FFmpeg, no
vendor encoder SDKs. Four long-lived worker threads plus the tray thread talk
over bounded channels:

- **capture** — Windows Graphics Capture → D3D11 VideoProcessor BGRA→NV12.
  Pixels stay on the GPU and are never copied into system RAM.
- **encode** — Media Foundation hardware H.264 MFT + dual-AAC, driving
  NVENC/AMF/QuickSync under one API.
- **audio** — WASAPI loopback + mic → `rubato` resample → MF AAC.
- **buffer/mux** — a ring of *compressed* packets capped by both duration and
  bytes; a save walks back to a keyframe, rebases both tracks against one origin,
  and writes a crash-safe fragmented MP4 atomically.

Everything is timestamped in one clock domain: `QueryPerformanceCounter`, in
100 ns ticks. The normative specs live in
[`clipper-devpack/devpack/`](clipper-devpack/devpack/) —
[`02-AV-SYNC-SPEC.md`](clipper-devpack/devpack/02-AV-SYNC-SPEC.md) (the frozen
timestamp/sync spec) overrides everything. Internal engineering notes and the
decision log are under [`docs/`](docs/).

The dev loop:

```sh
just check    # cargo check + clippy -D warnings + fmt --check   (the gate)
just test     # unit tests (nextest, or cargo test)
just run      # debug build + run with dev config + verbose tracing
```

See [`CONTRIBUTING.md`](CONTRIBUTING.md) before opening a PR.

## License

[GPL-3.0-only](LICENSE). The source is free software; the compiled binary may be
sold. See [`docs/DECISIONS.md`](docs/DECISIONS.md) for the rationale.
