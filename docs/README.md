# docs/

Internal engineering and process documentation. None of this is required to
*use* clipd — start with the top-level [`README.md`](../README.md). This folder
is the working record of how the project is built, kept public for transparency.

| Path | What it is |
|---|---|
| [`DECISIONS.md`](DECISIONS.md) | Append-only log of every non-obvious choice the project made, with rationale and reversible fallback. Scope creep is meant to be visible here. |
| [`HANDOVER.md`](HANDOVER.md) | Rolling session-to-session state handover (what just shipped, what's next). |
| [`B7-CHECKLIST.md`](B7-CHECKLIST.md) | Beta-readiness checklist. |
| [`plans/`](plans/) | Per-milestone implementation plans and research notes (M2–M8, the UI redesign, and the audio-track slice). |

The **normative specifications** live one level up in
[`clipper-devpack/devpack/`](../clipper-devpack/devpack/) — in particular
[`02-AV-SYNC-SPEC.md`](../clipper-devpack/devpack/02-AV-SYNC-SPEC.md), the frozen
timestamp/sync spec that overrides everything else.
