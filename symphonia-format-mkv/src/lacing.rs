// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::{HashMap, VecDeque};

use symphonia_core::errors::{Error, Result, decode_error};
use symphonia_core::io::{BufReader, ReadBytes};

use crate::demuxer::TrackState;
use crate::ebml::{read_signed_vint, read_unsigned_vint};
use crate::segment::{
    MatroskaTicks, SegmentTicks, SignedMatroskaTicks, SignedTrackTicks, TrackTicks, nanos_to_ticks,
};

enum Lacing {
    None,
    Xiph,
    FixedSize,
    Ebml,
}

fn parse_flags(flags: u8) -> Result<Lacing> {
    match (flags >> 1) & 0b11 {
        0b00 => Ok(Lacing::None),
        0b01 => Ok(Lacing::Xiph),
        0b10 => Ok(Lacing::FixedSize),
        0b11 => Ok(Lacing::Ebml),
        _ => unreachable!(),
    }
}

fn read_ebml_sizes<R: ReadBytes>(mut reader: R, num_frames: usize) -> Result<Vec<u64>> {
    let mut sizes: Vec<u64> = Vec::with_capacity(num_frames);
    for _ in 0..num_frames {
        if let Some(last_size) = sizes.last().copied() {
            let delta = read_signed_vint(&mut reader)?;

            // Do not allow the size to overflow or become negative.
            let Some(size) = last_size.checked_add_signed(delta)
            else {
                return decode_error("mkv: laced size is invalid");
            };

            sizes.push(size)
        }
        else {
            let size = read_unsigned_vint(&mut reader)?;
            sizes.push(size);
        }
    }

    Ok(sizes)
}

pub(crate) fn read_xiph_sizes<R: ReadBytes>(mut reader: R, num_frames: usize) -> Result<Vec<u64>> {
    let mut sizes = Vec::with_capacity(num_frames);
    let mut prefixes = 0;
    while sizes.len() < num_frames {
        let byte = reader.read_byte()? as u64;
        if byte == 255 {
            prefixes += 1;
        }
        else {
            let size = prefixes * 255 + byte;
            prefixes = 0;
            sizes.push(size);
        }
    }

    Ok(sizes)
}

pub(crate) struct Frame {
    /// The Matroska track number (Symphonia's track ID).
    pub(crate) track_num: u32,
    /// The frame's presentation timestamp.
    pub(crate) pts: SignedTrackTicks,
    /// The frame's duration.
    pub(crate) dur: TrackTicks,
    /// Frame data.
    pub(crate) data: Box<[u8]>,
    /// The number of decoded samples to discard from the start of the frame (negative
    /// `DiscardPadding`).
    pub(crate) trim_start: u64,
    /// The number of decoded samples to discard from the end of the frame (positive
    /// `DiscardPadding`).
    pub(crate) trim_end: u64,
}

/// Calculate the PTS of a block. This is the PTS of the first frame in the block.
///
/// The PTS is calculated in nanoseconds, so that the codec delay and the timestamp scales do not
/// lose any accuracy, and only then converted to the (possibly much finer than Segment ticks)
/// timebase of the track.
fn calculate_block_pts(
    cluster_ts: SegmentTicks,
    block_rel_ts: SignedTrackTicks,
    track: &TrackState,
) -> Option<SignedTrackTicks> {
    // Cluster and block timestamps are both in Segment ticks.
    let ticks = i128::from(cluster_ts.get()).checked_add(i128::from(block_rel_ts.get()))?;

    let mut nanos = ticks.checked_mul(i128::from(track.timestamp_scale))?;

    if track.track_timestamp_scale != 1.0 {
        nanos = (nanos as f64 * track.track_timestamp_scale).round() as i128;
    }

    // Codec delay is in nanoseconds.
    nanos = nanos.checked_sub(i128::from(track.codec_delay.get()))?;

    nanos_to_ticks(nanos, track.track_time_base).map(SignedTrackTicks::from)
}

/// Iterator-like utility to precisely compute the duration of frames in a block.
struct FrameDurationIter {
    block_dur: TrackTicks,
    num_frames: u64,
    accumulator: u64,
}

