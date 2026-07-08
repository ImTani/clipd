//! `ring` — the compressed-packet replay ring (`02-AV-SYNC-SPEC.md §3`, `§6.2`).
//!
//! The buffer that makes clipd a replay-buffer clipper: continuous capture feeds
//! encoded video + audio packets in; the ring retains the most recent window,
//! bounded by BOTH a duration cap and a byte cap; a hotkey save (`save.rs`, M3-2)
//! walks it back to a keyframe and muxes the last N seconds.
//!
//! Per `§3`:
//! - one `VecDeque<EncodedPacket>` for video, one `VecDeque<EncodedAudioPacket>`
//!   per audio track (0 = desktop, 1 = mic — `§2.5`);
//! - **video eviction is whole-GOP only**: pop from the front until the front is
//!   again an IDR, so a survivor always begins with a keyframe a save can start
//!   from — never a partial GOP with orphaned P-frames (plan pitfall 20);
//! - **audio eviction follows video**: drop audio with `pts < video_front_pts −
//!   500 ms`, the slack that guarantees audio always fully covers any surviving
//!   video range;
//! - **both caps enforced on every push** (`buffer_seconds` and `buffer_bytes =
//!   buffer_seconds × est_bitrate × 1.5`, `§6.2`).
//!
//! This module is pure and 100 % safe (`CLAUDE.md`: ring is on the no-`unsafe`,
//! unit-test-heavy list). Packet bytes are `Arc<[u8]>`, so retaining a packet and
//! snapshotting a save window are handle clones, never bulk copies — the property
//! the RAM budget depends on (`01-PROJECT-PLAN.md §1`; DECISIONS 2026-07-04).

use std::collections::VecDeque;

use crate::encode::mft_aac::EncodedAudioPacket;
use crate::encode::mft_h264::EncodedPacket;
use crate::spec_constants::ring::AUDIO_EVICTION_SLACK_MS;
use crate::spec_constants::units::TICKS_PER_MILLISECOND;

/// The dual caps bounding the ring, both enforced on every push (`§3`/§6.2).
#[derive(Debug, Clone, Copy)]
pub struct RingCaps {
    /// Retained video duration cap, in ticks (`buffer_seconds × 1 s`).
    pub max_duration_ticks: i64,
    /// Total byte cap over video + audio
    /// (`buffer_seconds × est_bitrate × 1.5`, `§6.2`).
    pub max_bytes: u64,
    /// Number of audio tracks (0 = video-only; else desktop first, mic second).
    pub num_audio_tracks: usize,
}

/// The compressed-packet ring. Drive it from the buffer thread: `push_video` /
/// `push_audio` as packets arrive, and (on a hotkey) hand `video`/`audio_track`
/// to the save path.
pub struct Ring {
    caps: RingCaps,
    video: VecDeque<EncodedPacket>,
    /// One deque per audio track, indexed as `§2.5` (0 = desktop, 1 = mic).
    audio: Vec<VecDeque<EncodedAudioPacket>>,
    /// Running byte totals (kept incrementally so the byte cap is O(1) per push).
    video_bytes: u64,
    audio_bytes: u64,
}

impl Ring {
    /// Create an empty ring with the given caps.
    pub fn new(caps: RingCaps) -> Self {
        let audio = (0..caps.num_audio_tracks)
            .map(|_| VecDeque::new())
            .collect();
        Ring {
            caps,
            video: VecDeque::new(),
            audio,
            video_bytes: 0,
            audio_bytes: 0,
        }
    }

    /// Admit one encoded video packet, then enforce the caps.
    pub fn push_video(&mut self, packet: EncodedPacket) {
        self.video_bytes += packet.data.len() as u64;
        self.video.push_back(packet);
        self.enforce();
    }

    /// Admit one encoded AAC access unit for `track` (0 = desktop). A packet for a
    /// track that does not exist is dropped (returns `false`) — mirrors the muxer's
    /// tolerance of an out-of-range track index.
    pub fn push_audio(&mut self, track: usize, packet: EncodedAudioPacket) -> bool {
        let Some(deque) = self.audio.get_mut(track) else {
            return false;
        };
        self.audio_bytes += packet.data.len() as u64;
        deque.push_back(packet);
        self.enforce();
        true
    }

