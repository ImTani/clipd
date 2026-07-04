# Milestone 3 Plan — the ring buffer (the product)

> Draft for orchestrator review. No code written yet. Normative sources:
> `02-AV-SYNC-SPEC.md §3` (ring + eviction), `§4` (save-path rebasing — the mux
> contract), `§6.2` (byte caps), `§6.3` (watchdog thresholds), `§7` (epoch on
> device loss). All frozen — implement literally. `05-MILESTONE-TRACKER.md` M3 is
> the gate.

## 0. What M3 delivers

M1/M2 built **record-to-disk** (duration-bound: capture → encode → mux → `.mp4`).
M3 builds **replay-buffer mode** on the *same* pipeline: continuous capture into a
compressed in-memory ring, and a **global hotkey saves the last N seconds** as a
clean fMP4 in < 1 s. This is the milestone that makes clipd *clipd*.

**Exit criteria (tracker M3 — each closes only on a Nitro measurement):**
1. Compressed-packet ring with duration+byte caps, whole-GOP eviction.
2. Global hotkey save: keyframe walk-back, timestamp rebase, atomic write-then-rename, < 1 s.
3. Re-entrant/debounced saves; optional buffer clear after save.
4. ffprobe assertion script green on 50 consecutive saved clips.
5. 24-hour soak: RAM flat, no handle leaks, clip saved at hour 24 is perfect.

## 1. Architecture — where the ring sits

Today (M1/M2), packets flow encode/audio-process → **one bounded `MuxItem`
channel** → mux thread → `.mp4`. The mux thread is duration-bound and finalizes on
channel disconnect.

M3 inserts the ring **between the producers and the sink**, and introduces a second
engine lifecycle:

```text
                         ┌────────── record mode (M1/M2, unchanged) ──────────┐
capture ─▶ encode ─┐     │  MuxItem channel ─▶ mux thread ─▶ .mp4 (duration)   │
                   ├─────┤                                                     │
audio  ─▶ process ─┘     │  buffer mode (M3): MuxItem ─▶ RING ─▶ (hotkey) ─▶   │
                         │                              save.rs ─▶ .mp4 window  │
                         └─────────────────────────────────────────────────────┘
```

- The **producers do not change.** `EncodedPacket` / `EncodedAudioPacket` already
  carry everything the ring needs (`pts`, `duration`, `is_keyframe`, `epoch_id`,
  bytes). The ring stores them verbatim.
- **Buffer mode is a new engine lifecycle**, not a variant of `record`. Continuous
  capture into the ring; the ring thread owns the `VecDeque`s and applies caps on
  every push; save is *triggered* (hotkey), not duration-bound. The engine must be
  fully functional even if a save never fires.
- The mux thread is **re-used by the save path**, not by the ring: on a save, a
  fresh `Fmp4Writer` is created for that one clip, fed the selected window, and
  finalized — exactly like a tiny record session. (See §4 design decision.)

## 2. Task breakdown (branch per item, named after it)

### M3-1 `ring.rs` — the packet ring  *(pure, safe, unit-test-heavy)*
**Spec:** §3, §6.2. **Constraint:** 100% safe, no `unsafe`, no COM — logic module
per CLAUDE.md; exhaustive unit tests including the spec's edge numbers.

Structure (per §3): two `VecDeque<Packet { pts, dur, epoch_id, keyframe, bytes }>`
— one video, one audio-per-track. Dual caps, **both** enforced on every push:
- `buffer_seconds` (config `buffer.seconds`, default 120, max 600).
- `buffer_bytes` = `buffer_seconds × est_bitrate × 1.5` (§6.2 table × 1.5 headroom).

Eviction rules (verbatim §3):
- **Video: whole GOP only** — pop from the front until the front packet is an IDR.
  Never leave a partial GOP (a save needs a leading IDR).
- **Audio: pop packets with `pts < video_front_pts − 500 ms`** (the 500 ms slack
  guarantees audio always covers any surviving video range).
- `auto_qp_relief` (§6.2): if the byte cap evicts below 90% of `buffer_seconds` for
  > 60 s continuously, raise QP by 1 for the session + log. *Signal only in M3-1*
  (emit the condition); the QP bump is wired in M3-3 where the encoder is reachable.

