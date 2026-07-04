//! `save` — the hotkey save path: the frozen `§4` rebasing contract over the ring.
//!
//! On a save request for the last `L` seconds at master time `T_req`, [`select_window`]
//! implements `§4.1`–`§4.4` **purely** (no COM, unit-tested): it walks the ring back
//! to the right keyframe, picks the clip `origin`, and gathers the video + per-track
//! audio packets that belong to the clip. [`save_clip`] is the thin, safe shell that
//! drives the reused [`Fmp4Writer`] over that window and writes the file atomically.
//!
//! ## Why this reuses the record-path muxer
//! `Fmp4Writer` aligns A/V to an **origin = the first video packet's PTS**, emitting
//! `pts − origin`. `select_window` feeds it packets starting at the chosen `§4.2`
//! IDR, so the muxer's origin *is* the `§4` origin and its offsetting *is* the
//! `§4.3`/`§4.4` rebasing — no second muxer, and the `§4.5` container math /
//! `§4.6` fragmenting / `§4.7` atomic rename all come for free. This module owns the
//! *selection* (`§4.2` origin + epoch clamp, `§4.4` trailing-audio bound); the muxer
//! owns the *mechanism*.
//!
//! ## NOT the M2 record alignment
//! The M2 record path also origin-aligns, but there origin = the first frame of the
//! recording. Here origin = a chosen IDR at/before `target`, the clip is bounded to
//! one epoch (`§0`: a clip must not span epochs), and trailing audio is included to
//! `last_video_pts + D` — the real `§4` contract (DECISIONS "M2 Task 5" deferred it
//! here). This module is 100 % safe (`CLAUDE.md`: `save` is on the no-`unsafe` list);
//! `save_clip` calls the muxer's safe API but contains no `unsafe` itself.

use std::path::{Path, PathBuf};

use windows::Win32::Media::MediaFoundation::IMFMediaType;

use crate::encode::mft_aac::EncodedAudioPacket;
use crate::encode::mft_h264::EncodedPacket;
use crate::mux::fmp4::{AudioTrackConfig, Fmp4Writer};
use crate::mux::MuxError;
use crate::ring::Ring;

