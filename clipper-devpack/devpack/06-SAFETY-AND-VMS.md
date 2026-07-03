# System Safety & VM Policy

## Can this project corrupt the system or drivers? — No, by construction.

The software is a pure user-mode process consuming documented public Windows
APIs (WGC, D3D11, Media Foundation, WASAPI, RegisterHotKey). It contains:
no kernel driver, no process injection, no API hooking, no firmware access,
no driver-file modification, no registry writes beyond an optional HKCU Run
key. User-mode code on modern Windows cannot persistently corrupt drivers or
the OS through these APIs — the entire kernel/user boundary exists to
guarantee that. There is no realistic pathway from a bug in this codebase to
"my driver installation / Windows install is damaged."

## What CAN happen during development (worst realistic outcomes)

| Event | What it looks like | Persistence | Mitigation |
|---|---|---|---|
| App crash / hang | Process dies or a thread wedges | None — restart the exe | Watchdog + thread panic routing (briefing) |
| GPU TDR (driver timeout reset) | 2–3 s black/flicker, "display driver recovered" toast; our D3D device is lost | None — driver reloads itself; reboot never required for damage, occasionally for tidiness | This is exactly the DEVICE_REMOVED epoch-rebuild path; treat every TDR during dev as a free test of it |
| Vendor driver bug → bluescreen | BSOD, machine reboots | None — no corruption; drivers are stateless across boot | Rare with these APIs; if reproducible, it's the vendor's bug — capture the minidump, pin the driver version, work around |
| Disk fill | Clips/logs eat the SSD | Annoying, not damaging | Implement the free-space check early (plan pitfall #24); dev config points output at a dedicated folder |
| Runaway RAM (ring bug pre-caps) | System pressure, paging, sluggish | None after kill | Byte caps are an early-milestone deliverable, not a polish item |
| Sustained thermals (laptop) | Fans, throttling during game+encode soak tests | None — GPUs self-clamp; throttling is protection working | Run soaks on AC, on a hard surface; expected and harmless |

Notes for the orchestrator's peace of mind:
- Even the historical horror stories about "programs killing GPUs" involve
  firmware flashing or voltage tools — categories this project never touches.
- The MF/WGC/WASAPI stack is the same one PowerPoint recording, Xbox Game Bar,
  and Teams run on daily; drivers are hardened against user-mode misuse of it.
- The riskiest thing on the machine during this project is a driver UPDATE
  changing behavior mid-milestone, not the software damaging the driver.

Sensible hygiene anyway (cheap, not fear-driven): keep the repo on git with
remotes (obviously), export the working config before experiments, know where
minidumps land (%LOCALAPPDATA%\CrashDumps + C:\Windows\Minidump), and create
one Windows restore point before the first driver-version pin — for
convenience of rollback, not because corruption is expected.

## Are VMs necessary? — No. Mostly they're useless here.

Two separate questions:

1. **VMs for safety?** Not needed — per the table above there is no
   system-corruption risk to isolate.
2. **VMs for functionality?** Standard VMs (Hyper-V, VirtualBox, VMware
   without GPU passthrough) expose a virtual display adapter: no NVENC, no
   QSV, no real WGC/D3D11 hardware path, no meaningful WASAPI device churn.
   The entire interesting 80% of this project cannot run in one. GPU
   passthrough (Hyper-V GPU-P / DDA) exists but is fiddly, laptop-hostile
   (Optimus complicates passthrough), and would test a topology no real user
   has.

Where a VM IS worth having (one case): a **Windows 10 22H2 VM** for
logic-level down-level coverage — config handling, API-availability probing
(does the code correctly detect missing IsBorderRequired support and degrade),
installer/first-run behavior. That's CPU-only code and virtualizes fine.
The real Win10 GPU behavior still needs a physical Win10 machine eventually
(Milestone 6 matrix item), borrowed or cheap.

**Policy: develop and test bare-metal on the Nitro V15. One Win10 VM for
logic coverage. No passthrough heroics.**
