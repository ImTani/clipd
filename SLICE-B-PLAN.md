# Slice B Plan — "M8′: four-track audio" (engine work + real HW cycle)

**Written:** 2026-07-08, after a full read of the code and specs (no code changed).
This is the working plan for Slice B (B1–B7 + B3.5). It refines the task sketch in
**`M7-M8-PLAN.md` §4** with the concrete change surface, the source/track
architecture, per-task test/HW plans, and the decisions the orchestrator must
settle before coding. Slice A (A1–A8) is complete, local-green, HW-validated, and
`main` is pushed to `origin`.

> **Normativity.** `CLAUDE.md` + the devpack (`clipper-devpack/devpack/`) are
> normative. `02-AV-SYNC-SPEC.md` is frozen and overrides everything EXCEPT the
> dated `DECISIONS.md` amendments — the three **2026-07-07** entries most relevant
> here are **§2.2** (process-loopback `QPCPosition` is the master domain,
> pass-through), **§2.5** (mixed-first + optional per-app track layout), and **§4**
> (OBS-Hybrid `moov` finalize on save). `M7-M8-PLAN.md §0` records the orchestrator
> decisions this plan implements. Do not re-derive the research facts — they are in
> `M7-M8-PLAN.md §5` and `HANDOVER.md §4`.

---

## 0. What Slice B delivers (end-state)

The container grows from **2 tracks (desktop, mic)** to the fixed **5-track
topology** of `M7-M8-PLAN.md §2`, plus the two owed follow-ons:

| # | Track (`AudioTrackKind`) | Fed by | Present when |
|---|--------------------------|--------|--------------|
| 1 | **Mix** (always first) | sum(default-endpoint loopback + mic), −3 dB, soft-clip | always |
| 2 | Game | process-loopback **include-tree**(game PID) | game bound (§3 rules) |
| 3 | Voice chat | process-loopback **include-tree**(VC PID) | a `vc_apps` process is running |
| 4 | Other system | process-loopback **exclude-tree**(game PID) when a game is bound; **else** the plain default-endpoint loopback | `separate_tracks` on |
| 5 | Mic | existing WASAPI capture | mic ≠ `off` |

- **Default output (`separate_tracks = false`, the new default) = tracks 1 + 5 (mix
  + mic).** Full 5-track set only when `separate_tracks = true`. This is a locked
  *semantics change + default flip* to `separate_tracks` — see Decision **D1**
  (default was `true`/{desktop,mic} through Slice A).
- Container finalized with the **OBS-Hybrid appended `moov`** on save (B5) so
  editors/Explorer read it cleanly; fragments still written first (crash-safety of
  §4.6 preserved).
- Uploads/CapCut/browsers keep exactly **track 1 (mix)** — the reason mix MUST be
  first.
- Also lands: **B3.5** the enumerated mic-device dropdown (the last owed Slice-A
  fast-follow) and **B6** the honesty docs (`LIMITATIONS.md`).

---

## 1. The one architectural decision everything hangs on: **sources ≠ tracks**

Today the pipeline conflates the two: `AudioStreamKind` (Desktop, Mic) is *both* the
capture source *and* the container track, 1:1. **Slice B breaks that 1:1** and the
plan must model it explicitly:

- **Mix (track 1) is derived, not captured** — it is the software *sum* of two
  other sources.
- **Other-system (track 4)'s source switches at runtime** — endpoint loopback with
  no game bound, process-exclude-tree once a game binds, with a logged silence gap
  at the switch.
- **Game/VC tracks are conditional** on runtime detection (a game becoming
  foreground-fullscreen; a `vc_apps` process appearing) and can start/stop mid-session.
- **The default-endpoint loopback and the mic each feed TWO consumers**: the mix
  (summed) and a standalone track (other/mic).

So Slice B introduces a clean split (names illustrative, settle in B1):