/// Errors from the save path.
#[derive(Debug, thiserror::Error)]
pub enum SaveError {
    /// The ring holds no video — nothing to save.
    #[error("ring is empty — nothing to save")]
    Empty,
    /// No IDR keyframe in the newest epoch to start the clip from (should not
    /// happen — every epoch begins with an IDR).
    #[error("no IDR keyframe in the buffer to start the clip from")]
    NoKeyframe,
    /// The muxer failed while writing the clip.
    #[error("mux: {0}")]
    Mux(#[from] MuxError),
}

/// A selected save window over the ring — the `§4` clip, ready to mux. Owns cloned
/// packets (`Arc<[u8]>` handle clones, no bulk copy) so the engine can release the
/// ring lock before muxing (the RAM budget, `01-PROJECT-PLAN.md §1`). Packets keep
/// their ORIGINAL PTS; the muxer rebases them against [`Self::origin`].
#[derive(Debug)]
pub struct SaveWindow {
    /// Clip origin PTS (`§4.2`): the newest IDR with `pts ≤ target` in the newest
    /// epoch, or that epoch's first IDR when clamped. Output PTS = `pts − origin`,
    /// applied by the muxer (`§4.3`).
    pub origin: i64,
    /// The single epoch the clip belongs to (`§0`: a clip must not span epochs).
    pub epoch_id: u32,
    /// True when `target` fell before the newest epoch's first IDR, so the clip is
    /// shorter than requested (`§4.2`) — the caller logs + toasts.
    pub clamped: bool,
    /// Video packets with `pts ≥ origin`, in order (original PTS).
    pub video: Vec<EncodedPacket>,
    /// Per-track audio packets with `origin ≤ pts < last_video_pts + D` (`§4.4`).
    pub audio: Vec<Vec<EncodedAudioPacket>>,
    /// PTS of the last video packet in the window (clip end, for logging).
    pub last_video_pts: i64,
}

impl SaveWindow {
    /// Total packets selected (video + all audio) — a cheap size signal for logs.
    pub fn packet_count(&self) -> usize {
        self.video.len() + self.audio.iter().map(Vec::len).sum::<usize>()
    }
}

/// Select the `§4` clip window from the ring for a save whose start time is
/// `target_ticks` (`= T_req − L`, master domain). Pure: clones the chosen packets'
/// `Arc` handles and returns them; the caller muxes off-lock.
///
/// `§4.2`: `origin` = newest video IDR with `pts ≤ target` in the newest packet's
/// epoch; if `target` precedes that epoch's first IDR, `origin` = the first IDR of
/// the epoch and the clip is `clamped` (shorter than requested). `§4.3`: video =
/// packets with `pts ≥ origin`. `§4.4`: audio (per track) starts at the first
/// `pts ≥ origin`. The clip END is `min(video_end, each track's last audio end)` so
/// every stream has data to the end — in buffer mode the newest audio lags the
/// newest video (pipeline latency, no per-save flush), and taking all video would
/// leave the audio short of `§5 AV-3`'s one-AAC-frame bound.
pub fn select_window(ring: &Ring, target_ticks: i64) -> Result<SaveWindow, SaveError> {
    let video = ring.video();
    let newest = video.back().ok_or(SaveError::Empty)?;
    let epoch_id = newest.epoch_id;

    // §4.2 origin: scan the newest epoch's IDRs (oldest→newest). Track the epoch's
    // first IDR (for the clamp) and the newest IDR at/before target (the origin).
    let mut origin_pts: Option<i64> = None;
    let mut first_epoch_idr: Option<i64> = None;
    for p in video
        .iter()
        .filter(|p| p.epoch_id == epoch_id && p.is_keyframe)
    {
        if first_epoch_idr.is_none() {
            first_epoch_idr = Some(p.pts);
        }
        if p.pts <= target_ticks {
            origin_pts = Some(p.pts); // oldest→newest, so the last hit is the newest
        }
    }
    let (origin, clamped) = match origin_pts {
        Some(o) => (o, false),
        // target precedes the epoch's first IDR: clamp to it (clip is shorter).
        None => (first_epoch_idr.ok_or(SaveError::NoKeyframe)?, true),
    };

    // §4.3 video candidates: pts ≥ origin, bounded to the newest epoch (a clip must
    // not span epochs, §0 — belt-and-braces alongside the naturally monotonic PTS).
    let video_all: Vec<&EncodedPacket> = video
        .iter()
        .filter(|p| p.epoch_id == epoch_id && p.pts >= origin)
        .collect();
    let last = video_all.last().ok_or(SaveError::NoKeyframe)?;
    let video_end = last.pts + last.duration; // last_video_pts + D

    // Audio candidates per track: pts ≥ origin. §4.4 bounds trailing audio at
    // `last_video_pts + D`, ASSUMING audio reaches the newest video. In buffer mode
    // it does NOT: the newest audio LAGS the newest video by the audio pipeline
    // latency (WASAPI buffer + AAC framing, ~60–90 ms) and there is no per-save
    // flush (unlike the record path's stop-time flush). So the clip must END where
    // EVERY track has data — otherwise audio is short of video and fails §5 AV-3
    // ("audio within 1 AAC frame"). `clip_end = min(video_end, each track's last
    // audio end)`; the min() also covers the §4.4 audio-ahead case.
    let mut clip_end = video_end;
    let mut audio_candidates: Vec<Vec<&EncodedAudioPacket>> =
        Vec::with_capacity(ring.num_audio_tracks());
    for t in 0..ring.num_audio_tracks() {
        let deque = ring.audio_track(t).expect("track index in range");
        let cand: Vec<&EncodedAudioPacket> = deque.iter().filter(|a| a.pts >= origin).collect();
        if let Some(last_a) = cand.last() {
            clip_end = clip_end.min(last_a.pts + last_a.duration);
        }
        audio_candidates.push(cand);
    }

    // Trim every stream to [origin, clip_end) so the tracks end together (the
    // first audio AU is still the first with pts ≥ origin — ≤ one 21.33 ms AU of
    // head silence, §4.4; the muxer rebases against origin). clip_end > origin
    // because audio starts at/after origin, so the origin IDR always survives.
    let video_win: Vec<EncodedPacket> = video_all
        .iter()
        .filter(|p| p.pts < clip_end)
        .map(|p| (*p).clone())
        .collect();
    let last_video_pts = video_win.last().ok_or(SaveError::NoKeyframe)?.pts;
    let audio: Vec<Vec<EncodedAudioPacket>> = audio_candidates
        .iter()
        .map(|cand| {
            cand.iter()
                .filter(|a| a.pts < clip_end)
                .map(|a| (*a).clone())
                .collect()
        })
        .collect();

    Ok(SaveWindow {
        origin,
        epoch_id,
        clamped,
        video: video_win,
        audio,
        last_video_pts,
    })
}

/// Drive the reused [`Fmp4Writer`] over a selected window and write the clip
/// atomically (`§4.5`–`§4.7` are the muxer's). `output_type` is the video encoder's
/// negotiated output media type (frame size, fps, SPS/PPS for `avcC`) — the same
/// one the record path hands the muxer, captured once per epoch by the engine and
/// matching the window's epoch. Packets are fed in PTS order (video first on a tie)
/// so the origin IDR sets the muxer origin and fragments interleave ~1 s at a time,
/// like the record path.
pub fn save_clip(
    window: &SaveWindow,
    output_type: &IMFMediaType,
    audio_tracks: &[AudioTrackConfig],
    output_path: &Path,
) -> Result<PathBuf, SaveError> {
    let mut mux = Fmp4Writer::create(output_type, audio_tracks, output_path)?;
    for item in merged_feed(window) {
        match item {
            Feed::Video(p) => mux.write_video_packet(p)?,
            Feed::Audio(track, a) => mux.write_audio_packet(track, a)?,
        }
    }
    Ok(mux.finish()?)
}

/// One item in the PTS-ordered feed to the muxer.
enum Feed<'a> {
    Video(&'a EncodedPacket),
    Audio(usize, &'a EncodedAudioPacket),
}

impl Feed<'_> {
    fn pts(&self) -> i64 {
        match self {
            Feed::Video(p) => p.pts,
            Feed::Audio(_, a) => a.pts,
        }
    }
    /// Video sorts before audio at an equal PTS, so the origin IDR is fed first and
    /// sets the muxer origin before any audio at the same tick.
    fn rank(&self) -> u8 {
        match self {
            Feed::Video(_) => 0,
            Feed::Audio(..) => 1,
        }
    }
}

/// Merge the window's video + per-track audio into one PTS-ordered feed. The
/// per-stream inputs are already sorted, so this is a stable sort by `(pts, rank)`.
fn merged_feed(window: &SaveWindow) -> Vec<Feed<'_>> {
    let mut items: Vec<Feed> = Vec::with_capacity(window.packet_count());
    items.extend(window.video.iter().map(Feed::Video));
    for (t, track) in window.audio.iter().enumerate() {
        items.extend(track.iter().map(|a| Feed::Audio(t, a)));
    }
    items.sort_by(|x, y| x.pts().cmp(&y.pts()).then(x.rank().cmp(&y.rank())));
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::wasapi_stream::AudioStreamKind;
    use crate::ring::{Ring, RingCaps};
    use std::sync::Arc;