impl FrameDurationIter {
    fn new(block_dur: Option<TrackTicks>, track: &TrackState, num_frames: u64) -> Self {
        // If the block duration is known, use it. Otherwise, derive the block duration from the
        // default frame duration if it is known. Otherwise, assume a 0 duration.
        //
        // Durations are summed in nanoseconds, and only converted to track ticks after
        // multiplying to maintain as much accuracy as possible. If the multiplication overflows,
        // it won't be possible to calculate the correct duration, so don't.
        let block_dur_nanos = block_dur
            .and_then(|dur| {
                // The block duration is in Segment ticks.
                let nanos = u128::from(dur.get()).checked_mul(u128::from(track.timestamp_scale))?;

                if track.track_timestamp_scale == 1.0 {
                    Some(nanos)
                }
                else {
                    Some((nanos as f64 * track.track_timestamp_scale).round() as u128)
                }
            })
            .or_else(|| {
                track.default_frame_duration.and_then(|frame_dur| {
                    u128::from(frame_dur.get()).checked_mul(u128::from(num_frames))
                })
            });

        let block_dur = block_dur_nanos
            .and_then(|nanos| i128::try_from(nanos).ok())
            .and_then(|nanos| nanos_to_ticks(nanos, track.track_time_base))
            .map(|ticks| TrackTicks::from(ticks.unsigned_abs()))
            .unwrap_or_default();

        FrameDurationIter { block_dur, num_frames, accumulator: 0 }
    }

    fn next(&mut self) -> TrackTicks {
        // Accumulate the remainder after each computation since integer division rounds down.
        self.accumulator = self.accumulator.saturating_add(self.block_dur.get());
        let dur = TrackTicks::from(self.accumulator / self.num_frames);
        self.accumulator %= self.num_frames;
        dur
    }
}