    /// Drop everything (the `clear_after_save` option, `§6.2` / plan pitfall 23).
    pub fn clear(&mut self) {
        self.video.clear();
        for track in &mut self.audio {
            track.clear();
        }
        self.video_bytes = 0;
        self.audio_bytes = 0;
    }

    // ── read access for the save path (M3-2) and the watchdog ────────────────

    /// The retained video packets, oldest first. The front is always an IDR once
    /// any eviction has run (whole-GOP invariant).
    pub fn video(&self) -> &VecDeque<EncodedPacket> {
        &self.video
    }

    /// The retained AAC access units for `track` (0 = desktop), oldest first.
    pub fn audio_track(&self, track: usize) -> Option<&VecDeque<EncodedAudioPacket>> {
        self.audio.get(track)
    }

    /// Number of audio tracks.
    pub fn num_audio_tracks(&self) -> usize {
        self.audio.len()
    }

    /// The caps this ring was built with (the engine compares the retained
    /// duration against `max_duration_ticks` for the `§6.2` auto-QP-relief signal).
    pub fn caps(&self) -> RingCaps {
        self.caps
    }

    /// Retained video span in ticks: `back.pts + back.duration − front.pts`
    /// (0 when empty). This is what the duration cap bounds and the fill fraction
    /// is measured against.
    pub fn duration_ticks(&self) -> i64 {
        match (self.video.front(), self.video.back()) {
            (Some(front), Some(back)) => (back.pts + back.duration) - front.pts,
            _ => 0,
        }
    }

    /// Total retained bytes (video + all audio) — what the byte cap bounds.
    pub fn total_bytes(&self) -> u64 {
        self.video_bytes + self.audio_bytes
    }

    /// Whether the ring holds no video packets.
    pub fn is_empty(&self) -> bool {
        self.video.is_empty()
    }

    // ── eviction (`§3`) ──────────────────────────────────────────────────────

    /// Enforce both caps: evict whole video GOPs while either cap is exceeded (and
    /// a spare GOP exists), trimming audio behind the advancing video front.
    fn enforce(&mut self) {
        while self.over_cap() {
            if !self.evict_oldest_gop() {
                break; // only one GOP left — never evict the last (a save needs it)
            }
            self.evict_audio();
        }
        self.evict_audio();
    }

    /// Either cap exceeded?
    fn over_cap(&self) -> bool {
        self.duration_ticks() > self.caps.max_duration_ticks
            || self.total_bytes() > self.caps.max_bytes
    }

    /// True when more than one GOP is present — i.e. a keyframe exists after the
    /// front packet, so evicting the leading GOP still leaves a keyframe at the
    /// front. Guarantees the last GOP is never evicted.
    fn has_spare_gop(&self) -> bool {
        self.video.iter().skip(1).any(|p| p.is_keyframe)
    }

    /// Evict the oldest whole GOP: pop the leading IDR, then every following
    /// non-keyframe, so the new front is again a keyframe (`§3` "pop from the front
    /// until the front packet is an IDR"). No-op returning `false` if only one GOP
    /// remains.
    fn evict_oldest_gop(&mut self) -> bool {
        if !self.has_spare_gop() {
            return false;
        }
        if let Some(p) = self.video.pop_front() {
            self.video_bytes -= p.data.len() as u64;
        }
        while let Some(front) = self.video.front() {
            if front.is_keyframe {
                break;
            }
            let p = self.video.pop_front().expect("front just observed");
            self.video_bytes -= p.data.len() as u64;
        }
        true
    }

