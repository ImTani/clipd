# Test Machine Profile — Acer Nitro V15 (RTX 4050 Laptop + Intel iGPU)

This machine is the primary and, initially, only hardware test platform.
This document maps the plan's hardware-dependent items onto it.

## Why this machine is a strong test platform
1. **Hybrid graphics is the default here.** On Optimus laptops the internal
   display is normally driven by the Intel iGPU while games render on the RTX
   4050 and frames are copied across. That is pitfall #14 (cross-adapter
   capture) — the hardest capture topology — as the OUT-OF-THE-BOX state.
   If the pipeline is correct on this machine, desktops are the easy case.
2. **Two encoder vendors on one machine.** RTX 4050 = Ada-generation NVENC
   (H.264, HEVC, AV1 encode). The 13th-gen Intel iGPU = QuickSync (H.264,
   HEVC; AV1 encode only on Arc, not on UHD-class iGPUs). Milestone 6's
   NVENC and QSV rows are locally testable; only AMD/AMF requires borrowed
   hardware or a helper with a Radeon.

## Machine-specific facts and procedures

### Adapter topology (do this before Milestone 1 testing)
- Enumerate adapters with the Milestone-0 WGC spike and record which adapter
  WGC hands textures on for (a) internal display, (b) an external monitor if
  one is plugged in (external ports on many Nitro V15 units are wired to the
  dGPU — verify, don't assume).
- Windows Settings > Display > Graphics lets you force per-app GPU. Also check
  the NVIDIA app for a MUX/"Advanced Optimus" display-mode switch; some Nitro
  V15 SKUs have it, some don't. If present, BOTH mux states are test
  configurations (iGPU-driven display AND dGPU-driven display) — that doubles
  the topology coverage of this one laptop.
- Encoder co-location rule from the plan: encode on the adapter that owns the
  captured texture. On this machine the interesting case is game-on-dGPU +
  display-on-iGPU: verify whether WGC delivers an iGPU-resident texture (then
  QSV co-location vs NVENC cross-copy is a real measured tradeoff — measure
  both, record numbers in DECISIONS.md).

### NVENC specifics (RTX 4050 Laptop)
- Ada NVENC: excellent quality; supports H.264/HEVC/AV1. 6 GB VRAM is ample
  (pipeline uses < 100 MB VRAM).
- Consumer-driver concurrent NVENC session cap applies (the cap has been 3→5→8
  across driver history; verify current with a quick multi-open test).
  The contention test (Milestone 6: Discord screenshare + buffer) matters
  extra on laptops because Discord may also grab NVENC.
- Keep the driver at a fixed known version during a milestone; driver updates
  mid-milestone invalidate comparisons. Record driver version in every test
  log line (the spike tools should print it).

### Laptop-specific test conditions (these are now part of the matrix)
- **AC vs battery**: on battery, both GPUs downclock aggressively and Windows
  may switch power plans; run all performance-budget tests on AC, then do ONE
  battery pass to confirm graceful degradation (dropped-frame counters rise,
  watchdog warns, nothing crashes).
- **Thermals**: sustained game + encode on a thin chassis will hit thermal
  limits. This is throttling, not damage — GPUs clamp themselves. But it means
  perf numbers drift over a long session; take PresentMon measurements in the
  first 5 minutes AND after 30 minutes hot.
- **Lid close / modern standby**: laptops sleep far more often than desktops —
  the sleep/resume epoch-rebuild path (pitfall #25) will be exercised
  constantly and organically on this machine. Treat every resume as a free
  test: buffer must be alive and saving correctly after every lid cycle.
- **External display hot-plug** while buffering = topology change = epoch
  restart path. Easy to test here; hard on a desktop.

### What this machine CANNOT test (track as open matrix items)
- AMD AMF (needs any Radeon RDNA card or Ryzen APU).
- Windows 10 down-level WGC behavior (this unit ships Win11) — logic-only
  coverage via a Win10 VM is fine (see 06-SAFETY-AND-VMS.md); real WGC
  behavior needs a physical Win10 machine eventually.
- Desktop-class "display driven directly by the dGPU" topology — unless the
  SKU has the MUX switch, in which case it can.
- True exclusive-fullscreen edge cases vary per title; test with at least one
  old title that still does real FSE plus modern borderless titles.

## Standing measurement checklist (run per milestone on this machine)
1. PresentMon: 99th-pct frametime of a GPU-bound game, buffer off vs on
   (budget: < 4% impact, expect < 2%).
2. Task Manager GPU engine graphs: our process ≈ 0% on "3D", visible on
   "Video Encode" only. Screenshot into the milestone log.
3. GPUView trace (one per milestone): confirm color-convert work rides the
   video-processor engine, not 3D.
4. avrig click/flash: AV-1..AV-5 acceptance numbers from spec §5.
5. RAM: Private Bytes flat over a 2-hour buffer session at 1440p is not
   applicable (panel is 1080p/144 or 165 Hz on most V15 SKUs) — run the
   1080p60 row as primary, and the 144 Hz-source→60 fps-grid pacing test
   (plan Milestone 6) which this panel enables natively.
