<!--
Thanks for contributing! Please read CONTRIBUTING.md first — especially the
non-goals and the dependency whitelist. Non-trivial changes should have an issue
agreeing they're in scope before the PR.
-->

## What this changes

<!-- One or two sentences. Link the issue it closes, if any. -->

## Why

<!-- The problem being solved. -->

## Checklist

- [ ] `just check` is green (fmt + clippy -D warnings).
- [ ] `just test` is green.
- [ ] `Cargo.lock` is committed if dependencies changed (dep/toolchain bumps are a **separate** PR).
- [ ] No new core-binary dependency — or, if there is one, it's justified in `docs/DECISIONS.md` and called out below.
- [ ] Any non-obvious decision is recorded in `docs/DECISIONS.md` (newest last).
- [ ] This change respects the non-goals and the frozen A/V sync spec.
- [ ] Commits are DCO signed-off (`git commit -s`).

## Hardware verification

<!--
CI has no GPU encoder. If this touches capture / encode / audio / the save path,
say what to run on real hardware and the expected numbers. If it's pure-logic,
write: "no hardware step; CI green suffices."
-->

## Decisions / scope notes

<!-- Anything the reviewer should know that isn't obvious from the diff. -->