```
CaptureSource (a WASAPI producer)          AudioTrackKind (a container track / meter)
  EndpointLoopback (default render)          Mix          ← sum(EndpointLoopback, Mic)
  MicEndpoint (default-follow | pinned)      Game         ← ProcessLoopback{game, include}
  ProcessLoopback { pid, include_tree }      VoiceChat    ← ProcessLoopback{vc, include}
                                             OtherSystem  ← ProcessLoopback{game, exclude} | EndpointLoopback
                                             Mic          ← MicEndpoint
```

**Why this is the honest model:** it is the only shape that expresses "one source →
two tracks" (mix + standalone) without double-opening WASAPI clients, and "one track
← two sources" (mix) and "one track ← a source that changes" (other-system). The
existing per-stream DSP chain (`StreamResampler` = gaps+drift+resample, then
`AacEncoder`) stays **per track**; a source fans its resampled 48 kHz PCM out to the
track encoders that consume it (the mix encoder sums two; every other track consumes
one). Levels/meters and status are **per track** (5 meters), matching the UI.

> **YAGNI guard.** Do not build a general routing matrix. There are exactly five
> tracks and at most five sources, wired by a small explicit builder from the config
> + the live game/VC binding. `separate_tracks = false` collapses to 2 tracks and 2
> sources (endpoint + mic) — the mixer is the only non-trivial piece in the default
> path.

---

## 2. Change surface (grounded in the current code)

Two independent traces confirm the pipeline is **already generalized over
`num_audio` / positional `track_index`** — the ring, save, mux, epoch loop, and
per-stream thread spawn need **no structural change** to carry N tracks. The
"exactly two" knowledge is narrow:

### Already N-generic (no structural change)
- `ring.rs` — `audio: Vec<VecDeque<..>>` sized by `RingCaps.num_audio_tracks`;
  push/evict/clear loop all tracks (`ring.rs:51,82,197`).
- `save.rs` — selects/rebases `0..ring.num_audio_tracks()`; `clip_end = min` over
  all tracks; feeds mux positionally (`save.rs:136-166,226-234`).
- `mux/fmp4.rs` — `audio: Vec<AudioTrack>`; `write_audio_packet(track_index, …)`;
  `build_moov(&audio)` writes tracks in the order registered (`fmp4.rs:116,228,192`).
- `engine.rs` — the producer spawn is a `for (track_index, (kind, selection)) in
  audio_streams.iter().enumerate()` loop; `audio_process_thread` routes by
  `track_index` via `MuxItem::Audio(track_index, au)`; `mux_worker_thread`,
  `asc_slots`, `ingest_audio`, and the epoch loop are all `num_audio`-parametric;
  the single `Arc<AudioLevels>` survives epoch rebuilds
  (`engine.rs:1290-1303,552-654,1732,1000,1056-1109`).
- `audio/levels.rs` — `slots: [StreamLevel; AudioStreamKind::COUNT]` self-sizes;
  `index()`'s exhaustive match forces a new arm (`levels.rs:84,92`).
- `AacEncoder::new(kind, …)` — the `kind` field is **cosmetic** (48 kHz/stereo/CBR
  AAC is source-independent; `configure()` uses only bitrate). New kinds cost it
  nothing (`mft_aac.rs:110`).

### The narrow "knows there are two" edit set
| Site | File:line | Change |
|---|---|---|
| The enum | `audio/wasapi_stream.rs:60-96` | 2 → 5 track kinds; `COUNT`, `index()`, `label()`, `title()`; **split** capture-source vs track (see §1) |
| Enabled-set builder | `engine.rs:935-944` (`enabled_audio_kinds`) | build the 5-track list from config + live binding, **mix first** |
| Endpoint/source map | `engine.rs:964-973` (`match kind`) | new arms → `CaptureSource` per track |
| `BufferParams` audio fields | `engine.rs:734-740` | carry the track toggles + `vc_apps` + binding inputs |
| Endpoint data-flow map | `audio/devices.rs:73-80` (`stream_flow`) | process-loopback sources don't follow an endpoint default — new arms / bypass |
| `[Desktop, Mic]` literal | `main.rs:555` | iterate `engine.audio_streams()` instead |
| Levels asserts / meter labels+colors | `levels.rs:92`, `ui/settings.rs` VU section | new variants |
| Status audio cells (if any) | `status.rs` | grow with the track set (status has no per-track audio field today — add only if the strip should show them) |
| Test fixtures | `engine.rs:2049,2117`, `ring.rs`, `save.rs` tests | enum references |