pub(crate) fn extract_frames(
    block: &[u8],
    block_duration: Option<TrackTicks>,
    discard_padding: Option<SignedMatroskaTicks>,
    cluster_ts: SegmentTicks,
    tracks: &HashMap<u32, TrackState>,
    frames: &mut VecDeque<Frame>,
) -> Result<bool> {
    let mut reader = BufReader::new(block);
    let track_num = read_unsigned_vint(&mut reader)? as u32;
    let block_rel_ts = SignedTrackTicks::from((reader.read_be_u16()? as i16) as i64);
    let flags = reader.read_byte()?;
    let lacing = parse_flags(flags)?;

    // Get the track associated with the block. It's an error if the track doesn't exist.
    let track =
        tracks.get(&track_num).ok_or(Error::DecodeError("mkv: unvalid track number for block"))?;

    let mut pts = match calculate_block_pts(cluster_ts, block_rel_ts, track) {
        Some(pts) => track.snap_pts(pts),
        _ => return Ok(false),
    };
    let first_frame = frames.len();

    // `DiscardPadding` is a duration (in nanoseconds) of padding at the end of the block (if
    // positive) or at the start of the block (if negative). Convert it into an exact number of
    // samples. This requires knowing the sample rate of the (audio) track.
    let (padding_start, padding_end) = match (discard_padding, track.sample_rate) {
        (Some(padding), Some(sample_rate)) if padding.get() != 0 => {
            let samples =
                MatroskaTicks::from(padding.get().unsigned_abs()).into_samples(sample_rate);
            if padding.get() < 0 { (samples, 0) } else { (0, samples) }
        }
        _ => (0, 0),
    };

    match lacing {
        Lacing::None => {
            let data = reader.read_boxed_slice_exact(block.len() - reader.pos() as usize)?;
            let mut dur = FrameDurationIter::new(block_duration, track, 1).next();

            // PCM blocks have a duration that can be calculated exactly from their size if it is
            // not otherwise specified.
            if dur.get() == 0 {
                if let Some(pcm_dur) = track.pcm_duration(data.len()) {
                    dur = pcm_dur;
                }
            }
            frames.push_back(Frame { track_num, pts, data, dur, trim_start: 0, trim_end: 0 });
        }
        Lacing::Xiph | Lacing::Ebml => {
            // Read number of stored sizes which is actually `number of frames` - 1
            // since size of the last frame is deduced from block size.
            let num_frames = reader.read_byte()? as usize;
            let sizes = match lacing {
                Lacing::Xiph => read_xiph_sizes(&mut reader, num_frames)?,
                Lacing::Ebml => read_ebml_sizes(&mut reader, num_frames)?,
                _ => unreachable!(),
            };

            // The total of all decoded frame sizes should not exceed the block they will be read
            // from.
            let total_laced_size = sizes.iter().try_fold(0u64, |acc, &size| acc.checked_add(size));

            match total_laced_size {
                Some(size) if size <= block.len() as u64 => (),
                _ => return decode_error("mkv: total of laced frame sizes exceeds block"),
            }

            let mut dur_it = FrameDurationIter::new(block_duration, track, num_frames as u64 + 1);

            for frame_size in sizes {
                let data = reader.read_boxed_slice_exact(frame_size as usize)?;
                let dur = dur_it.next();

                frames.push_back(Frame { track_num, pts, data, dur, trim_start: 0, trim_end: 0 });

                // If PTS overflows, end the stream.
                pts = match pts.checked_add_unsigned(dur) {
                    Some(pts) => pts,
                    None => return Ok(false),
                };
            }

            // Size of last frame is not provided so we read to the end of the block.
            let size = block.len() - reader.pos() as usize;
            let data = reader.read_boxed_slice_exact(size)?;
            frames.push_back(Frame {
                track_num,
                pts,
                data,
                dur: dur_it.next(),
                trim_start: 0,
                trim_end: 0,
            });
        }
        Lacing::FixedSize => {
            let num_frames = reader.read_byte()? as usize + 1;
            let total_size = block.len() - reader.pos() as usize;
            if total_size % num_frames != 0 {
                return decode_error("mkv: invalid block size");
            }

            let mut dur_it = FrameDurationIter::new(block_duration, track, num_frames as u64);

            let frame_size = total_size / num_frames;
            for _ in 0..num_frames {
                let data = reader.read_boxed_slice_exact(frame_size)?;
                let dur = dur_it.next();

                frames.push_back(Frame { track_num, pts, data, dur, trim_start: 0, trim_end: 0 });

                // If PTS overflows, end the stream.
                pts = match pts.checked_add_unsigned(dur) {
                    Some(pts) => pts,
                    None => return Ok(false),
                };
            }
        }
    }

    // Apply the block's `DiscardPadding` to the first (negative padding) or last (positive
    // padding) frame of the block. The padding is part of the decoded frames, but not of the
    // frames to be presented, so the duration of the frame (the duration of the presented frames)
    // is the duration of the decoded frame less the padding.
    if frames.len() > first_frame && (padding_start != 0 || padding_end != 0) {
        // The duration of a decoded frame is the default frame duration, if there is one. Unlike
        // an explicit block duration, it is not rounded to the precision of the timestamps.
        let decoded_dur = track
            .default_frame_duration
            .map(|dur| i128::from(dur.get()))
            .and_then(|nanos| nanos_to_ticks(nanos, track.track_time_base))
            .map(|ticks| TrackTicks::from(ticks.unsigned_abs()));

        let last_frame = frames.len() - 1;

        for index in [first_frame, last_frame] {
            if let Some(dur) = decoded_dur {
                frames[index].dur = dur;
            }
        }

        // If the decoded duration is not known, an explicit block duration is assumed to already
        // exclude the padding (as written by muxers), but a duration that was derived from the
        // default frame duration cannot.
        if decoded_dur.is_some() || block_duration.is_none() {
            let first = &mut frames[first_frame];
            first.dur = TrackTicks::from(first.dur.get().saturating_sub(padding_start));

            let last = &mut frames[last_frame];
            last.dur = TrackTicks::from(last.dur.get().saturating_sub(padding_end));
        }

        frames[first_frame].trim_start = padding_start;
        frames[last_frame].trim_end = padding_end;
    }

    Ok(true)
}
