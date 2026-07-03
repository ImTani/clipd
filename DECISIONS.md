# DECISIONS

Append-only log of choices the coding agent made, per `CLAUDE.md` "How to handle
ambiguity". Newest last. Each entry: what, why, and (where relevant) the
reversible fallback. Scope creep is meant to be visible here.

---

## 2026-07-03 — Bootstrap / calibration task

Decisions carried over from the previous session's `HANDOVER.md` §2, now recorded
here so the handover file can be deleted:

- **License = `GPL-3.0-only`.** The source is FOSS but the compiled binary is
  sold (e.g. on Steam). As sole copyright holder you can still sell binaries;
  GPL copyleft stops a competitor shipping a closed-source paid fork (Krita is
  the precedent — GPL, sold on Steam). **Caveat:** if outside contributions are
  ever accepted, add a DCO or lightweight CLA to retain relicensing/selling
  rights. Solo development = no issue. `LICENSE` is the verbatim GPLv3 text from
  gnu.org.
- **Project relocated off OneDrive** to `X:\clipd` (disk pressure on C: +
  avoiding OneDrive sync locking the build directory).
- **`CARGO_HOME` moved to `X:\cargo`** (C: had ~4.6 GB free); persisted as a User
  env var with `X:\cargo\bin` on the User PATH.

### Bootstrap structural decisions (this task)

- **Crate is split library + binary** (`src/lib.rs` + `src/main.rs`, both named
  `clipd`). Rationale: the pure-logic modules (clock, config, spec_constants)
  must be unit-testable in isolation (`CLAUDE.md` testing rules); a lib target
  makes that clean and lets future integration tests under `tests/` import them.
  The binary is a thin shell. Reversible.

- **`clock.rs` reads QPC via the `windows` crate** with exactly the
  `Win32_System_Performance` feature gate (added in the same commit that calls
  `QueryPerformanceCounter`/`QueryPerformanceFrequency`, per the no-blanket-
  features rule). The conversion math and the monotonicity guard are pure/safe
  and exhaustively unit-tested; `unsafe` is confined to the two FFI reads, each
  with a `// SAFETY:` comment. `clock` is not on `CLAUDE.md`'s no-unsafe list
  (ring/pacing/drift/save/config), so the syscall boundary living here is
  consistent with the conventions.

- **Profiles live in `Cargo.toml`, linker in `.cargo/config.toml`.**
  07-DEVFLOW §1 phrases the fast-iteration setup as all in `.cargo/config.toml`,
  but cargo does not read `[profile.*]` from there. So `debug = 1` and
  `[profile.dev.package."*"] opt-level = 1` are in `Cargo.toml`; the dev linker
  (`rust-lld.exe`) is in `.cargo/config.toml`. Verified a debug build links with
  rust-lld. If rust-lld ever breaks on a machine, delete the `.cargo/config.toml`
  `linker` line to fall back to the default MSVC linker (correctness unaffected).

- **`release` profile does NOT set `panic = "abort"`.** `CLAUDE.md` requires
  worker-thread panics to be caught at the thread boundary (`catch_unwind`) and
  routed to the watchdog; that needs unwinding. Size budget is met via
  `lto`/`codegen-units = 1`/`strip` instead.

- **`rust-toolchain.toml` pins `1.95.0`** (07-DEVFLOW §6). Toolchain bumps are
  standalone PRs.

- **Config schema v1 tolerates unknown keys on read but does not yet preserve
  them on rewrite.** There is no config-rewrite path in v1 (nothing writes
  config to disk), so `--check-config` is read-validate-print only. Full
  unknown-key *preservation* on rewrite (01-PROJECT-PLAN §3 pitfall 30) is a
  Milestone-5 deliverable and will likely need `toml_edit` (not on the current
  dependency whitelist — a whitelist addition to raise then). Flagged, not
  silently adopted.

- **`justfile` stubs `rig`/`verify`/`spike`/`trace`.** Their deliverables
  (measurement rig, ffprobe assertion script, spikes, MFTrace wiring) arrive in
  Milestones 0–3. The recipes exist now so the command surface is stable; each
  stub prints where its deliverable will land.

### Open items for the orchestrator (do not need a decision now)

- **`tracing-subscriber` is a whitelist gap.** The dependency whitelist includes
  `tracing` and `tracing-appender` but not `tracing-subscriber`, which is
  required to actually install a subscriber and render logs to the rotating file
  (01-PROJECT-PLAN §2 logging row). No subscriber is wired in this task (pure-
  logic tests assert counters directly, not log output), so nothing is added
  yet. Recommend adding `tracing-subscriber` to the whitelist when logging is
  first wired (Milestone 0 spike or Milestone 5). Raised here rather than
  silently pulled in.