**Tests (CLAUDE.md testing rules — the edge numbers are mandatory):**
- Duration cap: evicts oldest whole GOP when span exceeds `buffer_seconds`.
- Byte-cap pressure: byte cap trips *before* duration cap → still whole-GOP evict.
- Eviction across a GOP boundary: partial GOP never exposed at the front.
- Audio 500 ms slack: audio retained ≥ `video_front_pts − 500 ms`, evicted below.
- Empty-ring / single-GOP / IDR-at-front invariants.
- Epoch mix: ring may hold ≥ 1 epoch; eviction is epoch-agnostic (save enforces
  single-epoch, not the ring).

### M3-2 `save.rs` — the save path  *(the §4 mux contract)*
**Spec:** §4 (all of it), §0 (no clip spans epochs). **This is NOT the M2
origin-based muxer alignment** — implement the real rebase.

Given save request at master time `T_req`, clip length `L` (`buffer.seconds` or a
per-hotkey length later):
1. `target = T_req − L`.
2. `origin` = PTS of the newest video IDR with `pts <= target`, **same epoch as the
   newest packet**. If the epoch boundary is newer than `target`, `origin` = first
   IDR of the current epoch (clip shorter than requested — log + toast). Worst-case
   pre-roll slack = one GOP = 2 s.
3. Video: all packets with `pts >= origin`. Output PTS = `pts − origin`.
4. Audio (per track): first packet = first with `pts >= origin` (≤ 21.33 ms head
   silence accepted). Output PTS = `pts − origin`. Include trailing packets until
   `pts >= last_video_pts + D`.
5. Container numbers (fMP4): movie timescale 1000; video timescale `fps×1000`,
   sample_delta 1000; audio timescale 48000, sample_delta 1024; tick→timescale via
   round-half-even.
6. Fragment: one moof/mdat per 1 s.
7. Atomicity: write `name.mp4.part`, `FlushFileBuffers`, rename to `name.mp4`.

**Snapshot discipline:** the save must operate on a *consistent* view of the ring
while capture keeps running. Plan: under the ring lock, clone (Arc-share) the
selected packet window into a save job, release the lock, then mux off-lock. Ring
packets are `Arc<[u8]>`-backed (see §5 decision) so the snapshot is cheap.

**Tests:**
- IDR walk-back: `target` mid-GOP → `origin` = preceding IDR; rebased PTS start at 0.
- Rebase across a GOP boundary (CLAUDE.md-named edge).
- Epoch boundary newer than target → clip clamps to current-epoch first IDR + logs.
- Trailing-audio inclusion stops at `last_video_pts + D`, not before/after.
- Round-half-even conversion at tick values that land exactly on .5 of a tick.
- Head-silence ≤ 21.33 ms; audio track duration within 1 AAC frame of video.
- Atomic write: `.part` exists mid-write, `.mp4` only after flush+rename.

### M3-3 hotkey + engine wiring  *(the buffer-mode lifecycle)*
**Spec:** §6.3 (save-duration WARN > 1000 ms), §3/§6.2 (`auto_qp_relief` wire-up).
**Deps:** `global-hotkey` (whitelisted) — `RegisterHotKey` only, no low-level hooks
(CLAUDE.md hard-constraint 5). Add in this commit; note in DECISIONS.md.

- New engine entry (e.g. `run_buffer` in `main.rs` + a `BufferEngine` in
  `engine.rs`, or a mode flag on the existing engine): continuous capture into the
  ring; no duration.
- `global-hotkey` registers `hotkeys.save_clip`; on press → enqueue a save job for
  `save.rs`. Save runs on its own thread (never block capture/encode).
- **Re-entrancy / debounce:** a save in flight blocks a second from starting (or
  debounces within a short window); log the dropped/coalesced press. Define the
  window (suggest 250 ms, matching the §7 debounce idiom) in `spec_constants.rs`.