### Genuinely new engine work (the HW risk)
- **Process-loopback capture** — the current `run_capture` opens by endpoint
  (`DeviceSelection::{DefaultFollow,Pinned(id)}`, `wasapi_stream.rs:363-432`).
  Process loopback binds a **PID + include/exclude tree**, cannot query
  `get_mixformat` (must request a fixed 48 kHz f32 stereo format), and needs a
  PID-liveness watchdog. New module `audio/process_loopback.rs` (B2).
- **The mixer** — a new pure-logic + thread stage that PTS-aligns and sums two
  48 kHz sources into the Mix track (B4).
- **Game/VC binding** — foreground-fullscreen + captured-window PID detector, VC
  process scanner over the `vc_apps` table, rebind-with-logged-gap (B3).
- **Hybrid `moov` finalize** — compute real per-track sample tables and append a
  finalized `moov` on save (B5). `finish()` today only flushes fragments +
  atomic-renames (`fmp4.rs:361`).
- **The ASC-complete save gate** — `process_save_job`/`open_recording` skip a save
  until **all** `num_audio` tracks have delivered their ASC (`engine.rs:1956,1908`,
  `v.len() == num_audio`). With conditional/late tracks (a VC app that opens
  mid-session) this would block saves. Must become "save with whatever tracks are
  ready" (see Decision D4).

---

## 3. Task breakdown

Branch per task (`b1-…`, `b2-…`, …); local-green (`just check` + `just test`) before
merge; batch the HW cycle at **B7** per the dev workflow. Pure-logic parts (mixer,
gap/drift already done, binding heuristics, sample-table math, config) get exhaustive
unit tests **including the spec edge numbers** (`CLAUDE.md` testing rules); COM/HW
parts get a thin `/tools` probe + a checklist and are never claimed to "work" until
the Nitro says so.

### B1 — N-track generalization (pure-logic + wiring; CI-green winnable)
**Goal:** the whole pipeline can carry the 5-track model; the source/track split of
§1 exists; the default (2-track) path is unchanged in behavior *except* track 1
becomes the mix seam (mix itself lands in B4 — until then track 1 may pass through
the endpoint source so nothing regresses; settle in D2).

- Introduce `AudioTrackKind` (Mix, Game, VoiceChat, OtherSystem, Mic) with
  `COUNT`/`index()`/`label()`/`title()`; introduce `CaptureSource`
  (`EndpointLoopback`, `MicEndpoint(DeviceSelection)`, `ProcessLoopback{pid,
  include_tree}`). Decide whether `AudioStreamKind` is renamed to `AudioTrackKind`
  or kept as the source enum — keep the `levels.rs`/`status.rs` grow-with-enum seam
  (`HANDOVER §6` "new from A3/A4").
- Update `levels.rs` `COUNT` + the `const _` asserts + the VU meter color/label
  paths (`ui/settings.rs`); grow `status.rs` only if the strip is to list tracks.
- `enabled_audio_kinds` → a track-set builder from `AudioConfig`
  (`desktop`/`mic`/`separate_tracks`/`tracks`) producing the ordered track list
  **mix first**; the `match kind` selection map → a `CaptureSource` per track.
- Generalize the capture spawn so a track can be fed by a source **or** (Mix) by the
  mixer; keep `track_index` positional routing.
- `main.rs:555` iterate `engine.audio_streams()`.
- **Tests:** track-set builder over every config combination (both `separate_tracks`
  values, each `tracks.*` toggle, mic `off`), ordering (mix always index 0), levels
  round-trip per new kind, `enabled` list == meter set invariant (the
  `enabled_audio_kinds` single-source-of-truth property).
