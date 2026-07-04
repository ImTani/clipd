# M2 Hardware Test Runbook (audio + A/V sync)

How to run the Milestone-2 hardware validations on the test box (the Nitro V15).
These are the tests that CI **cannot** run — they need a real GPU, real audio
endpoints, and a human to unplug things. Each maps to an M2 exit criterion
(`01-PROJECT-PLAN.md §6`) and an acceptance test in `02-AV-SYNC-SPEC.md §5`.

> Scope: the audio/A/V tests only. This is a living checklist you run per build;
> record outcomes under `testlogs/<date>/SUMMARY.md` (see §8). The spec
> (`02-AV-SYNC-SPEC.md`, frozen) is the source of truth for the numbers.

---

## 0. Before you start (once per session)

**Shell.** Use PowerShell. Put cargo on PATH first, every new shell:
```powershell
$env:Path = "X:\cargo\bin;$env:Path"
```

**Build the shipping binary.** Measure the *release* build — the debug build adds
CPU that can perturb the drift (AV-2) and load (AV-5) results:
```powershell
just release          # builds target\release\clipd.exe, prints size vs budget
```
Use `target\release\clipd.exe` for every recording below. (The rig itself,
`just rig …`, can stay debug — its timing is not perf-critical.)

**Config.** The defaults are already correct for these tests — no config file
needed:
- `[audio].desktop = true` (the click is recorded on **audio track 0**; the rig
  depends on this).
- `[audio].mic = "default-follow"` (mic on track 1, chases the default).

If you have a `config.toml`, confirm those two. `just check-config` prints the
effective config.

**Audio state.** System volume **up and not muted** — WASAPI loopback records the
post-mix render stream, so a muted endpoint records silence and the rig will find
0 clicks. Headphones vs speakers doesn't matter (loopback captures the stream
either way).

**Monitor.** The rig's flash window is full-screen on the **primary** monitor, and
`clipd record` captures the **primary** monitor — they line up by construction. On
a multi-monitor setup, make sure the primary is the one you're watching.

### How the rig works (so the steps make sense)

`tools/avrig` (run via `just rig`) has two modes:

- **`avrig flash`** — a full-screen window that, every 2 s, flashes **white for one
  frame** and at the *same instant* plays a short **click** through the default
  render endpoint. `clipd` records the monitor (sees the flash) and the desktop
  loopback (hears the click) simultaneously, so the **offset between flash and
  click in the saved clip is the pipeline's A/V sync error**.
- **`avrig measure <clip>`** — pulls per-frame luma (video) and the click envelope
  (audio track 0) out of the clip with ffmpeg, detects the flash and click events,
  pairs them, and prints the offset + drift with **AV-1 / AV-2 PASS/FAIL**.

The measurement math is unit-tested (`cargo test --manifest-path
tools/avrig/Cargo.toml`, 6 tests); only the flash generator and the ffmpeg
extraction need hardware.

### The two-process dance (important)

The flash window is full-screen and grabs focus, so you can't start the recorder
*after* it. **Always start the recorder first, then the flash:**

1. In **shell A**, start the recorder (it begins capturing a black screen).
2. **Alt-Tab to shell B** and start the flash. It goes full-screen and the flashes
   begin; the already-running recorder captures them.
3. Both stop on their own `--seconds`. Press **Esc** to end the flash early.
4. Alt-Tab back and run `measure`.

Give the flash a few seconds more than the record so the record window is fully
inside the flashing window (e.g. flash 35 s / record 30 s).

---

## 1. Smoke test — 3-track record (baseline, ~20 s)

Proves the pipeline still produces video + desktop + mic before you measure
anything. (Already validated 2026-07-04, but re-run it after any audio change.)

```powershell
# Play some desktop audio AND talk into the mic during this.
target\release\clipd.exe record --seconds 15 --out smoke.mp4
ffprobe -hide_banner smoke.mp4
```

**Expect:** 3 streams — `Video: h264 … 60 fps` + two `Audio: aac (LC) 48000 Hz,
stereo`. The file plays with **both** desktop sound and your voice. Console shows
two `audio capture started` lines and one `recording finalized`.

---

## 2. AV-4 — device chaos (Task 6, §7)

**Proves:** exit criterion #3 — a device change mid-record does not crash, desync,
or lose the clip; the hole is silence. Budget: recovery gap ≤ 750 ms.

This is manual. Have the FIFINE mic plugged into a USB port you can reach.

