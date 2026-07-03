# Session Handover — next up: Milestone 0 spikes

> Onboarding note for the next session. `CLAUDE.md` and the
> `clipper-devpack/devpack/` docs are normative and override anything here.
> `02-AV-SYNC-SPEC.md` (frozen) overrides everything.

**Written:** 2026-07-03, after the repo-bootstrap / calibration task shipped.

---

## 1. Where things stand

- **Bootstrap is done and green.** Public repo: https://github.com/ImTani/clipd
  (`main`). CI passes on `windows-latest` (fmt, clippy `-D warnings`, nextest,
  cargo-deny, release build + 10 MB size check, artifact upload).
- Orchestration workflow **step 1 (calibration task) is complete**; the next unit
  of work is **Milestone 0 — spikes** (see §4).

### What exists in the repo now
- `src/spec_constants.rs` — `PRODUCT_NAME` + every `02-AV-SYNC-SPEC.md` constant,
  each doc-commented with its §citation. Reference these; no inline magic numbers.
- `src/clock.rs` — QPC↔ticks (`i128` math) + `MonotonicGuard`; `unsafe` confined
  to the two QPF/QPC FFI reads. Exhaustively unit-tested.
- `src/config.rs` — versioned TOML schema v1 + validation + `--check-config`.
- `src/{lib,main}.rs` — lib + thin binary shell (engine not wired yet).
- Tooling: `justfile` (PowerShell recipes), `.cargo/config.toml` (rust-lld dev
  linker), `rust-toolchain.toml` (pins 1.95.0), `deny.toml`, GH Actions CI,
  `README`, `LICENSE` (GPL-3.0), `DECISIONS.md`.
- 32 unit tests. `just check` + `just test` green locally; release exe 0.45 MB.

## 2. Environment facts (this machine)

| Thing | Value |
|---|---|
| Repo root | `X:\Projects_X\clipd` (launch Claude Code + IDE here) |
| Rust | stable **1.95.0**, `x86_64-pc-windows-msvc` (pinned in rust-toolchain.toml) |
| `CARGO_HOME` | `X:\cargo` (persisted User env var; `X:\cargo\bin` on User PATH) |
| `RUSTUP_HOME` | default `C:\Users\tanis\.rustup` (left on C:) |
| Dev tools installed | `just`, `cargo-nextest`, `cargo-deny` (in `X:\cargo\bin`) |
| MSVC / SDK | VS Community 2022 VC Tools; Windows SDK 10.0.26100 |
| `mftrace.exe` | `C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64\mftrace.exe` |
| Other present | `ffprobe` 7.0.1, `git`, `gh` (authed as `ImTani`), `winget` |
| Git remote | `origin` = **HTTPS** (`https://github.com/ImTani/clipd.git`), token has `repo` + `workflow` scope |

**M0 hardware tools to install before the first spike RUNS on hardware**
(not needed to write it): PresentMon, RenderDoc, MediaInfo, GPUView (Windows ADK).

## 3. Gotchas (learned this session — don't retrip)

- **Stale launch environment.** If a fresh shell reports `cargo: not found` or an
  empty `CARGO_HOME`, the terminal/Claude Code was launched from a process with a
  stale env (old `C:\Users\tanis\.cargo\bin` on PATH). The *persisted* User env is
  correct — **relaunch the terminal** to fix. Stopgap for one session: prefix
  commands with `$env:CARGO_HOME='X:\cargo'; $env:PATH="X:\cargo\bin;$env:PATH"`.
- **git push uses HTTPS + the gh token, not SSH.** SSH keys aren't loaded here;
  `origin` was switched to HTTPS and `gh auth setup-git` configured the
  credential helper. Pushing workflow files needs the token's `workflow` scope
  (already granted). Don't switch `origin` back to SSH unless a key is loaded.
- **`just` runs recipes under PowerShell** (`set windows-shell`), because `sh`
  (just's default) isn't on PATH on a typical Windows box. CI does NOT use just —
  it calls cargo directly.
- **CI runner has no `bc`.** Use `awk` for any float formatting in CI bash steps.

## 4. Do this next: Milestone 0 spikes (00-README step 2; 01-PLAN §5; tracker M0)

Spikes are **throwaway** code kept under `/spikes`, **never linked** into the
crate. Each spike = one task; the agent writes the spike + a checklist of expected
output; the human runs it on the Nitro V15 and pastes results back. Order by risk:

1. **MF async hardware H.264 encoder** (highest risk — 01-PLAN §5.1 / pitfall 17):
   synthetic NV12 frames → playable `.h264`/`.mp4`. This is the "two weeks of
   pain" component; prove the METransformNeedInput/HaveOutput state machine and
   D3D device-manager plumbing in isolation first. `just trace` (MFTrace) will
   help; `mftrace.exe` path is in §2.
2. **WGC capture** primary monitor: count fps, verify texture format on SDR + HDR.
3. **WASAPI loopback + mic** → WAV; inspect timestamps during silence + unplug.
4. **Decision:** MF Sink Writer vs hand-rolled fMP4 (01-PLAN §5.2) → record in
   DECISIONS.md.

**Gate rule (00-README §4):** a milestone item closes only on a measurement from
the Nitro, never on an agent claim. Do not open M1 until M0 is green on hardware.

While spiking, this is where the whitelisted **`tracing-subscriber`** finally
gets added to `Cargo.toml` (it was authorized this session but not yet pulled in
— YAGNI): wire a subscriber the first time a spike needs to see log output.

## 5. Landmines (from CLAUDE.md — still binding)

- **`windows` crate features:** add ONLY the specific `Win32_*` gates for APIs you
  actually call, in the same commit. Blanket features are a review-rejection
  offense. (Current gate: `Win32_System_Performance` for QPC.)
- **Dependency whitelist is closed.** It now includes `tracing-subscriber`
  (added this session). Anything else needs a DECISIONS.md line + a task-summary
  callout. No async runtime, no FFmpeg, no vendor SDKs.
- **No scope additions** (non-goals list = the business model). **No UI before
  M7.** YAGNI (CLAUDE.md rule 8): prefer existing libraries, within the whitelist.
- **Branch per tracker item**, named after it (e.g. `m0-mf-encoder-spike`); task
  summary ends with the "run X on the test machine, expect Y" block.

## 6. Pending human TODOs

- Relaunch the terminal at some point (clears the stale env — see §3).
- Install the M0 hardware tools (§2) before running the first spike on the Nitro.
- (Done: OneDrive stale copy deleted; `tracing-subscriber` whitelisted.)