- **`clear_after_save`** (config): optionally drop the ring after a successful save.
- **`auto_qp_relief`:** consume the M3-1 signal → bump encoder QP by 1 for the
  session, log. (Encoder QP is runtime-settable via the MFT — confirm during impl;
  if not, document the deferral.)
- Save-duration watchdog: time each save, WARN > 1000 ms (§6.3, "disk suspect").

**Note:** the tray icon (`tray-icon`) is an **M5** deliverable — M3 triggers save
by hotkey only. Do **not** add `tray-icon` in M3 (scope ratchet). A headless
`buffer` subcommand that prints save events to the log is the M3 surface.

### M3-4 `just verify` — the ffprobe assertion script  *(build FIRST)*
**Spec:** §4.5 (container numbers), §5 (durations), CLAUDE.md testing rules.
Currently a stub in the justfile. Build this **before** M3-2/M3-3 land so every
saved clip is machine-checked from day one — it's the companion to `tools/avrig`.

Assertions on a saved `.mp4`:
- Track durations within 1 AAC frame (21.33 ms) of each other.
- Monotonic PTS on every track (no `ts_violation`).
- Video CFR: frame deltas exactly constant (sample_delta 1000 @ `fps×1000`).
- Fragment validity: each moof/mdat parses; file plays start-to-finish.
- Stream shape: 1 h264 + N aac-LC 48 kHz (reuse the M2 ffprobe idioms; ffmpeg 7
  uses `pts_time`, not `pkt_pts_time` — carried gotcha).

Deliver as a script (PowerShell or Rust bin under `/tools`, matching avrig) plus a
`just verify <clip>` recipe. Green on **50 consecutive** saves is exit criterion 4.

### M3-5 24-hour soak  *(the incumbent failure mode)*
**Spec:** §6.4 (RAM budget), tracker M3-5. Hardware/manual — agent writes the
harness + checklist, orchestrator runs it on the Nitro.
- Run `buffer` mode 24 h; sample RSS + GPU/handle counts periodically.
- Assert: RAM flat (ring is bounded; no growth beyond `buffer_bytes` + §6.4
  30–60 MB overhead, 75 MB budget); no handle leak; save at hour 24 is ffprobe-clean.
- This is *the* test the whole project exists to pass — a dead thread under a live
  process, a slow leak, an unbounded queue all surface here.

## 3. Test matrix (maps to exit criteria)

| Exit criterion | Covered by | Kind |
|---|---|---|
| 1. Ring caps + whole-GOP eviction | M3-1 unit tests (edge numbers) | `just test`, CI-green |
| 2. Hotkey save, rebase, atomic, < 1 s | M3-2 unit tests + M3-3 timing + `just verify` | mixed |
| 3. Re-entrant/debounced, clear-after-save | M3-3 unit tests + manual hotkey mashing | mixed |
| 4. ffprobe green ×50 | M3-4 script over 50 saves | Nitro |
| 5. 24-h soak | M3-5 harness | Nitro |

## 4. Key design decision — reuse `Fmp4Writer` for saves?

**Recommendation: reuse it, feeding it the §4-selected window; do the §4 *selection
and origin choice* in `save.rs`, not in the muxer.**

Rationale: `Fmp4Writer::write_video_packet` sets `origin = first video packet's
PTS` and offsets everything by it. In the save path, `save.rs` walks back to the
chosen IDR and feeds packets **from that IDR onward** — so the muxer's "first packet
PTS" *is* the §4 `origin`, and its offset-by-origin *is* §4.3/§4.4's `pts − origin`.
The muxer's existing audio prebuffer/`initial_offset` logic already yields the
≤ 21.33 ms head-silence §4.4 accepts. What the muxer does **not** own — and what
`save.rs` must — is: choosing `origin` (IDR walk-back, §4.2 epoch clamp), selecting
the video window, and selecting trailing audio up to `last_video_pts + D` (§4.4).