    /// Drop audio packets whose `pts < video_front_pts − 500 ms` (`§3`). The slack
    /// keeps audio covering any video range that can still be saved. No video → keep
    /// all audio (nothing anchors the trim yet).
    fn evict_audio(&mut self) {
        let Some(front) = self.video.front() else {
            return;
        };
        let threshold = front.pts - AUDIO_EVICTION_SLACK_MS * TICKS_PER_MILLISECOND;
        for track in &mut self.audio {
            while let Some(a) = track.front() {
                if a.pts < threshold {
                    let p = track.pop_front().expect("front just observed");
                    self.audio_bytes -= p.data.len() as u64;
                } else {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::wasapi_stream::AudioTrackKind;
    use std::sync::Arc;

    /// A video packet of `size` bytes at `pts` (ticks), keyframe or not, epoch 0.
    fn vpkt(pts: i64, dur: i64, keyframe: bool, size: usize) -> EncodedPacket {
        EncodedPacket {
            data: Arc::from(vec![0u8; size]),
            pts,
            duration: dur,
            is_keyframe: keyframe,
            epoch_id: 0,
        }
    }

    /// An audio packet of `size` bytes at `pts` (ticks).
    fn apkt(pts: i64, dur: i64, size: usize) -> EncodedAudioPacket {
        EncodedAudioPacket {
            stream: AudioTrackKind::Mix,
            data: Arc::from(vec![0u8; size]),
            pts,
            duration: dur,
        }
    }

    /// Push `gops` GOPs of `frames` frames each, frame duration `d` ticks, `size`
    /// bytes per frame, starting at pts 0. Frame 0 of each GOP is an IDR.
    fn push_gops(ring: &mut Ring, gops: usize, frames: usize, d: i64, size: usize) {
        let mut pts = 0i64;
        for _ in 0..gops {
            for f in 0..frames {
                ring.push_video(vpkt(pts, d, f == 0, size));
                pts += d;
            }
        }
    }

    /// The pts of every keyframe currently retained.
    fn keyframe_ptss(ring: &Ring) -> Vec<i64> {
        ring.video()
            .iter()
            .filter(|p| p.is_keyframe)
            .map(|p| p.pts)
            .collect()
    }

    fn caps(max_duration_ticks: i64, max_bytes: u64, tracks: usize) -> RingCaps {
        RingCaps {
            max_duration_ticks,
            max_bytes,
            num_audio_tracks: tracks,
        }
    }

    #[test]
    fn duration_cap_evicts_oldest_whole_gop() {
        // 4-frame GOPs, d = 1000 ticks → each GOP spans 4000 ticks. Cap = 2 GOPs
        // worth of span (8000). Push 4 GOPs; the two oldest evict, front stays IDR.
        let mut ring = Ring::new(caps(8_000, u64::MAX, 0));
        push_gops(&mut ring, 4, 4, 1_000, 10);
        // After the 4th GOP the span would be 16000 > 8000, so it evicts down to
        // ≤ 8000 while keeping whole GOPs. Front is always a keyframe.
        assert!(ring.duration_ticks() <= 8_000);
        assert!(ring.video().front().unwrap().is_keyframe);
        // Newest two GOPs (pts 8000, 12000) survive; oldest two (0, 4000) evicted.
        assert_eq!(keyframe_ptss(&ring), vec![8_000, 12_000]);
    }

    #[test]
    fn eviction_never_exposes_a_partial_gop() {
        // Cap tight enough to evict repeatedly; the front is a keyframe at every
        // observation across many pushes.
        let mut ring = Ring::new(caps(4_000, u64::MAX, 0));
        let (d, frames, size) = (1_000i64, 4usize, 10usize);
        let mut pts = 0i64;
        for g in 0..10 {
            for f in 0..frames {
                ring.push_video(vpkt(pts, d, f == 0, size));
                pts += d;
                if !ring.is_empty() {
                    assert!(
                        ring.video().front().unwrap().is_keyframe,
                        "front not a keyframe after GOP {g} frame {f}"
                    );
                }
            }
        }
    }

    #[test]
    fn byte_cap_pressure_still_evicts_whole_gops() {
        // Byte cap trips before the (huge) duration cap. Each frame = 100 bytes,
        // 4-frame GOP = 400 bytes. Cap = 900 bytes ⇒ at most 2 GOPs retained.
        let mut ring = Ring::new(caps(i64::MAX, 900, 0));
        push_gops(&mut ring, 5, 4, 1_000, 100);
        assert!(ring.total_bytes() <= 900);
        // Front is a keyframe (whole-GOP eviction under byte pressure).
        assert!(ring.video().front().unwrap().is_keyframe);
        // 2 GOPs × 400 bytes = 800 ≤ 900; a 3rd would be 1200 > 900.
        assert_eq!(ring.video().len(), 8);
    }

    #[test]
    fn never_evicts_the_last_gop() {
        // One GOP whose span (4000) already exceeds a tiny cap (1000): it must be
        // retained whole — a save always needs a leading IDR.
        let mut ring = Ring::new(caps(1_000, 1, 0));
        push_gops(&mut ring, 1, 4, 1_000, 100);
        assert_eq!(ring.video().len(), 4);
        assert!(ring.video().front().unwrap().is_keyframe);
        // Over both caps, but nothing evicted (no spare GOP).
        assert!(ring.duration_ticks() > 1_000);
        assert!(ring.total_bytes() > 1);
    }

    #[test]
    fn audio_evicted_below_video_front_minus_500ms() {
        // 500 ms = 5_000_000 ticks. Put the retained video front at a known pts,
        // then check audio is trimmed exactly at `front_pts − 5_000_000`.
        let slack = AUDIO_EVICTION_SLACK_MS * TICKS_PER_MILLISECOND; // 5_000_000
        let mut ring = Ring::new(caps(8_000, u64::MAX, 1));
        // Two GOPs of video with large per-frame duration so eviction advances the
        // front to a known pts. Frame d = 3_000_000 ticks, 2 frames/GOP.
        // GOP0 frames at 0, 3_000_000; GOP1 at 6_000_000, 9_000_000.
        for (pts, key) in [
            (0, true),
            (3_000_000, false),
            (6_000_000, true),
            (9_000_000, false),
        ] {
            ring.push_video(vpkt(pts, 3_000_000, key, 10));
        }
        // Duration cap 8000 forces eviction of GOP0; front becomes pts 6_000_000.
        assert_eq!(ring.video().front().unwrap().pts, 6_000_000);
        let threshold = 6_000_000 - slack; // 1_000_000
                                           // Audio straddling the threshold: pts below it evicts, at/above stays.
        for pts in (0..=2_000_000).step_by(500_000) {
            ring.push_audio(0, apkt(pts, 213_333, 5));
        }
        let remaining: Vec<i64> = ring.audio_track(0).unwrap().iter().map(|p| p.pts).collect();
        // Evict pts < 1_000_000 (0, 500_000); keep pts ≥ 1_000_000.
        assert_eq!(remaining, vec![1_000_000, 1_500_000, 2_000_000]);
        assert_eq!(threshold, 1_000_000);
    }

    #[test]
    fn audio_retained_when_no_video() {
        // With no video anchor, audio is never evicted (nothing to trim against).
        let mut ring = Ring::new(caps(8_000, u64::MAX, 1));
        for pts in (0..10_000_000).step_by(1_000_000) {
            ring.push_audio(0, apkt(pts, 213_333, 5));
        }
        assert_eq!(ring.audio_track(0).unwrap().len(), 10);
    }

    #[test]
    fn push_audio_to_missing_track_is_dropped() {
        let mut ring = Ring::new(caps(8_000, u64::MAX, 1));
        assert!(ring.push_audio(0, apkt(0, 213_333, 5)));
        assert!(!ring.push_audio(1, apkt(0, 213_333, 5))); // no track 1
        assert_eq!(ring.num_audio_tracks(), 1);
        assert_eq!(ring.total_bytes(), 5); // the dropped one did not count
    }

    #[test]
    fn byte_and_duration_accounting_track_evictions() {
        let mut ring = Ring::new(caps(8_000, u64::MAX, 1));
        push_gops(&mut ring, 4, 4, 1_000, 100); // evicts to 2 GOPs = 8 frames
        ring.push_audio(0, apkt(7_000, 213_333, 40));
        // 8 video frames × 100 + 1 audio × 40 = 840.
        assert_eq!(ring.total_bytes(), 840);
        // Span of the 2 retained GOPs: pts 8000..(15000+1000) = 8000.
        assert_eq!(ring.duration_ticks(), 8_000);
    }

    #[test]
    fn clear_empties_everything() {
        let mut ring = Ring::new(caps(8_000, u64::MAX, 2));
        push_gops(&mut ring, 2, 4, 1_000, 100);
        ring.push_audio(0, apkt(0, 213_333, 5));
        ring.push_audio(1, apkt(0, 213_333, 5));
        ring.clear();
        assert!(ring.is_empty());
        assert_eq!(ring.total_bytes(), 0);
        assert_eq!(ring.duration_ticks(), 0);
        assert_eq!(ring.audio_track(0).unwrap().len(), 0);
    }

    #[test]
    fn single_gop_under_caps_is_fully_retained() {
        let mut ring = Ring::new(caps(1_000_000, u64::MAX, 0));
        push_gops(&mut ring, 1, 4, 1_000, 100);
        assert_eq!(ring.video().len(), 4);
        assert_eq!(keyframe_ptss(&ring), vec![0]);
    }
}
