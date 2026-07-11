# Dev Pack — Lightweight FOSS Replay-Buffer Clipper (Rust / Windows)

Compiled 2026-07-03 (rev 3: +name/GTM doc; +devflow, +feature-complete scope, briefing extended with devflow/UI/naming rules, tracker extended M7-M10). Human orchestrator + Claude Opus 4.8 as coding agent.
Primary test hardware: Acer Nitro V15 (RTX 4050 Laptop + Intel iGPU).

## Contents & authority order
| File | Role | Authority |
|---|---|---|
| 01-PROJECT-PLAN.md | Product definition, architecture, 31-pitfall catalogue, dev environment, milestones | Normative, 2nd |
| 02-AV-SYNC-SPEC.md | Frozen timestamp/sync spec: clocks, pacing, drift control, rebasing, tuning tables, acceptance tests | **Normative, 1st — overrides everything** |
| 03-AGENT-BRIEFING.md | Instructions for the coding agent; copy into repo root as CLAUDE.md | Normative, 3rd |
| 04-TEST-MACHINE.md | Nitro V15 topology, NVENC/QSV coverage, laptop-specific test conditions, standing measurement checklist | Reference |
| 05-MILESTONE-TRACKER.md | The full M0–M6 checklist as a standalone trackable file | Working doc |
| 06-SAFETY-AND-VMS.md | System-corruption risk analysis (none) and VM policy (one Win10 logic VM, otherwise bare metal) | Reference |
| 07-DEVFLOW.md | Solo-dev workflow: inner loop, justfile command surface, task loop, CI, debugging bindings | Normative for process |
| 08-FEATURE-COMPLETE.md | v1.0 scope: UI (egui satellite), per-app audio, AV1/HDR, QoL/release engineering, rejected list | Normative, scope ratchet |
| 09-NAME-AND-GTM.md | ORCHESTRATOR-ONLY: name collision report + claim checklist, creator/forum/press map, trend positioning, launch sequence | Reference (kept local/private — not in the public tree) |

## Orchestration workflow (suggested)
1. **Repo bootstrap task**: hand the agent 03 (as CLAUDE.md), 01, 02; ask for
   repo skeleton + `clock.rs` + `spec_constants.rs` with unit tests. Low-risk
   calibration task — judge the agent's spec adherence here before trusting it
   with the MFT.
2. **Milestone 0 spikes next** (plan §5 item 1 is the highest-risk component:
   the async MF encoder). Each spike is one task; you run the produced tool on
   the Nitro and paste its output back as the task result.
3. Work the tracker top-to-bottom. One checklist item ≈ one agent task. The
   agent must end every task with "run X on the test machine, expect Y"
   (per briefing); your role is executing those and feeding numbers back.
4. **Gate rule**: do not open a milestone until the previous one's checklist
   is fully green ON THE MACHINE. Agent claims don't close items; measurements do.
5. Keep DECISIONS.md (agent-maintained, per briefing) under review — it is
   where scope creep will try to enter.
6. MVP = M0-M6 (engine proven). Feature-Complete/1.0 = M7-M10 per
   08-FEATURE-COMPLETE.md. The rejected list at the end of 08 is a ratchet:
   only an explicit orchestrator instruction reopens an item on it.

## The three sentences to re-read when tempted to deviate
- The non-goals list is the business model, not a v1 shortcut.
- All sync error was deliberately pushed into the audio drift loop; if sync is
  wrong, the spec's §5 table names the single suspect — don't shotgun-debug.
- Quality bends before buffer duration does; silence is synthesized, never
  skipped; a save may fail loudly but never silently.