    fn vpkt(pts: i64, dur: i64, keyframe: bool, epoch: u32) -> EncodedPacket {
        EncodedPacket {
            data: Arc::from(vec![0u8; 8]),
            pts,
            duration: dur,
            is_keyframe: keyframe,
            epoch_id: epoch,
        }
    }
    fn apkt(pts: i64, dur: i64) -> EncodedAudioPacket {
        EncodedAudioPacket {
            stream: AudioStreamKind::Desktop,
            data: Arc::from(vec![0u8; 4]),
            pts,
            duration: dur,
        }
    }

    /// A ring with no eviction (huge caps) and `tracks` audio tracks.
    fn open_ring(tracks: usize) -> Ring {
        Ring::new(RingCaps {
            max_duration_ticks: i64::MAX,
            max_bytes: u64::MAX,
            num_audio_tracks: tracks,
        })
    }

    /// Push `gops` GOPs of `frames` frames, duration `d`, starting at `start`,
    /// epoch `epoch`; frame 0 of each GOP is an IDR. Returns the next pts.
    fn push_gops(
        ring: &mut Ring,
        start: i64,
        gops: usize,
        frames: usize,
        d: i64,
        epoch: u32,
    ) -> i64 {
        let mut pts = start;
        for _ in 0..gops {
            for f in 0..frames {
                ring.push_video(vpkt(pts, d, f == 0, epoch));
                pts += d;
            }
        }
        pts
    }

