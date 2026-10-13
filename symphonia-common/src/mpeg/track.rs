// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Track construction, tail scanning, and seeking shared by the MPEG program stream, transport
//! stream, and FLV demuxers.

use symphonia_core::codecs::CodecParameters;
use symphonia_core::errors::{Error, Result, SeekErrorKind, seek_error};
use symphonia_core::formats::prelude::*;
use symphonia_core::units::{Duration, TimeBase, Timestamp};

use super::es::{EsInfo, EsTrack};
use super::pes::{PTS_MODULUS, pts_delta, unwrap_pts};
use super::seek::bisect;
use super::timeline::ticks_to_clock;

/// The maximum number of bytes at the end of the stream to scan for the last PES packets of the
/// streams, to find the duration.
pub const MAX_TAIL_WINDOW: u64 = 64 * 1024 * 1024;

/// The range, in bytes, below which the bisection stops, and the stream is scanned forward.
pub const BISECT_MIN_GAP: u64 = 64 * 1024;

/// The number of bytes to scan for a PES packet with a PTS when probing during a seek.
pub const PROBE_SCAN_LEN: u64 = 512 * 1024;

/// The initial size of the window at the end of the stream that is scanned for the last PES
/// packets.
const INITIAL_TAIL_WINDOW: u64 = 1024 * 1024;

/// Returns the (raw) PTS that is time 0 of the timeline: the earliest first PTS of the streams,
/// adjusted so that the first sample of a stream that is not trimmed has the timestamp 0 for a
/// single stream. `streams` are the elementary streams and their first (raw) PTS.
pub fn earliest_base<'a>(
    streams: impl IntoIterator<Item = (&'a EsTrack, Option<u64>)>,
) -> Option<u64> {
    let mut base: Option<u64> = None;

    for (es, first_pts) in streams {
        let Some(pts) = first_pts
        else {
            continue;
        };

        let rate = es.info().map_or(1, |i| i.rate);
        let lead = ticks_to_clock(es.lead_in().unwrap_or(0) as i64, rate);
        let pts = (pts + lead as u64) % PTS_MODULUS;

        base = Some(match base {
            Some(b) if pts_delta(pts, b) >= 0 => b,
            _ => pts,
        });
    }

    base
}

/// Set the duration of `track` of `dur` ticks of its timebase, and the number of frames.
pub fn set_track_duration(track: &mut Track, info: &EsInfo, dur: u64) {
    let out_rate = info.params.sample_rate.unwrap_or(info.rate);

    track.with_duration(Duration::new(dur));
    track.with_num_frames(dur * u64::from(out_rate) / u64::from(info.rate));
}

/// Create the track `id` of the elementary stream `es`, the time base of which is the sample rate
/// of the stream. The stream must have been probed, and its timeline initialised with `base`.
/// `first_pts` is the (raw) PTS of the first PES packet of the stream, and `end_ts` is the end of
/// the stream in the timebase of the track, if known.
pub fn build_audio_track(
    id: u32,
    es: &EsTrack,
    first_pts: Option<u64>,
    base: Option<u64>,
    end_ts: Option<i64>,
    invalid_rate_msg: &'static str,
) -> Result<Track> {
    let info = es.info().expect("stream was probed");
    let rate = info.rate;

    let mut track = Track::new(id);

    track.with_time_base(
        TimeBase::try_from_recip(rate).ok_or(Error::DecodeError(invalid_rate_msg))?,
    );
    track.with_codec_params(CodecParameters::Audio(info.params.clone()));

    // The timestamp of the first sample that is not trimmed.
    let lead_in = es.lead_in().unwrap_or(0) as i64;

    let start_ts = match (first_pts, base) {
        (Some(pts), Some(base)) => es.timeline().ticks_for_clock(pts_delta(pts, base)) + lead_in,
        _ => 0,
    };

    if lead_in > 0 {
        track.with_delay(lead_in as u32);
    }

    track.with_start_ts(Timestamp::new(start_ts));

    if let Some(end_ts) = end_ts {
        if end_ts > start_ts {
            set_track_duration(&mut track, info, (end_ts - start_ts) as u64);
        }
    }

    Ok(track)
}

/// The PTS and the payloads of the last PES packet with a PTS of a stream, and of those after it
/// (which may complete its last frame).
#[derive(Clone, Default)]
pub struct LastPes(Option<(u64, Vec<Vec<u8>>)>);