### 2a. Mic unplug / replug  ✅ (passed 2026-07-04 — re-run after changes)
```powershell
target\release\clipd.exe record --seconds 30 --out av4_mic.mp4
```
While it records (talk into the mic so there's signal):
1. ~5 s in: **unplug** the mic. Keep talking (into nothing).
2. ~10 s later: **replug** it. Resume talking.
3. Let it finish.

**Expect:**
- No crash; `av4_mic.mp4` finalizes and plays.
- Console logs (they're on the console; `just run` sets `RUST_LOG=debug`, or set
  `$env:RUST_LOG='debug'` before the release exe): a
  `audio device error — rebuilding stream (§7)` on unplug, then a fresh
  `audio capture started` on recovery.
- The **mic track** has a **silence gap** across the unplug window (roughly the
  unplug→replug duration), then audio resumes **in sync** (your voice after the
  gap lines up with the video). The **desktop track and video are unaffected**.
- Recovery gap after replug (silence before your voice returns) ≤ ~750 ms — you
  can eyeball this in an editor, or measure precisely with the ffprobe assertion
  script when it lands (M3).

### 2b. Default render (desktop-output) switch
Have a second render endpoint available (e.g. plug in headphones, or enable
"Stereo Mix" / a second output).
```powershell
target\release\clipd.exe record --seconds 30 --out av4_render.mp4
```
While recording, play continuous desktop audio (music), then **change the default
playback device** in Windows sound settings (Win+Ctrl+V, or Settings → Sound).

**Expect:** console logs `default endpoint changed — rebuilding stream (§7)`
(debounced ~250 ms after the switch), a brief silence on the **desktop track**
during the rebuild, then desktop audio resumes in sync. No crash; video and mic
unaffected.

### 2c. Pinned mic that disappears (optional, §7 "never substitute")
Set `[audio].mic` to a specific endpoint id (from the device properties), start a
record, then unplug that exact device. **Expect:** the mic track goes silent and
stays silent (a WARNING is logged) — clipd does **not** switch to a different mic.
`default-follow` (2a) is the one that chases a new device.

---

## 3. AV-1 — baseline offset (§5, ~40 s)

**Proves:** part of exit criterion #4 — click-vs-flash offset ≤ ±16.7 ms (one
frame @ 60 fps). Expected ≤ 10 ms.

```powershell
# Shell A — start the recorder first:
target\release\clipd.exe record --seconds 30 --out av1.mp4
```
Alt-Tab to **shell B** and start the flash:
```powershell
just rig flash --seconds 35
```
Let both finish (Esc ends the flash early). Then:
```powershell
just rig measure av1.mp4
```

**Expect** the report:
```
detected ~15 flashes (video) and ~15 clicks (audio track 0)
── A/V sync report (~15 paired events) ──
  offset  mean +X.XX ms   min …   max …   sd …
  drift   ±… ms across the clip
  AV-1 (|offset| ≤ 16.7 ms):  PASS
```
- **AV-1 PASS** = every paired offset within ±16.7 ms.
- A small **constant** mean (a few ms) is fine and expected — see §7.

---

## 4. AV-2 — drift over 10 minutes (§5) ★ the incumbent-killer

**Proves:** exit criterion #4's core — the offset drifts by ≤ 5 ms between minute 1
and minute 10. This is the test cheap clippers fail; it validates the whole
drift-correction design (`§2.4`).

```powershell
# Shell A:
target\release\clipd.exe record --seconds 620 --out av2.mp4
```
Alt-Tab to **shell B**:
```powershell
just rig flash --seconds 625
```
Leave the machine alone for ~10.5 minutes (don't sleep/lock it — that's a
different test). Then:
```powershell
just rig measure av2.mp4
```

**Expect:**
```
  drift   ±X.XX ms across the clip (least-squares)
  AV-2 (|drift|  ≤ 5.0 ms):   PASS
```
- **AV-2 PASS** = |drift| ≤ 5 ms. The report fits a line to offset-vs-time across
  ~300 events, so it's robust to per-event jitter.
- A **linear** drift that fails this = a drift-controller bug (`§2.4`), not a
  constant offset. See §7.

---

## 5. AV-3 — mid-clip silence (§5)

**Proves:** exit criterion #2 — a stretch of desktop silence does **not** shorten
the audio track or desync what follows (the `§2.3` loopback-silence fill, exercised
on hardware for the first time here).

The trick: the rig's clicks are *themselves* desktop audio, so you can't just mute.
Instead use **two flash bursts around a true 60 s idle gap**, all in one continuous
record. During the gap, the desktop must be **genuinely silent** — no music, no
notification sounds — so the loopback goes idle (no packets) and `§2.3` fills it.