    fn vptss(w: &SaveWindow) -> Vec<i64> {
        w.video.iter().map(|p| p.pts).collect()
    }

    #[test]
    fn idr_walkback_picks_newest_idr_at_or_before_target() {
        // Two GOPs of 4 frames, d = 1000: IDRs at 0 and 4000.
        let mut ring = open_ring(0);
        push_gops(&mut ring, 0, 2, 4, 1_000, 0);
        // target mid-second-GOP → origin = IDR @ 4000.
        let w = select_window(&ring, 5_500).unwrap();
        assert_eq!(w.origin, 4_000);
        assert!(!w.clamped);
        assert_eq!(vptss(&w), vec![4_000, 5_000, 6_000, 7_000]);
        assert_eq!(w.last_video_pts, 7_000);
    }

    #[test]
    fn idr_walkback_target_before_second_idr_uses_first() {
        let mut ring = open_ring(0);
        push_gops(&mut ring, 0, 2, 4, 1_000, 0);
        // target = 3999 → newest IDR ≤ target is @ 0.
        let w = select_window(&ring, 3_999).unwrap();
        assert_eq!(w.origin, 0);
        assert!(!w.clamped);
        // Window spans the GOP boundary (0..7000) — rebasing across it is the
        // muxer's job; here we confirm the whole range is selected.
        assert_eq!(vptss(&w).len(), 8);
    }

    #[test]
    fn epoch_clamp_when_target_precedes_newest_epoch_first_idr() {
        // Epoch 0: IDR@0, P@1000. Epoch 1: IDR@2000, P@3000, IDR@4000, P@5000.
        let mut ring = open_ring(0);
        ring.push_video(vpkt(0, 1_000, true, 0));
        ring.push_video(vpkt(1_000, 1_000, false, 0));
        ring.push_video(vpkt(2_000, 1_000, true, 1));
        ring.push_video(vpkt(3_000, 1_000, false, 1));
        ring.push_video(vpkt(4_000, 1_000, true, 1));
        ring.push_video(vpkt(5_000, 1_000, false, 1));
        // target = 1500 precedes epoch 1's first IDR (@2000) → clamp to it.
        let w = select_window(&ring, 1_500).unwrap();
        assert_eq!(w.epoch_id, 1);
        assert!(w.clamped);
        assert_eq!(w.origin, 2_000);
        // Only epoch-1 packets, none from epoch 0.
        assert_eq!(vptss(&w), vec![2_000, 3_000, 4_000, 5_000]);
    }

    #[test]
    fn newest_epoch_only_even_when_older_epoch_has_idr_at_or_before_target() {
        // Epoch 0 IDR@0..; epoch 1 IDR@2000. target=2500 → origin = epoch-1 IDR@2000
        // (the newest epoch's IDR ≤ target), never epoch 0.
        let mut ring = open_ring(0);
        ring.push_video(vpkt(0, 1_000, true, 0));
        ring.push_video(vpkt(1_000, 1_000, false, 0));
        ring.push_video(vpkt(2_000, 1_000, true, 1));
        ring.push_video(vpkt(3_000, 1_000, false, 1));
        let w = select_window(&ring, 2_500).unwrap();
        assert_eq!(w.epoch_id, 1);
        assert!(!w.clamped);
        assert_eq!(w.origin, 2_000);
        assert_eq!(vptss(&w), vec![2_000, 3_000]);
    }

    #[test]
    fn trailing_audio_bounded_to_last_video_plus_d() {
        // One GOP of 4 frames, d = 1000: last_video_pts = 3000, D = 1000, so the
        // audio bound is 4000 (exclusive).
        let mut ring = open_ring(1);
        push_gops(&mut ring, 0, 1, 4, 1_000, 0);
        for pts in (0..=4_500).step_by(500) {
            ring.push_audio(0, apkt(pts, 213_333));
        }
        let w = select_window(&ring, 3_000).unwrap();
        let a: Vec<i64> = w.audio[0].iter().map(|p| p.pts).collect();
        // origin = 0; keep 0..<4000; drop 4000 and 4500.
        assert_eq!(a, vec![0, 500, 1_000, 1_500, 2_000, 2_500, 3_000, 3_500]);
    }