impl LastPes {
    /// Record a PES packet, with the (raw) PTS `pts` if it has one, and the payload `payload`.
    pub fn record(&mut self, pts: Option<u64>, payload: impl Into<Vec<u8>>) {
        match (pts, self.0.as_mut()) {
            (Some(pts), _) => self.0 = Some((pts, vec![payload.into()])),
            (None, Some((_, data))) => data.push(payload.into()),
            (None, None) => (),
        }
    }

    /// Returns true if a PES packet with a PTS was recorded.
    pub fn is_some(&self) -> bool {
        self.0.is_some()
    }

    /// The end timestamp of the stream `es` in its timebase, from the last PES packets. `base` is
    /// the (raw) PTS that is time 0 of the timeline.
    pub fn end_ts(&self, es: &EsTrack, base: u64) -> Option<i64> {
        let (pts, payloads) = self.0.as_ref()?;
        let clock = pts_delta(*pts, base);

        Some(es.timeline().ticks_for_clock(clock) + es.payload_duration(payloads) as i64)
    }
}

/// The end timestamp, in the timebase of each stream, of the streams `es`, from the last PES
/// packets, `last`, found by [`scan_tail`].
pub fn tail_ends<'a>(
    last: &[LastPes],
    es: impl IntoIterator<Item = &'a EsTrack>,
    base: Option<u64>,
) -> Vec<Option<i64>> {
    let mut ends = vec![None; last.len()];

    if let Some(base) = base {
        for ((end, l), es) in ends.iter_mut().zip(last).zip(es) {
            *end = l.end_ts(es, base);
        }
    }

    ends
}

/// Scan windows of increasing size at the end of a stream of `len` bytes, the first packet of
/// which is at `start_pos`, for the last PES packet with a PTS of each of `streams` streams.
///
/// `read_window(begin, last)` must read the PES packets from the position `begin` to the end of
/// the stream, record them in `last` (indexed by stream), and return the position at which it
/// began reading (which may be after `begin`, if it synchronised to the stream). The windows grow
/// until the PES packets of all of the streams are found, the start of the stream is reached, or
/// the window is [`MAX_TAIL_WINDOW`].
pub fn scan_tail<E>(
    len: u64,
    start_pos: u64,
    streams: usize,
    mut read_window: impl FnMut(u64, &mut [LastPes]) -> std::result::Result<u64, E>,
) -> std::result::Result<Vec<LastPes>, E> {
    let mut window = INITIAL_TAIL_WINDOW;

    loop {
        let begin = len.saturating_sub(window).max(start_pos);

        let mut last = vec![LastPes::default(); streams];
        let from = read_window(begin, &mut last)?;

        if last.iter().all(LastPes::is_some) || from <= start_pos || window >= MAX_TAIL_WINDOW {
            return Ok(last);
        }

        window = window.saturating_mul(8);
    }
}

/// The parameters of a seek, resolved from the request.
pub struct SeekPlan {
    /// The index of the track.
    pub track_idx: usize,
    pub track_id: u32,
    /// The timestamp requested, in the timebase of the track.
    pub required_ts: Timestamp,
    /// The timestamp to start decoding from, so that a decoder reproduces a continuous decode at
    /// the required timestamp.
    pub start_ts: i64,
    /// The sample rate of the track.
    pub rate: u32,
}