- **HW:** none of its own; folds into B7. "Expect: CI green suffices."
- **Depends on:** nothing. **Blocks:** B2, B4, B5.

### B2 — Process-loopback capture module (`audio/process_loopback.rs`) — HW risk
**Goal:** capture one process tree's audio as an `AudioPacket` stream, same contract
as `run_capture`.

- Use `wasapi::AudioClient::new_application_loopback_client(pid, include_tree)`
  (**confirmed present**, wasapi 0.23.0 `api.rs:681`). **NB the crate's doc/example
  `include_tree: false` is EXCLUDE mode** — `false` ⇒
  `PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE` (`api.rs:689-693`). Include =
  `true`.
- `get_mixformat` etc. are `E_NOTIMPL` on this client (`api.rs:648-661`) — **request
  a fixed format** (48 kHz f32 stereo) at `initialize_client(Capture, Shared,
  EventsShared{autoconvert, buffer_duration_hns})`; the requested period is
  irrelevant (`api.rs:644-645`). `QPCPosition` from `GetBuffer` **is** valid and IS
  the master domain — pass through per amended §2.2 (reuse `PtsDeriver`).
- **PID-liveness watchdog:** process exit ⇒ silence forever, no error (`§5`
  research). Own a poll (e.g. `OpenProcess`/`WaitForSingleObject` on the PID, or a
  scan tick) so the binding can be torn down and the track silence-filled; do not
  rely on a WASAPI error.
- **Serialize activations** — parallel `ActivateAudioInterfaceAsync` spam froze OBS
  (`§5`). One activation at a time (a mutex/queue in the module).
