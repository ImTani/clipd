# Session Handover — clipd bootstrap

> Transient onboarding note for the next session. Once its contents are folded
> into `DECISIONS.md` by the bootstrap task, delete this file.

**Written:** 2026-07-03 · previous session ran from the old OneDrive path; this
repo now lives at `X:\clipd`. Read `CLAUDE.md` and the `clipper-devpack/devpack/`
docs first — they are normative and override anything here.

---

## 1. What was done this session (all verified)

- **Relocated the project off OneDrive** → `X:\clipd`. Files copied byte-for-byte
  (CLAUDE.md SHA-256 confirmed). Old copy at
  `C:\Users\tanis\OneDrive\Desktop\Projects\clipd` is now stale — **the human still
  needs to delete it** (it's only docs, but avoids a two-copies mixup + OneDrive churn).
- **Git initialized** at `X:\clipd`, branch `main`, docs baseline commit `2ee332b`,
  plus `.gitignore` (per 07-DEVFLOW §6: `/target`, `testlogs/` except `SUMMARY.md`).
- **Relocated `CARGO_HOME`** `C:\Users\tanis\.cargo` → **`X:\cargo`** to keep the
  registry/crate-source cache off the cramped C: drive:
  - `CARGO_HOME=X:\cargo` persisted as a **User env var** (new processes inherit it).
  - User PATH entry swapped `C:\Users\tanis\.cargo\bin` → `X:\cargo\bin`.
  - Old `.cargo` deleted; ~548 MB reclaimed on C:.
  - Verified: `cargo 1.95.0` and `rustc 1.95.0` run from `X:\cargo\bin`.

## 2. Decisions made (record these in DECISIONS.md during bootstrap)

- **License = `GPL-3.0-only`.** Rationale: source is FOSS but the compiled binary
  is sold on Steam. As copyright holder you can still sell binaries; GPL copyleft
  stops a competitor shipping a closed-source paid fork (Krita is the precedent —
  GPL, sold on Steam). **Future caveat:** if you accept outside contributions, add
  a DCO or lightweight CLA so you retain relicensing/selling rights. Solo = no issue.
- **Project relocation** off OneDrive to X: (disk + sync-locking reasons).
- **`CARGO_HOME` on X:** (C: only had ~4.6 GB free).

## 3. Environment facts (this machine)

| Thing | Value |
|---|---|
| Repo root | `X:\clipd` (launch Claude Code + IDE here) |
| Rust | stable 1.95.0, `x86_64-pc-windows-msvc` |
| MSVC linker | VS Community 2022 + VC Tools x86/x64 (present) |
| Windows SDK | 10.0.26100 |
| `mftrace.exe` | `C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64\mftrace.exe` |
| `rust-lld.exe` | `C:\Users\tanis\.rustup\toolchains\stable-x86_64-pc-windows-msvc\lib\rustlib\x86_64-pc-windows-msvc\bin\rust-lld.exe` |
| Other present | `wpr.exe`, `ffprobe` 7.0.1, `git` 2.43, `winget` |
| `CARGO_HOME` | `X:\cargo` (persisted) |
| `RUSTUP_HOME` | default `C:\Users\tanis\.rustup` (~1.2 GB, left on C: — fixed size, doesn't grow) |
| Disk | C: ~5.3 GB free (still tight), X: ~78.8 GB free |

**Not yet installed** (install next session — they'll land in `X:\cargo\bin`, on PATH):
`just`, `cargo-nextest`, `cargo-deny`.

**Milestone-0/1 hardware-test tools** (install before the first spike RUNS on hardware,
not before coding): PresentMon, RenderDoc, MediaInfo, GPUView UI (ADK).

## 4. Do these next, in order

1. **Sanity-check env in the fresh session:** `echo $env:CARGO_HOME` → `X:\cargo`;
   `cargo --version` works.
2. **Install tooling:** `cargo install just cargo-nextest cargo-deny`
   (or `winget install casey.just`). Confirm each is on PATH.
3. **Add `.gitattributes`** to stop CRLF churn (git warned on the baseline commit):
   `* text=auto eol=lf` (+ `*.rs text eol=lf`). Do this before adding source.
4. **Add `.cargo/config.toml`** with the 07-DEVFLOW §1 fast-iteration linker setup:
   `rust-lld` as dev linker, `debug = 1` (line tables), and
   `[profile.dev.package."*"] opt-level = 1` so the `windows` crate isn't rebuilt slow.
5. **Bootstrap task (README §"Orchestration workflow" step 1 — the calibration task).**
   Per `CLAUDE.md` repo layout, create:
   - Cargo project skeleton (crate/binary name `clipd`).
   - `spec_constants.rs` — `PRODUCT_NAME` const + **every** constant from
     `02-AV-SYNC-SPEC.md` with a doc-comment citing its spec section. **Read that
     spec fully first — it is frozen and overrides everything.**
   - `clock.rs` — QPC↔ticks (100 ns), monotonicity guard, **exhaustive unit tests**
     incl. the spec's edge numbers.
   - `config.rs` — versioned schema v1 + `--check-config`.
   - `justfile` (recipes from 07-DEVFLOW §2), `LICENSE` (GPL-3.0), README with the
     non-goals list, `DECISIONS.md` (log the §2 decisions above).
   - GitHub Actions workflow (07-DEVFLOW §4: fmt, clippy `-D warnings`, nextest,
     cargo-deny, release build w/ binary-size print).
6. **Create the GitHub repo and push** — do it *with* this skeleton so the first push
   includes the CI workflow, then confirm Actions is green.
7. **Gate:** `just check && just test` green locally before declaring the task done.
   End the task summary with the mandatory "run X on the test machine, expect Y" block
   (for this pure-logic task: "no hardware step; CI/nextest green suffices").

## 5. Landmines (from CLAUDE.md — don't trip these)

- **`windows` crate features:** add ONLY the specific `Win32_*` gates for APIs you
  actually call, in the same commit. Blanket features are a review-rejection offense.
- **Dependency whitelist** is closed; anything new needs a DECISIONS.md line + a
  callout at the top of the task summary. No async runtime, no FFmpeg, no vendor SDKs.
- **No UI work before M7.** No scope additions — the non-goals list is the business model.
- Delete the stale OneDrive copy once you're confident in `X:\clipd`.