**Risk to verify during M3-2:** confirm the muxer's round-half-even and timescale
math (§4.5) is already exact for the save path, and that feeding a window starting
at an arbitrary IDR (not the epoch's first frame) produces PTS starting at 0. If any
mismatch surfaces, the fix is a thin save-specific entry on the muxer, **not** a
second muxer. Flag the outcome in DECISIONS.md either way.

*This decision is reversible and stays out of the hot path — chosen per CLAUDE.md
ambiguity rule 3 (simpler, more logged, reversible). Recorded here for review.*

## 5. Design decisions (resolved against the devpack)

1. **Packet byte ownership → `Arc<[u8]>` (resolved by the RAM budget).** The ring
   holds packets long-term and the save must mux **off-lock** (pitfall 24) so
   capture/eviction never stalls. If the save *clones* its window to do that, the
   transient allocation is the window size — ~246 MB at the 120 s/1080p default,
   **~1.9 GB at the 300 s/4K row in §6.2** — which blows the "ring size + < 75 MB
   overhead" budget (CLAUDE.md rule 7, "budgets are requirements"). Backing packet
   bytes with `Arc<[u8]>` makes the snapshot a pointer-clone: peak RAM stays at ring
   size. `01-PROJECT-PLAN §2` also describes save as "**slice, mux**" — a view, not
   a copy. Touches `EncodedPacket`/`EncodedAudioPacket` (`Vec<u8>` → `Arc<[u8]>`),
   std-only, reversible. **Decided: `Arc<[u8]>`.**
2. **Buffer-mode engine shape → ring is the spine; reuse the spawn helpers
   (resolved by the architecture).** `01-PROJECT-PLAN §2` lists the ring/buffer-mux
   as one of the **four permanent threads** ("ring buffer of packets; on save: slice
   from keyframe, mux"), and M4 is "'record next N minutes' **sharing the same
   pipeline** with a disk sink." So the ring is the architectural spine, not a mode,
   and the M1/M2 duration-bound `RecordingEngine` is transitional (ring-less)
   scaffolding. **Decided:** M3 builds the buffer-mode lifecycle with the ring as the
   sink, **reusing the existing capture/encode/audio `spawn` helpers** — not a second
   divorced pipeline, and not a flag on the duration-bound type. M4 converges
   timed-record onto the same ring spine. Reversible.
3. **`est_bitrate` source for the byte cap.** §6.2 table is keyed by resolution+fps.
   Derive from `(width, height, fps)` at buffer start via a small table in
   `spec_constants.rs` (cites §6.2), not a magic number. No config surface needed.
4. **Save length.** M3 uses `buffer.seconds` as `L`. Multi-length clip hotkeys are
   **M10** (scope ratchet) — not in M3.

All four are grounded in the frozen spec / plan or fall under CLAUDE.md ambiguity
rule 3 (simpler, more logged, reversible). #1 and #2 were the two flagged for ack;
the devpack resolves both, so I'll proceed and record them in DECISIONS.md when the
respective tasks land. None are irreversible or cross a hard constraint.

## 6. Suggested sequencing

1. **M3-4 `just verify`** first — so every later save is checked. (Small, no HW.)
2. **M3-1 `ring.rs`** — pure + fully unit-tested; the foundation. (No HW.)
3. **M3-2 `save.rs`** — §4 rebase; unit-tested; verified against M3-4. (No HW to
   build; ffprobe-checked on Nitro.)
4. **M3-3 hotkey + buffer engine** — wires 1–3 into a live `buffer` mode. (Nitro:
   press hotkey, clip saves, `just verify` green.)
5. **M3-5 soak** — last; needs the whole thing running 24 h. (Nitro.)

Steps 1–2 are CI-green-only ("no hardware step; CI green suffices"); 3–5 end with a
"run X on the Nitro, expect Y" block per CLAUDE.md task hygiene.

## 7. Deps & scope notes

- **Add `global-hotkey`** (whitelisted) in M3-3 — `RegisterHotKey` only. DECISIONS.md line.
- **Do NOT add `tray-icon`** — that's M5. M3's surface is a headless `buffer` subcommand + logs.
- No new non-whitelisted deps anticipated. Ring/save/verify use std + existing crates.
- Every §-derived constant (500 ms audio slack, 21.33 ms head-silence, debounce
  window, byte-cap ×1.5) lives in `spec_constants.rs` with a spec citation — no
  inline magic numbers (CLAUDE.md).