- **Runtime floor probe:** process loopback needs Win10 2004 (build 19041; docs
  claim 20348 — probe, don't trust the doc). Below the floor, **hide the per-app
  tracks**; the mix/mic pipeline is unaffected (`M7-M8-PLAN §2`).
- `unsafe` confined to the module (`CLAUDE.md`); PID-liveness + include/exclude
  policy structs are pure + tested. `windows` feature gates added in the same commit
  that calls the APIs (`07-DEVFLOW.md`).
- **Tests:** the pure parts (mode mapping include↔true, the fixed `WaveFormat`
  builder, the liveness state machine); the COM path via a `/tools/audio-probe`-style
  binary capturing `std::process::id()`'s own tree to a WAV, plus a checklist.
- **HW:** B7 (QPCPosition epoch vs raw QPC; process-exit silence; dead-PID activation
  HRESULT; same-PID double capture; Discord tray-minimized).
- **Depends on:** B1. **Blocks:** B3, B4-other, B7.

### B3 — Game / VC binding — HW risk
**Goal:** decide which PID feeds the game / VC / other-system sources, live.

- **Game binding (orchestrator decision 0.2):** *window mode* → the captured
  window's process tree. *Monitor mode* → **no game track** until the foreground is
  a **fullscreen/borderless** app, then bind that PID's tree; the binding sticks
  while the process lives; a *different* fullscreen app retargets with a **logged,
  silence-filled gap**. **No game-title database** (non-goal intact) — pure
  foreground+fullscreen heuristics.
- **VC scan:** enumerate processes over the `[[audio.vc_apps]]` table
  (`config.rs:264`, Discord family seeded); **detect by process image name, never by
  window** (tray-minimized Discord has no window). Discord = top-most same-name
  process (parent not same-name) + include-tree. Log the chosen PID + why on each
  capture start.
- **Rebind logic** feeds B2's producer a new PID (or none) with a logged gap; the
  §2.3 silence synthesizer already fills the hole downstream.
- **Tests:** the fullscreen-foreground classifier and the VC "top-most same-name"
  selector as pure functions over a fake process/window snapshot (inject the
  enumeration); rebind gap accounting.
- **HW:** B7 (Discord tray-minimized detection; game bind on a borderless title;
  retarget gap).
- **Depends on:** B2. **Blocks:** B7.

### B3.5 — Mic-device dropdown (the last owed Slice-A fast-follow)
**Goal:** replace A5's free-text pinned-id field with a populated device list.

- Add a WASAPI `EnumAudioEndpoints` + friendly-name wrapper in `audio/devices.rs`
  (confined unsafe COM). Populate a combo in `ui/settings.rs` (keep **Default
  (follow)** and **Off** as entries); still writes through `Config::write_atomic`;
  still validated. Fixes the A5 finding "a bad pinned id just fails to open"
  (`HANDOVER §5` A5 finding #2).
- **Tests:** the enumeration→UI-model mapping is pure; the COM read is HW-only.
- **HW:** B7 (rides B2's audio-COM cycle — pick the FIFINE, restart, mic returns;
  unplug it, the list updates on reopen).
- **Depends on:** nothing hard (parallel to B2/B3); **land it on the B7 COM cycle.**

### B4 — Mix track — HW-adjacent (pure mixer + a thread)
**Goal:** the always-first Mix track = sum(endpoint loopback, mic), −3 dB headroom,
soft clip.

- **Pure mixer core:** two 48 kHz sources in (each already gap-filled + drift-
  corrected by its `StreamResampler`, so both are continuous from their anchor),
  PTS-aligned on the master-domain tick grid, summed sample-wise; apply −3 dB
  headroom then a soft clipper (simple `tanh`-style or cubic — **no AGC/filters**,
  DSP rabbit holes are out of scope permanently per `08` REJECTED + `M7-M8-PLAN §6`).
  Handle unequal anchors (mic starts later → the earlier source plays alone until
  the other joins) and a source stopping (fall back to the survivor).
- **Wiring:** the endpoint + mic sources fan their resampled chunks to the mix
  encoder in addition to their own standalone track encoder (when present). In the
  2-track default there are no standalone endpoint/other tracks — the mixer + mic
  are the whole output.
- **Tests (exhaustive, spec-pinned):** sum of two known signals; −3 dB gain exact;
  soft-clip monotonic + bounded at/above full scale; aligned/misaligned anchors;
  one source silent; one source ending mid-clip; a gap in one source (already
  silence from its resampler) doesn't shift the other.
- **HW:** B7 (mix plays; levels sane; a Discord-upload of the default 2-track clip
  plays the mix).
- **Depends on:** B1 (+ B2 for the source fan-out shape). **Blocks:** B7.

### B5 — Muxer: N tracks + hybrid `moov` finalize on save (amended §4)
**Goal:** 5 audio tracks, mix first, all `track_enabled | track_in_movie` flagged;
finalized `moov` appended on save.

- The N-track write path is **already there** (`fmp4.rs` `Vec<AudioTrack>`); confirm
  `build_moov` orders mix first and sets the enabled/in-movie flags so disabled
  tracks don't vanish in editors (HandBrake precedent, `M7-M8-PLAN §2`).
- **New:** compute per-track sample tables (`stts`/`stsz`/`stsc`/`stco`/`stss`) from
  the fragments and write a **finalized `moov`** on save (OBS-Hybrid: fragments
  first for crash-safety, appended/rewritten `moov` for editor + Explorer
  compatibility — solves the fMP4-on-disk duration/seek quirks, `§4` amendment +
  `M7-M8-PLAN §5`). Preserve §4.7 atomicity (`.part` → fsync → rename) and §4.6
  fragment-first ordering.
- **Tests (pure box math):** sample-table sizes/offsets for a known fragment
  sequence; `moov` box nesting/lengths (extend the existing
  `moov_with_two_audio_tracks_nests_and_counts_tracks` test, `fmp4.rs:1043`) to 5
  tracks; round-trip a small file through `just verify` (ffprobe) assertions.
- **HW:** B7 (a 5-track clip → CapCut import reads all enabled tracks + plays mix;
  Explorer shows correct duration; WMP seeks).
- **Depends on:** B1. **Blocks:** B7. (Can proceed in parallel with B2/B3.)

### B6 — LIMITATIONS.md + docs
**Goal:** the honesty list for the 4-track reality.

- In-game voice (Vivox/EOS/Steamworks — Valorant/Fortnite/Apex/LoL) renders **inside
  the game process** and can NEVER be separated (`M7-M8-PLAN §2`). VC **bleed in
  track 4** (accepted, decision 0.1 — "other system" also contains VC; API can't
  express system−game−VC). Pings/soundboard/Go-Live on the VC track. **Uploads
  flatten to track 1.** Browser-based VC out of scope. **Win10 <2004 hides per-app
  tracks.** Editors keeping tracks 3+4 double the VC.
- Update `README.md` audio section; keep it in the existing `LIMITATIONS.md` voice.
- **Depends on:** the shape settled by B2–B5. **No HW.**

### B7 — HW validation cycle (Nitro) — the gate
Re-pass **AV-1..AV-5** with tracks on (M8 acceptance) + the empirical checklist:
QPCPosition epoch vs raw QPC; process-exit behavior; mute-state behavior; dead-PID
activation HRESULT; same-PID double capture; Discord tray-minimized; **long-session
(≥ 1 h) crackle/drift watch** (OBS #8086 desync is unfixed there — our per-stream
§2.4 controller is the mitigation, prove it); a **5-track clip → Discord upload +
CapCut import** behave (mix plays). Also close **B3.5** (mic dropdown) and the
**still-owed 2 h open-window UI soak** (M7 acceptance) on this cycle if not already
done. Record every result in `05-MILESTONE-TRACKER.md` with the date.

---

## 4. Decisions

**D1 and D2 are LOCKED (orchestrator, 2026-07-08; see `DECISIONS.md`).** D3–D6 the
agent decides under the CLAUDE.md §"ambiguity" rules (simpler / more-logged /
reversible, logged in DECISIONS) unless the orchestrator wants a say.

- **D1 — `separate_tracks` semantics change + default flip. ✅ LOCKED.** Today
  `separate_tracks = true` (the shipped default) means "desktop + mic, two tracks"
  (`config.rs:315,333`). Slice B redefines it: **`false` = mix + mic (2 tracks),
  `true` = full 5-track**, and the **default flips to `false`** — mix+mic is the new
  "current users' muscle memory" (`M7-M8-PLAN §2`). The **default clip changes from
  {desktop, mic} to {mix, mic}.** Migration (pre-1.0 friends-beta, no
  `config_version` bump): the key is honored under the new semantics with the new
  `false` default; a hand-written `separate_tracks = true` from Slice A now yields
  the full 5-track set (acceptable — they asked for separate tracks). Documented in
  B6. Update the `AudioConfig::default()` + the config template + `--check-config`
  wording in B1.
- **D2 — B1 interim for track 1. ✅ LOCKED: pass-through in B1, real sum in B4.**
  Between B1 and B4 track 1 passes through the raw default-endpoint loopback ("mix"
  is a rename only until B4 lands the real sum), so B1 stays CI-green and
  independently mergeable and the working desktop path never regresses mid-slice.
- **D3 — mixer topology (source fan-out vs double-capture).** Recommend **fan-out**:
  the endpoint + mic sources each resample once and feed both the mix encoder and
  their standalone track encoder — avoids a second WASAPI loopback client and keeps
  one drift domain per source. (Reversible; logged.)
- **D4 — ASC-complete save gate.** Change `v.len() == num_audio`
  (`engine.rs:1956,1908`) to admit a save with the tracks whose ASC is ready, so a
  late/conditional track (VC app opening mid-session, a not-yet-bound game) never
  blocks a save. Recommend: save the tracks that are ready; a track with no source
  yet is simply absent that clip (or an all-silence track — decide). (Logged.)
- **D5 — other-system source switch = logged gap, not an epoch.** Retargeting
  track 4 between endpoint-loopback and process-exclude-tree (game bind/unbind) is a
  **within-epoch** source swap filled by §2.3 silence (like a device rebuild), NOT a
  video-touching epoch bump. Confirm it does not restart the ring/encoder.
- **D6 — `AacEncoder::new(kind)` param.** The `kind` is cosmetic (`mft_aac.rs:110`);
  either thread the new track kind through for logging or drop the param. Trivial;
  agent's call.

---

## 5. Budgets & risks (constraint 7 — surfaced now)

- **CPU** (`§6.4`: "0.5–1.5%, budget 2%" at 2 audio streams) needs re-baselining at
  up to **5 sources + 5 AAC encoders + the mixer**; expect ~+0.5–1%, **measure at
  B7** and fail the manual test if over 2%. The mixer + fan-out adds one thread and
  two sums per 48 kHz frame — cheap, but prove it.
- **Ring RAM** +~1.2 Mbit/s for the extra AAC tracks (negligible vs the video-
  dominated `§6.2` byte caps); confirm `est_bitrate` in the byte-cap accounts for up
  to 5 × 160 kbps, not 2.
- **Binary size** +process-loopback + mixer is small; the release build is **8.81 MB
  vs the 10 MB budget** today — watch it but no risk expected. (`just release`
  checks it.)
- **Sync under long session** — the OBS #8086 crackle/desync is the headline HW risk;
  the per-stream §2.4 drift controller already exists and is the mitigation. The
  ≥ 1 h crackle/drift watch at B7 is the proof.
- **Process-loopback fragility** — dead-PID activation HRESULT, same-PID double
  capture, activation serialization: all field-issue-driven (`M7-M8-PLAN §5`), all
  covered by B2's watchdog/serialization and B7's checklist. Never claim B2 works
  until the Nitro says so (`CLAUDE.md`).

---

## 6. Sequencing

```
B1 (enum/track model, CI-green) ─┬─▶ B2 (process loopback) ──▶ B3 (game/VC binding) ─┐
                                 ├─▶ B4 (mixer) ───────────────────────────────────── ├─▶ B7 (Nitro HW gate) ─▶ friends-beta v1
                                 └─▶ B5 (muxer N + hybrid moov) ──────────────────────┘        │
                       B3.5 (mic dropdown) ──────────────────────────────────────────▶ (rides B7 COM cycle)
                       B6 (LIMITATIONS/docs) ─────────────────────────────────────────▶ (after B2–B5 shape settles)
```

- **B1 first** (unblocks everything, CI-green winnable, no HW).
- **B2 → B3** are the process-loopback spine (B3 needs B2's PID-bound producer).
- **B4 (mixer)** and **B5 (muxer)** parallel B2/B3 — both depend only on B1.
- **B3.5** parallel; **land it on the B7 audio-COM HW cycle** (`M7-M8-PLAN §4`).
- **B6** once the topology is real; **B7** is the single batched Nitro gate that
  closes the slice → friends-beta v1.
- After Slice B: the **UI pass** (whose planning gates the two deferred A6 items —
  live hotkey re-registration + record_toggle re-default) → final friend release
  (`M7-M8-PLAN §7`). M6's external-hardware matrix closes on beta evidence along the
  way.

## 7. Acceptance (Slice B done)
- AV-1..AV-5 re-pass **with the 5-track set on** (B7).
- A default (mix+mic) clip and a full 5-track clip both `just verify`-clean; the
  5-track clip imports into CapCut (all enabled tracks) and uploads to Discord (mix
  plays); Explorer shows correct duration.
- Discord auto-detected tray-minimized; a game binds in monitor mode on a borderless
  title; retarget leaves a silence gap, no desync.
- ≥ 1 h session: no crackle, drift within §5 budget (the incumbent-failing test).
- CPU ≤ 2% at 5 streams; binary ≤ 10 MB.
- `LIMITATIONS.md` + README tell the truth about VC bleed, in-game voice, the Win10
  floor, and upload flattening.
