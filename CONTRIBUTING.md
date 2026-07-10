# Contributing to clipd

Thanks for your interest. clipd is a small, deliberately-scoped native Windows
tool; the bar for changes is "does it make the engine more correct, more
reliable, or more honest without growing the scope." Please read this before
opening a PR — a lot of would-be contributions are ruled out by the project's
non-goals, and it's kinder to say so up front.

## Before you write code

- **Read the non-goals.** The [non-goals list in `README.md`](README.md#non-goals-load-bearing--this-list-is-the-design)
  and the REJECTED list in
  [`clipper-devpack/devpack/08-FEATURE-COMPLETE.md`](clipper-devpack/devpack/08-FEATURE-COMPLETE.md)
  are the design, not a to-do list. Overlays, editors, uploaders, accounts,
  telemetry, webcam, streaming, game-detection, and cross-platform support are
  **permanent exclusions**. A PR that adds one of these will be closed regardless
  of how well it's written.
- **The A/V sync spec is frozen.** Anything touching timestamps, pacing, drift,
  the ring buffer, or the save path must conform to
  [`clipper-devpack/devpack/02-AV-SYNC-SPEC.md`](clipper-devpack/devpack/02-AV-SYNC-SPEC.md),
  which overrides everything. Constants live in `spec_constants.rs`, not inline.
- **The dependency list is a whitelist.** See the "Dependency whitelist" section
  of [`CLAUDE.md`](CLAUDE.md). Adding a crate to the core binary needs a
  justification recorded in [`docs/DECISIONS.md`](docs/DECISIONS.md) and called
  out in the PR. Dev-dependencies are free.
- For anything non-trivial, **open an issue first** so we can agree it's in scope
  before you spend time on it.

## Prerequisites

- Windows 10 1903+ or Windows 11 (the capture/encode paths are Windows-only).
- The Rust **MSVC** toolchain — the exact stable version is pinned in
  [`rust-toolchain.toml`](rust-toolchain.toml) and installed automatically by
  rustup on first build.
- [`just`](https://github.com/casey/just) for the command surface below.
- Optional but recommended: [`cargo-nextest`](https://nexte.st/) (faster tests)
  and `cargo-deny` (license/advisory audit — CI runs it).

## The command surface

All routine actions go through `just` so humans and CI run identical commands:

```sh
just check        # cargo check + clippy -D warnings + fmt --check   (the gate)
just test         # unit tests via nextest (falls back to cargo test)
just run          # debug build + run with dev config + verbose tracing
just release      # locked, stripped release build; prints size vs the 10 MB budget
just verify FILE  # ffprobe assertion script against a saved clip
just rig          # build & run the click/flash A/V-offset measurement rig
```

**Both `just check` and `just test` must be green before a PR is ready.** CI
enforces `fmt --check`, `clippy -D warnings`, the test suite, `cargo-deny`, the
release build, and the 10 MB binary-size budget.

## Coding conventions

- Rust stable, edition 2021+. `unsafe` is confined to the COM/D3D/MF wrapper
  modules and every `unsafe` block carries a `// SAFETY:` comment. The pure-logic
  modules (ring, pacing, drift, save, config, clock) must stay 100% safe and
  unit-tested — including the spec's edge numbers.
- Errors use `thiserror` enums per module; `Result` everywhere; `unwrap`/`expect`
  only in tests and in `main()` before threads start.
- Add only the `windows` crate `Win32_*` feature gates for APIs you actually
  call, in the same commit that calls them. Blanket features are rejected.
- Log the things the trust model depends on: every save attempt and outcome,
  device changes, epoch restarts, watchdog triggers.

## Hardware-dependent changes

CI has no GPU encoder, so the capture/encode/audio paths can't be validated
there — they are verified manually on real hardware. If your change touches one
of these paths, describe **what to run and the expected numbers** in the PR (the
milestone plans under [`docs/plans/`](docs/plans/) and
[`clipper-devpack/devpack/04-TEST-MACHINE.md`](clipper-devpack/devpack/04-TEST-MACHINE.md)
show the format). Don't claim a hardware path "works" — claim it "builds and is
ready for procedure X"; the machine says whether it works.

## Commits & PRs

- Branch per change, named after the work. Keep `Cargo.lock` committed.
  Dependency and toolchain bumps are **standalone PRs**, never mixed with
  features.
- Record any non-obvious choice you made in [`docs/DECISIONS.md`](docs/DECISIONS.md)
  (newest last) and reference it in the PR — that's where scope decisions live.
- Fill in the pull-request template.

## Contributions & licensing (please read)

clipd is [GPL-3.0-only](LICENSE). The **source is free software; the compiled
binary may be sold** (this funds the project — see
[`docs/DECISIONS.md`](docs/DECISIONS.md)). To keep that arrangement clean, every
commit must be signed off under the
[Developer Certificate of Origin](https://developercertificate.org/): add a
`Signed-off-by` line with

```sh
git commit -s
```

which certifies you wrote the change (or have the right to submit it) and agree
to contribute it under the project's license. Contributions without a DCO
sign-off can't be merged.