    #[test]
    fn audio_head_starts_at_first_packet_at_or_after_origin() {
        // Two GOPs so origin can be the second IDR (@4000). Audio before origin is
        // excluded (§4.4: first AU is the first with pts ≥ origin).
        let mut ring = open_ring(1);
        push_gops(&mut ring, 0, 2, 4, 1_000, 0); // last_video_pts=7000, bound=8000
        for pts in (3_000..=7_000).step_by(500) {
            ring.push_audio(0, apkt(pts, 213_333));
        }
        let w = select_window(&ring, 5_500).unwrap(); // origin = 4000
        let a: Vec<i64> = w.audio[0].iter().map(|p| p.pts).collect();
        assert_eq!(*a.first().unwrap(), 4_000); // 3000/3500 dropped (< origin)
        assert!(*a.last().unwrap() < 8_000);
    }

    #[test]
    fn video_trimmed_to_audio_end_when_audio_lags() {
        // The real buffer-mode case: audio lags video at save time. Video runs to
        // pts 5000 (end 6000); audio only reaches pts 2000 (end 3000). The clip must
        // end at the audio end, trimming the trailing video that has no audio —
        // otherwise the tracks misalign (the -80 ms AV-3 failure from the Nitro).
        let mut ring = open_ring(1);
        push_gops(&mut ring, 0, 1, 6, 1_000, 0); // video pts 0..5000, end 6000
        for pts in (0..=2_000).step_by(1_000) {
            ring.push_audio(0, apkt(pts, 1_000)); // audio pts 0,1000,2000; end 3000
        }
        let w = select_window(&ring, 0).unwrap();
        // clip_end = min(6000, 3000) = 3000 → video trimmed to pts < 3000.
        assert_eq!(vptss(&w), vec![0, 1_000, 2_000]);
        assert_eq!(w.last_video_pts, 2_000);
        let a: Vec<i64> = w.audio[0].iter().map(|p| p.pts).collect();
        assert_eq!(a, vec![0, 1_000, 2_000]);
        // Ends align: video_end = 2000+1000 = 3000; audio_end = 2000+1000 = 3000.
    }

    #[test]
    fn two_audio_tracks_selected_independently() {
        let mut ring = open_ring(2);
        push_gops(&mut ring, 0, 1, 4, 1_000, 0); // bound = 4000
        for pts in (0..=4_000).step_by(1_000) {
            ring.push_audio(0, apkt(pts, 213_333));
            ring.push_audio(1, apkt(pts, 213_333));
        }
        let w = select_window(&ring, 0).unwrap();
        assert_eq!(w.audio.len(), 2);
        // Both tracks: 0,1000,2000,3000 (4000 excluded).
        for track in &w.audio {
            let a: Vec<i64> = track.iter().map(|p| p.pts).collect();
            assert_eq!(a, vec![0, 1_000, 2_000, 3_000]);
        }
    }

    #[test]
    fn empty_ring_errors() {
        let ring = open_ring(1);
        assert!(matches!(select_window(&ring, 0), Err(SaveError::Empty)));
    }

    #[test]
    fn merged_feed_is_pts_ordered_video_first_on_ties() {
        let mut ring = open_ring(1);
        push_gops(&mut ring, 0, 1, 3, 1_000, 0); // video @0,1000,2000
        ring.push_audio(0, apkt(0, 213_333)); // audio @0 ties with video IDR
        ring.push_audio(0, apkt(1_000, 213_333));
        let w = select_window(&ring, 0).unwrap();
        let feed = merged_feed(&w);
        // First item is the video IDR (rank 0) at pts 0, not the audio at pts 0.
        assert!(matches!(feed[0], Feed::Video(p) if p.pts == 0 && p.is_keyframe));
        assert!(matches!(feed[1], Feed::Audio(0, a) if a.pts == 0));
        // Feed is non-decreasing in PTS.
        let ptss: Vec<i64> = feed.iter().map(Feed::pts).collect();
        assert!(ptss.windows(2).all(|w| w[0] <= w[1]));
    }
}