/// Resolve a seek request to a track, and the timestamps to seek to. `es` are the elementary
/// streams, one per track in the order of `tracks`.
pub fn plan_seek<'a>(
    tracks: &[Track],
    es: impl IntoIterator<Item = &'a EsTrack>,
    to: SeekTo,
) -> Result<SeekPlan> {
    let (track_idx, required_ts) = match to {
        SeekTo::Timestamp { ts, track_id } => match tracks.iter().position(|t| t.id == track_id) {
            Some(idx) => (idx, ts),
            None => return seek_error(SeekErrorKind::Unseekable),
        },
        SeekTo::Time { time, track_id } => {
            let idx = match track_id {
                Some(id) => tracks.iter().position(|t| t.id == id),
                None => Some(0),
            };

            let Some(idx) = idx
            else {
                return seek_error(SeekErrorKind::Unseekable);
            };

            let tb = tracks[idx].time_base.ok_or(Error::SeekError(SeekErrorKind::Unseekable))?;
            let ts = tb.calc_timestamp(time).ok_or(Error::SeekError(SeekErrorKind::OutOfRange))?;

            (idx, ts)
        }
    };

    let track = &tracks[track_idx];
    let track_start = track.start_ts;

    let Some(es) = es.into_iter().nth(track_idx)
    else {
        return seek_error(SeekErrorKind::Unseekable);
    };

    if required_ts.is_negative() {
        return seek_error(SeekErrorKind::OutOfRange);
    }

    // The timestamp must be within the duration, if it is known.
    if let Some(dur) = track.duration {
        if required_ts.get() > track_start.get().saturating_add(dur.get() as i64) {
            return seek_error(SeekErrorKind::OutOfRange);
        }
    }

    // The timestamp to start decoding from, so that a decoder reproduces a continuous decode at
    // the required timestamp.
    let start_ts = es.parser().seek_start_ts(required_ts.get()).max(track_start.get());

    Ok(SeekPlan {
        track_idx,
        track_id: track.id,
        required_ts,
        start_ts,
        rate: es.info().map_or(1, |i| i.rate),
    })
}

/// Search the byte range `lo..hi` of a stream for the last position at which a PES packet with a
/// PTS of at most `target_clock` (90 kHz clock ticks since the raw PTS `base`) begins. See
/// [`bisect`]. `probe(pos)` must return the position and the raw PTS of the first PES packet with
/// a PTS at or after `pos`.
pub fn bisect_pts<E>(
    lo: u64,
    hi: u64,
    base: u64,
    target_clock: i64,
    mut probe: impl FnMut(u64) -> std::result::Result<Option<(u64, u64)>, E>,
) -> std::result::Result<u64, E> {
    bisect(lo, hi, target_clock, BISECT_MIN_GAP, |pos| {
        Ok(probe(pos)?.map(|(pos, pts)| (pos, unwrap_pts(pts, base, target_clock))))
    })
}

/// If a stream cannot be seeked, a seek is only possible forward from the last packet returned.
pub fn check_forward_only(
    last_ts: Option<(u32, i64)>,
    track_id: u32,
    required_ts: Timestamp,
) -> Result<()> {
    if last_ts.is_some_and(|(id, ts)| id == track_id && required_ts.get() < ts) {
        return seek_error(SeekErrorKind::ForwardOnly);
    }

    Ok(())
}

/// Read packets with `next` to the first packet of the track `track_id` that ends after the
/// timestamp `start_ts`. Returns an out of range error if there is none.
pub fn find_seek_packet(
    mut next: impl FnMut() -> Result<Option<Packet>>,
    track_id: u32,
    start_ts: i64,
) -> Result<Packet> {
    loop {
        match next()? {
            Some(packet)
                if packet.track_id == track_id
                    && packet.pts.get().saturating_add(packet.dur.get() as i64) > start_ts =>
            {
                return Ok(packet);
            }
            Some(_) => (),
            None => return seek_error(SeekErrorKind::OutOfRange),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_scan_tail_grows_until_all_streams_are_found() {
        // Stream 1 has a PES packet only in the first 3 MiB of a 100 MiB stream.
        let len = 100 * 1024 * 1024u64;
        let mut windows = vec![];

        let last = scan_tail::<()>(len, 0, 2, |begin, last| {
            windows.push(begin);
            last[0].record(Some(1), &[1u8][..]);

            if len - begin >= 8 * 1024 * 1024 {
                last[1].record(Some(2), &[2u8][..]);
            }

            Ok(begin)
        })
        .unwrap();

        assert_eq!(windows, [len - (1 << 20), len - (8 << 20)]);
        assert!(last.iter().all(LastPes::is_some));
    }

    #[test]
    fn verify_last_pes_keeps_payloads_after_the_last_pts() {
        let mut last = LastPes::default();

        last.record(None, &[0u8][..]);
        assert!(!last.is_some());

        last.record(Some(10), &[1u8][..]);
        last.record(None, vec![2u8]);
        last.record(Some(20), &[3u8][..]);
        last.record(None, &[4u8][..]);

        let (pts, payloads) = last.0.unwrap();
        assert_eq!(pts, 20);
        assert_eq!(payloads, [vec![3u8], vec![4u8]]);
    }
}