```powershell
# Shell A — one continuous 150 s record:
target\release\clipd.exe record --seconds 150 --out av3.mp4
```
Then, timed by hand (Alt-Tab to shell B for each flash):
1. **0–30 s:** `just rig flash --seconds 30`  (first click burst).
2. **30–90 s:** let the flash end; **play nothing** — true desktop silence for 60 s.
3. **90–120 s:** `just rig flash --seconds 30`  (second click burst).
4. Record stops at 150 s. Then:
```powershell
just rig measure av3.mp4
```

**Expect:**
- `measure` pairs clicks in both bursts and reports **AV-1 PASS** with **no offset
  jump** between the pre-gap and post-gap events (the fill kept the timeline
  aligned).
- `ffprobe av3.mp4` shows the **desktop audio track duration within ~1 AAC frame
  (~21 ms) of the video duration** — the 60 s silence was *filled*, not dropped.
- Console: you should **not** see `audio silence gap exceeds fill cap` (the cap is
  120 s; a 60 s gap is well under it). Seeing it means the gap ran long or a
  suspend happened.

---

## 6. AV-5 — AV-1 under GPU load (§5)

**Proves:** exit criterion #4 holds under contention — the same ±16.7 ms as AV-1,
but with the 3D engine pinned. A failure here indicts the pacing grid's grace
window (`§1.2`) or encoder queueing, not the timestamps.

Start a GPU-saturating game or a benchmark (anything pinning the 3D engine near
100% on the primary monitor), then run the **AV-1 procedure (§3)** while it runs.

**Expect:** same as AV-1 — `AV-1 PASS`, offsets ≤ ±16.7 ms. If AV-1 passed but
AV-5 fails, the suspect is grid grace / encoder queue depth, per `§5`.

---

## 7. Reading the report / diagnosing a failure

`§5` is designed so each failure mode has exactly one suspect:

| Symptom in `measure` | Meaning | Where to look |
|---|---|---|
| `AV-1 FAIL` by a **constant** offset (mean ≈ each event) | AAC encoder-delay constant wrong | `§2.6` priming — currently the fallback **1024**; the impulse measurement is deferred. A steady ~X ms is that constant. |
| `AV-1 FAIL` with large **sd / jitter** | grid quantization or encoder queueing | `§1.2` grace window, encode channel depth |
| `AV-2 FAIL` — a **linear** drift | drift controller bug | `audio/drift.rs`, `resample.rs` (`§2.4`) |
| Duration mismatch after a silent span (AV-3) | silence not filled / over-filled | `audio/gaps.rs`, `resample.rs` gap path (`§2.3`), the 120 s cap |
| `detected 0 clicks` | click not recorded | volume muted, desktop loopback off, or the flash was on a monitor clipd didn't capture |

A **constant** mean offset is *expected* and is NOT a drift failure — it's the
priming/rig-latency term AV-1 tolerates and AV-2 cancels. Only a *trend* fails
AV-2.

---

## 8. Recording results

Log each run under `testlogs/<YYYY-MM-DD>/SUMMARY.md` (that path is git-tracked;
the rest of `testlogs/` is ignored — `07-DEVFLOW §6`). For each test note: build
commit (`git rev-parse --short HEAD`), the `measure` report (or ffprobe output),
PASS/FAIL, and anything odd in the console log. When AV-1..AV-5 + AV-4 all pass,
M2's exit criteria are met and `m2-audio` is ready to **merge into `main`**.

---

## 9. Quick reference

```powershell
$env:Path = "X:\cargo\bin;$env:Path"        # every shell
just release                                # build target\release\clipd.exe

# 3-track smoke
target\release\clipd.exe record --seconds 15 --out smoke.mp4 ; ffprobe -hide_banner smoke.mp4

# rig: recorder in shell A FIRST, then flash in shell B (Alt-Tab)
target\release\clipd.exe record --seconds 30  --out av1.mp4   # A
just rig flash --seconds 35                                   # B
just rig measure av1.mp4                                      # after both stop

# AV-2 (10 min): record 620 / flash 625, then measure
# AV-4: record, then unplug/replug the mic or switch the default output

cargo test --manifest-path tools/avrig/Cargo.toml            # the rig's own math
```

Tips: run `measure` from the folder holding the clip and pass a **relative** path
(avoids `movie=` filter escaping for `C:\…` paths). `just rig` with no args prints
usage. Press **Esc** to end a flash run early.
