// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::codecs::CodecParameters;
use symphonia_core::support_format;

use symphonia_core::codecs::audio::well_known::{
    CODEC_ID_AAC, CODEC_ID_ALAC, CODEC_ID_FLAC, CODEC_ID_OPUS, CODEC_ID_TRUEHD,
};
use symphonia_core::errors::{
    Error, Result, SeekErrorKind, decode_error, seek_error, unsupported_error,
};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::formats::well_known::FORMAT_ID_ISOMP4;
use symphonia_core::io::*;
use symphonia_core::meta::{ChapterGroup, Metadata, MetadataBuilder, MetadataLog};
use symphonia_core::units::Time;

use std::collections::HashMap;
use std::io::{Seek, SeekFrom};
use std::num::NonZero;
use std::sync::Arc;

use crate::atoms::chpl::make_chapter_group;
use crate::atoms::ilst::ISOMP4_METADATA_INFO;
use crate::atoms::qt_meta::decode_text;
use crate::atoms::{AtomError, AtomIterator, AtomType, ReadAtom};
use crate::atoms::{FtypAtom, GaplessInfo, MetaAtom, MoofAtom, MoovAtom, SidxAtom, TrakAtom};
use symphonia_common::mpeg::audio::{
    AudioSpecificConfig, aac_overlap_frames, aac_sbr_phase_period, aac_seek_max_preroll_frames,
    aac_seek_start_frame_with_period,
};

use crate::stream::*;

use log::{debug, info, trace, warn};

/// Opus seek pre-roll in frames at 48 kHz. The packets are not inspected (the sample table is
/// all that is known when seeking), so this is the pre-roll of a CELT-only stream, 1.5 s, which
/// brings a reset decoder to a bit-identical continuous decode (RFC 7845 section 4.6 only
/// mandates 80 ms). See `symphonia_common::xiph::audio::opus::SEEK_PREROLL_CELT`.
const OPUS_SEEK_PREROLL_FRAMES: u64 = symphonia_common::xiph::audio::opus::SEEK_PREROLL_CELT;

/// TrueHD seek pre-roll in samples (one access unit each). Decoding can only start at an access
/// unit with a major sync, which encoders write at most 128 access units apart (FFmpeg
/// `mlpenc.c`, `MAX_HEADER_INTERVAL`), so a major sync is always found before the target.
const TRUEHD_SEEK_PREROLL_PACKETS: u32 = 128;

const ISOMP4_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FORMAT_ID_ISOMP4,
    short_name: "isomp4",
    long_name: "ISO Base Media File Format",
};

pub struct TrackState {
    /// The track number.
    track_num: usize,
    /// The track ID.
    track_id: u32,
    /// The current segment.
    cur_seg: usize,
    /// The current sample index relative to the track.
    next_sample: u32,
    /// The current sample byte position relative to the start of the track.
    next_sample_pos: u64,
    /// The number of decoded audio frames per tick of the track's timebase. This is 1 for almost
    /// all audio tracks. For HE-AAC (SBR), the timescale of a track may be the core codec rate
    /// while the decoded output has twice the sample rate.
    frames_per_tick: u64,
    /// The number of decoded frames at the start of the timeline of the track that are silent
    /// because of an initial empty edit of the edit list.
    start_offset: u64,
    /// The time base of the media (sample table) timestamps, which differs from the time base of
    /// the track if `frames_per_tick` is not 1.
    media_time_base: TimeBase,
    /// The nominal duration of a sample in the sample table: the most common one.
    stts_nominal: Option<u64>,
    /// How to pre-roll the decoder after a seek, so that the decoder's internal state
    /// converges before the seek target.
    seek_preroll: SeekPreroll,
}

impl TrackState {
    pub fn make(
        track_num: usize,
        trak: &TrakAtom,
        timescale: NonZero<u32>,
        duration: Option<Duration>,
        movie_timescale: NonZero<u32>,
        itunes_gapless: Option<GaplessInfo>,
    ) -> (Self, Track) {
        let mut track = Track::new(trak.tkhd.id);

        // Create the codec parameters using the sample description atom.
        if let Some(codec_params) = trak.mdia.minf.stbl.stsd.make_codec_params() {
            track.with_codec_params(codec_params);
        }

        // Populate timing information. This is revised below if the number of decoded frames per
        // tick of the media timescale is not 1.
        track.with_time_base(TimeBase::from_recip(timescale));

        if let Some(duration) = duration {
            track.with_duration(duration);
        }

        // If the track is an audio track, and the timescale is equal to the sample rate, then the
        // number of frames is equal to the duration. This is the case for almost all audio tracks.
        // If not, there is no generic, low overhead, & precise way to determine the number of
        // frames.
        //
        // The exception is an AAC track with SBR whose timescale is the core codec rate: the
        // sample rate of its codec parameters is the (integer multiple) SBR output rate, and each
        // tick of the timescale is that many decoded frames.
        let frames_per_tick = match &track.codec_params {
            Some(CodecParameters::Audio(audio)) => match audio.sample_rate {
                Some(rate) if rate == timescale.get() => Some(1),
                Some(rate) if audio.codec == CODEC_ID_AAC && rate % timescale.get() == 0 => {
                    Some(u64::from(rate / timescale.get()))
                }
                _ => None,
            },
            _ => None,
        };

        let mut start_offset = 0;

        if let Some(frames_per_tick) = frames_per_tick {
            // The total number of frames, including any delay and padding, if the duration of the
            // track is known. The duration of a fragmented stream may be unknown.
            let total_frames = duration.map(|dur| dur.get().saturating_mul(frames_per_tick));

            // If a tick of the media timescale is more than one decoded frame (e.g., HE-AAC, where
            // the media timescale is the sample rate of the core codec), express the timeline of
            // the track in decoded frames: the same unit as the delay, padding, number of frames,
            // and the trim of packets.
            if frames_per_tick > 1 {
                if let Some(timescale) = u32::try_from(frames_per_tick)
                    .ok()
                    .and_then(|ratio| timescale.get().checked_mul(ratio))
                    .and_then(NonZero::new)
                {
                    track.with_time_base(TimeBase::from_recip(timescale));

                    if let Some(total_frames) = total_frames {
                        track.with_duration(Duration::new(total_frames));
                    }
                }
            }

            if let Some(total_frames) = total_frames {
                track.with_num_frames(total_frames);
            }

            // Derive the timing of the edit list, if any: the initial empty edit (the offset of
            // the start of the track), and the encoder delay and padding.
            let elst_timing =
                trak.edts.as_ref().and_then(|edts| edts.elst.as_ref()).and_then(|elst| {
                    derive_timing_from_elst(
                        elst,
                        duration.map(Duration::get),
                        timescale,
                        movie_timescale,
                    )
                });

            // An iTunes `iTunSMPB` freeform tag, if present, takes precedence over the edit list
            // for the delay and padding since it is the more explicit and widely honoured source
            // (matching common player behaviour). Both are expressed in media timescale ticks,
            // whereas the delay and padding of a track are decoded frames.
            let gapless = itunes_gapless
                .map(|gapless| (u64::from(gapless.delay), u64::from(gapless.padding)))
                .or(elst_timing.map(|timing| (timing.delay, timing.padding)))
                .and_then(|(delay, padding)| {
                    let delay = u32::try_from(delay.checked_mul(frames_per_tick)?).ok()?;
                    let padding = u32::try_from(padding.checked_mul(frames_per_tick)?).ok()?;
                    Some(GaplessInfo { delay, padding })
                });

            if let Some(gapless) = gapless {
                if gapless.delay > 0 {
                    track.with_delay(gapless.delay);
                }
                if gapless.padding > 0 {
                    track.with_padding(gapless.padding);
                }

                // Reduce the reported number of frames to exclude delay/padding, matching the
                // convention used by other demuxers (e.g., mp3, caf).
                if let Some(total_frames) = total_frames {
                    let discard = u64::from(gapless.delay) + u64::from(gapless.padding);
                    track.with_num_frames(total_frames.saturating_sub(discard));
                }
            }

            if let Some(timing) = elst_timing {
                start_offset = timing.start_offset.saturating_mul(frames_per_tick);

                if let Some(start_ts) = i64::try_from(start_offset).ok().map(Timestamp::new) {
                    track.with_start_ts(start_ts);
                }
            }
        }

        let state = Self {
            track_num,
            track_id: trak.tkhd.id,
            cur_seg: 0,
            next_sample: 0,
            next_sample_pos: 0,
            frames_per_tick: frames_per_tick.unwrap_or(1),
            start_offset,
            media_time_base: TimeBase::from_recip(timescale),
            stts_nominal: nominal_sample_delta(trak),
            seek_preroll: SeekPreroll::new(&track, nominal_sample_delta(trak), timescale),
        };

        (state, track)
    }
}

/// How to pre-roll a decoder after a seek.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum SeekPreroll {
    /// Start decoding at the sample containing the seek target.
    None,
    /// AAC. Start decoding some samples (packets) before the target. See
    /// [`aac_seek_start_frame_with_period`]. `overlap` is the number of previous frames the
    /// output of a frame depends on, and `period` the period of the phase of SBR in frames.
    Aac { sbr: bool, overlap: u32, period: u32 },
    /// Start decoding a fixed number of packets before the target. Opus: covering at least
    /// [`OPUS_SEEK_PREROLL_FRAMES`] (1.5 s at 48 kHz). TrueHD: [`TRUEHD_SEEK_PREROLL_PACKETS`].
    Packets { packets: u32 },
}

impl SeekPreroll {
    fn new(track: &Track, nominal_dur: Option<u64>, timescale: NonZero<u32>) -> Self {
        match &track.codec_params {
            Some(CodecParameters::Audio(audio)) if audio.codec == CODEC_ID_AAC => {
                let asc = audio
                    .extra_data
                    .as_deref()
                    .and_then(|extra_data| AudioSpecificConfig::read(extra_data).ok());

                let sbr = asc.as_ref().is_some_and(|asc| asc.sbr_present);
                let overlap = asc.as_ref().map_or(1, |asc| {
                    u32::try_from(aac_overlap_frames(asc.object_type)).unwrap_or(1)
                });

                let period = asc
                    .as_ref()
                    .map_or(16, |asc| u32::try_from(aac_sbr_phase_period(asc)).unwrap_or(16));

                SeekPreroll::Aac { sbr, overlap, period }
            }
            Some(CodecParameters::Audio(audio)) if audio.codec == CODEC_ID_OPUS => {
                // The pre-roll does not depend on the OpusHead pre-skip (which is excluded from
                // the seek timeline). Convert to packets using the nominal packet duration (in
                // ticks of the media timescale), assuming 20 ms packets if unknown.
                let ticks =
                    (u64::from(timescale.get()) * OPUS_SEEK_PREROLL_FRAMES).div_ceil(48_000);
                let nominal = nominal_dur.filter(|&d| d > 0).unwrap_or(ticks.div_ceil(4).max(1));
                let packets = ticks.div_ceil(nominal).min(u64::from(u16::MAX));
                SeekPreroll::Packets { packets: packets as u32 }
            }
            Some(CodecParameters::Audio(audio)) if audio.codec == CODEC_ID_TRUEHD => {
                SeekPreroll::Packets { packets: TRUEHD_SEEK_PREROLL_PACKETS }
            }
            _ => SeekPreroll::None,
        }
    }

    /// Get the number of the sample to start decoding from to reach `target`, where `from_start`
    /// is true if the segment containing the target starts at the beginning of the track.
    fn start_sample(self, target: u32, from_start: bool) -> u32 {
        match self {
            SeekPreroll::None => target,
            SeekPreroll::Packets { packets } => target.saturating_sub(packets),
            // The state of an SBR decoder is a function of the frames since the start of the
            // stream (the noise phase), so audio is only correct if decoding starts at a frame
            // from which a reset decoder converges. Therefore, an SBR stream is pre-rolled
            // regardless of the seek mode.
            //
            // The pre-roll is aligned to the stream start, which is only known for a segment
            // that starts the track. Otherwise, use the maximum pre-roll.
            SeekPreroll::Aac { sbr: true, period, .. } if !from_start => target.saturating_sub(
                u32::try_from(aac_seek_max_preroll_frames(u64::from(period))).unwrap_or(0),
            ),
            SeekPreroll::Aac { sbr, overlap, period } => {
                u32::try_from(aac_seek_start_frame_with_period(
                    u64::from(target),
                    sbr,
                    u64::from(overlap),
                    u64::from(period),
                ))
                .unwrap_or(target)
            }
        }
    }
}

/// The timing of a track derived from its edit list, in ticks of the media timescale.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct ElstTiming {
    /// The duration of the initial empty edit: the offset of the start of the track on the
    /// presentation timeline.
    start_offset: u64,
    /// The media time of the start of the first edit: the number of leading (encoder delay)
    /// ticks that are not presented.
    delay: u64,
    /// The number of trailing (encoder padding) ticks that are not presented. Zero if the total
    /// duration of the media is not known.
    padding: u64,
}

/// Derive the timing of an audio track from its edit list (ISO/IEC 14496-12, 8.6.6).
///
/// The edit list is interpreted as an optional initial empty edit (`media_time == -1`),
/// followed by one or more edits at the normal rate, that together present a contiguous range of
/// the media. A trailing empty edit is ignored. Returns `None` for edit lists that do not have
/// this form (e.g., edits that skip or repeat parts of the media, or play at another rate), which
/// are not handled.
///
/// `segment_duration` is in the movie timescale, whereas the media time is in the media timescale.
/// If `total_media_ticks`, the duration of the media, is not known, no padding is derived. An edit
/// with a segment duration of 0 extends to the end of the media.
fn derive_timing_from_elst(
    elst: &crate::atoms::ElstAtom,
    total_media_ticks: Option<u64>,
    media_timescale: NonZero<u32>,
    movie_timescale: NonZero<u32>,
) -> Option<ElstTiming> {
    let to_media = |segment_duration: u64| {
        let ticks = (u128::from(segment_duration) * u128::from(media_timescale.get()))
            / u128::from(movie_timescale.get());
        u64::try_from(ticks).ok()
    };

    let mut entries = elst.entries.iter().peekable();

    // Leading empty edits.
    let mut start_offset = 0u64;

    while let Some(entry) = entries.next_if(|entry| entry.media_time == -1) {
        start_offset = start_offset.checked_add(to_media(entry.segment_duration)?)?;
    }

    let first = entries.peek()?;
    let delay = u64::try_from(first.media_time).ok()?;

    // The media time of the end of the presented range, if bounded.
    let mut end = Some(delay);

    for entry in entries {
        // Ignore trailing empty edits.
        if entry.media_time == -1 {
            break;
        }

        // Only the normal rate is supported.
        if entry.media_rate_int != 1 || entry.media_rate_frac != 0 {
            return None;
        }

        // Edits must be contiguous.
        let end_time = end?;

        if u64::try_from(entry.media_time).ok()? != end_time {
            return None;
        }

        end = match entry.segment_duration {
            0 => None,
            dur => end_time.checked_add(to_media(dur)?),
        };
    }

    let padding = match (end, total_media_ticks) {
        (Some(end), Some(total)) => total.saturating_sub(end),
        _ => 0,
    };

    Some(ElstTiming { start_offset, delay, padding })
}

/// Gets the nominal duration of a sample in the sample table of a track: the most common one.
fn nominal_sample_delta(trak: &TrakAtom) -> Option<u64> {
    trak.mdia
        .minf
        .stbl
        .stts
        .entries
        .iter()
        .max_by_key(|entry| entry.sample_count)
        .map(|entry| u64::from(entry.sample_delta))
}

/// The number of leading samples of a track that may be shortened to signal an encoder delay.
const MAX_SHORTENED_LEADING_SAMPLES: u32 = 4;

/// Gets if the decoder of a track outputs exactly the number of frames that is signalled in the
/// bitstream of each packet, regardless of the duration in the sample table.
fn decoder_signals_frame_count(params: &Option<CodecParameters>) -> bool {
    match params {
        Some(CodecParameters::Audio(audio)) => {
            audio.codec == CODEC_ID_ALAC || audio.codec == CODEC_ID_FLAC
        }
        _ => false,
    }
}

/// Gets the duration of the full block of frames a packet decodes to. This is the nominal sample
/// duration of the sample table if the packet is a shortened (final) one and the decoder does not
/// signal the number of frames in the bitstream, otherwise it is the duration of the packet.
fn nominal_packet_dur(
    signals_frame_count: bool,
    stts_nominal: Option<u64>,
    dur: Duration,
) -> Duration {
    if signals_frame_count {
        return dur;
    }

    stts_nominal.filter(|&nominal| nominal > dur.get()).map(Duration::new).unwrap_or(dur)
}

#[cfg(test)]
mod opus_preroll_tests {
    use super::{OPUS_SEEK_PREROLL_FRAMES, SeekPreroll};
    use std::num::NonZero;
    use symphonia_core::codecs::CodecParameters;
    use symphonia_core::codecs::audio::AudioCodecParameters;
    use symphonia_core::codecs::audio::well_known::CODEC_ID_OPUS;
    use symphonia_core::formats::Track;

    fn opus_track() -> Track {
        let mut params = AudioCodecParameters::new();
        params.for_codec(CODEC_ID_OPUS);
        let mut track = Track::new(0);
        track.with_codec_params(CodecParameters::Audio(params));
        track
    }

    #[test]
    fn verify_opus_preroll_covers_1_5s() {
        assert_eq!(OPUS_SEEK_PREROLL_FRAMES, 72_000);
        let ts = NonZero::new(48_000).unwrap();

        // 20 ms packets: 75 packets.
        let p = SeekPreroll::new(&opus_track(), Some(960), ts);
        assert_eq!(p, SeekPreroll::Packets { packets: 75 });
        assert_eq!(p.start_sample(100, false), 25);
        assert_eq!(p.start_sample(2, true), 0);

        // 60 ms packets: 25 packets (1.5 s).
        assert_eq!(
            SeekPreroll::new(&opus_track(), Some(2880), ts),
            SeekPreroll::Packets { packets: 25 }
        );
        // 10 ms packets: 150 packets.
        assert_eq!(
            SeekPreroll::new(&opus_track(), Some(480), ts),
            SeekPreroll::Packets { packets: 150 }
        );
    }
}

#[cfg(test)]
mod packet_dur_tests {
    use super::{decoder_signals_frame_count, nominal_packet_dur};
    use symphonia_core::codecs::CodecParameters;
    use symphonia_core::codecs::audio::AudioCodecParameters;
    use symphonia_core::codecs::audio::well_known::{CODEC_ID_AAC, CODEC_ID_ALAC, CODEC_ID_FLAC};
    use symphonia_core::units::Duration;

    fn params(codec: symphonia_core::codecs::audio::AudioCodecId) -> Option<CodecParameters> {
        let mut audio = AudioCodecParameters::new();
        audio.for_codec(codec);
        Some(CodecParameters::Audio(audio))
    }

    #[test]
    fn shortened_final_packet_of_fixed_size_codec_is_trimmed_to_the_nominal_duration() {
        // An AAC decoder outputs 1024 frames for a packet with a duration of 568.
        let signals = decoder_signals_frame_count(&params(CODEC_ID_AAC));
        assert!(!signals);
        assert_eq!(nominal_packet_dur(signals, Some(1024), Duration::new(568)).get(), 1024);
        assert_eq!(nominal_packet_dur(signals, Some(1024), Duration::new(1024)).get(), 1024);
    }

    #[test]
    fn shortened_final_packet_of_self_describing_codec_is_not_trimmed() {
        // ALAC and FLAC decoders output exactly the shortened number of frames; the nominal
        // duration must not be used or the tail would be trimmed twice.
        for codec in [CODEC_ID_ALAC, CODEC_ID_FLAC] {
            let signals = decoder_signals_frame_count(&params(codec));
            assert!(signals);
            assert_eq!(nominal_packet_dur(signals, Some(4096), Duration::new(4088)).get(), 4088);
        }
    }

    #[test]
    fn missing_codec_parameters_are_not_self_describing() {
        assert!(!decoder_signals_frame_count(&None));
    }
}

#[cfg(test)]
mod gapless_tests {
    use super::{ElstTiming, derive_timing_from_elst};
    use crate::atoms::{ElstAtom, ElstEntry};
    use std::num::NonZero;

    fn ts(hz: u32) -> NonZero<u32> {
        NonZero::new(hz).unwrap()
    }

    fn entry(segment_duration: u64, media_time: i64) -> ElstEntry {
        ElstEntry { segment_duration, media_time, media_rate_int: 1, media_rate_frac: 0 }
    }

    fn derive(
        entries: Vec<ElstEntry>,
        total: Option<u64>,
        media_hz: u32,
        movie_hz: u32,
    ) -> Option<ElstTiming> {
        derive_timing_from_elst(&ElstAtom { entries }, total, ts(media_hz), ts(movie_hz))
    }

    #[test]
    fn single_entry_delay_and_padding() {
        // Total media frames = delay + segment_duration + padding.
        let timing = derive(vec![entry(10_000, 1024)], Some(1024 + 10_000 + 512), 44_100, 44_100);

        assert_eq!(timing, Some(ElstTiming { start_offset: 0, delay: 1024, padding: 512 }));
    }

    #[test]
    fn converts_segment_duration_across_timescales() {
        // The movie timescale is 1000 Hz and the media timescale 44.1 kHz: 2 s is 88200 frames.
        let timing = derive(vec![entry(2_000, 1024)], Some(1024 + 88_200 + 256), 44_100, 1_000);

        assert_eq!(timing, Some(ElstTiming { start_offset: 0, delay: 1024, padding: 256 }));
    }

    #[test]
    fn initial_empty_edit_offsets_the_start() {
        // 0.5 s of silence (in the 1 kHz movie timescale), then the media from its start.
        let timing =
            derive(vec![entry(500, -1), entry(1_000, 0)], Some(44_100 + 100), 44_100, 1_000);

        assert_eq!(timing, Some(ElstTiming { start_offset: 22_050, delay: 0, padding: 100 }));
    }

    #[test]
    fn only_an_empty_edit_is_ignored() {
        assert_eq!(derive(vec![entry(100, -1)], Some(1000), 44_100, 44_100), None);
    }

    #[test]
    fn contiguous_edits_are_merged() {
        let timing = derive(
            vec![entry(1_000, 1024), entry(2_000, 2_024)],
            Some(1024 + 3_000 + 7),
            44_100,
            44_100,
        );

        assert_eq!(timing, Some(ElstTiming { start_offset: 0, delay: 1024, padding: 7 }));
    }

    #[test]
    fn discontiguous_edits_are_ignored() {
        let edits = vec![entry(100, 0), entry(100, 500)];
        assert_eq!(derive(edits, Some(1000), 44_100, 44_100), None);
    }

    #[test]
    fn non_unity_rate_is_ignored() {
        let mut slow = entry(100, 0);
        slow.media_rate_int = 0;
        assert_eq!(derive(vec![slow], Some(1000), 44_100, 44_100), None);
    }

    #[test]
    fn unknown_total_and_unbounded_edit_have_no_padding() {
        // A fragmented stream with an unknown duration: the edit list has a segment duration of 0.
        let timing = derive(vec![entry(0, 1024)], None, 44_100, 44_100);
        assert_eq!(timing, Some(ElstTiming { start_offset: 0, delay: 1024, padding: 0 }));

        let timing = derive(vec![entry(1_000, 1024)], None, 44_100, 44_100);
        assert_eq!(timing, Some(ElstTiming { start_offset: 0, delay: 1024, padding: 0 }));
    }
}

/// Read the chapters from a QuickTime chapter track: a text track (`text`, or `tx3g` for 3GPP)
/// referenced by the `chap` track reference of another track. Each sample is a chapter whose
/// start time is the timestamp of the sample, and whose title is the text of the sample: a 16-bit
/// length followed by UTF-8 or UTF-16 (with a byte order mark) text.
///
/// Reads sample data, but leaves the position of the iterator unchanged. The stream must be
/// seekable.
fn read_chapter_track(
    it: &mut AtomIterator<MediaSourceStream<'_>>,
    seg: &dyn StreamSegment,
    moov: &MoovAtom,
) -> Result<Option<ChapterGroup>> {
    // The maximum number of chapters read, and the maximum size of a chapter sample.
    const MAX_CHAPTERS: u32 = 65_536;
    const MAX_SAMPLE_LEN: u32 = 1 << 16;

    let chapter_track_id = moov
        .traks
        .iter()
        .filter_map(|trak| trak.tref.as_ref())
        .find_map(|tref| tref.chapter_tracks.first().copied());

    let Some((track_num, trak)) = chapter_track_id
        .and_then(|id| moov.traks.iter().enumerate().find(|(_, t)| t.tkhd.id == id))
    else {
        return Ok(None);
    };

    let time_base = TimeBase::from_recip(trak.mdia.mdhd.timescale);

    let saved_pos = it.pos();
    let mut chapters = Vec::new();

    for sample_num in 0..MAX_CHAPTERS {
        let Ok(Some(timing)) = seg.sample_timing(track_num, sample_num)
        else {
            break;
        };

        let Ok(desc) = seg.sample_data(track_num, sample_num, true)
        else {
            break;
        };

        let Some(pos) = desc.offset.and_then(|offset| desc.base_pos.checked_add(offset))
        else {
            break;
        };

        let Some(start) =
            i64::try_from(timing.ts).ok().and_then(|ts| time_base.calc_time(Timestamp::new(ts)))
        else {
            break;
        };

        if desc.size < 2 || desc.size > MAX_SAMPLE_LEN {
            continue;
        }

        let Ok(data) = it.read_raw_boxed_slice_exact(pos, desc.size as usize)
        else {
            break;
        };

        let len = usize::from(u16::from_be_bytes([data[0], data[1]]));
        let text = &data[2..(2 + len).min(data.len())];

        chapters.push((start, decode_text(text)));
    }

    it.restore_pos(saved_pos)?;

    // The last chapter ends at the end of the movie.
    let end =
        TimeBase::from_recip(moov.mvhd.timescale).calc_duration(Duration::new(moov.mvhd.duration));

    Ok(make_chapter_group("chap", chapters, end))
}

/// Information regarding the next sample.
#[derive(Debug)]
struct NextSampleInfo {
    /// The track number of the next sample.
    track_num: usize,
    /// The track id.
    track_id: u32,
    /// The timestamp of the next sample.
    ts: Timestamp,
    /// The timestamp expressed in seconds.
    time: Time,
    /// The duration of the next sample.
    dur: Duration,
    /// The segment containing the next sample.
    seg_idx: usize,
}

/// Information regarding a sample.
#[derive(Debug)]
struct SampleDataInfo {
    /// The position of the sample in the track.
    pos: u64,
    /// The length of the sample.
    len: u32,
}

/// A representation of time, defining a duration relative to a specific frequency
#[derive(Copy, Clone, Debug)]
pub struct TimeSpan {
    pub timescale: NonZero<u32>,
    pub duration: Duration,
}

impl Default for TimeSpan {
    fn default() -> Self {
        Self { timescale: NonZero::new(1).expect("1 is non-zero"), duration: Duration::ZERO }
    }
}

impl TimeSpan {
    pub fn new(timescale: NonZero<u32>, duration: Duration) -> Self {
        TimeSpan { timescale, duration }
    }
}

/// ISO Base Media File Format (MP4, M4A, MOV, etc.) demultiplexer.
///
/// `IsoMp4Reader` implements a demuxer for the ISO Base Media File Format.
pub struct IsoMp4Reader<'s> {
    iter: AtomIterator<MediaSourceStream<'s>>,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    metadata: MetadataLog,
    /// Chapters from a Nero chapter list (`chpl`) atom, if present.
    chapters: Option<ChapterGroup>,
    /// Segments of the movie. Sorted in ascending order by sequence number.
    segs: Vec<Box<dyn StreamSegment>>,
    /// State tracker for each track.
    track_states: Vec<TrackState>,
    /// Optional, movie extends atom used for fragmented streams.
    moov: Arc<MoovAtom>,
}

impl<'s> IsoMp4Reader<'s> {
    pub fn try_new(mut mss: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
        // To get to beginning of the atom.
        mss.seek_buffered_rel(-4);

        let is_seekable = mss.is_seekable();

        let mut ftyp = None;
        let mut moov = None;

        // Get the total length of the stream, if possible.
        let total_len = if is_seekable {
            let pos = mss.pos();
            let len = mss.seek(SeekFrom::End(0))?;
            mss.seek(SeekFrom::Start(pos))?;
            info!("stream is seekable with len={len} bytes.");
            Some(len)
        }
        else {
            None
        };

        let mut metadata = opts.external_data.metadata.unwrap_or_default();

        // Parse all atoms if the stream is seekable, otherwise parse all atoms up-to the mdat atom.
        let mut it = AtomIterator::new(mss, total_len);
        // Maps each track id to its cumulative duration (TimeSpan) as parsed from the segment
        // index, or None if the indexes of the track are inconsistent.
        let mut sidx_timespans: HashMap<u32, Option<TimeSpan>> = HashMap::new();

        while let Some(header) = it.next_header()? {
            // Top-level atoms.
            match header.atom_type() {
                AtomType::FileType => {
                    ftyp = Some(it.read_atom::<FtypAtom>()?);
                }
                AtomType::Movie => {
                    moov = Some(it.read_atom::<MoovAtom>()?);
                }
                AtomType::SegmentIndex => {
                    let sidx = it.read_atom::<SidxAtom>()?;

                    // Calculate the total duration, per track, from the segment index atoms. The
                    // segment index is not authoritative: if it is inconsistent, the duration is
                    // taken from the other atoms instead of failing.
                    let sidx_timespan = sidx_timespans
                        .entry(sidx.reference_id)
                        .or_insert(Some(TimeSpan::new(sidx.timescale, Duration::ZERO)));

                    let next = sidx_timespan.and_then(|span| {
                        let duration =
                            span.duration.checked_add(Duration::new(sidx.media_duration));
                        duration
                            .filter(|_| span.timescale == sidx.timescale)
                            .map(|duration| TimeSpan::new(span.timescale, duration))
                    });

                    if next.is_none() && sidx_timespan.is_some() {
                        warn!("isomp4: inconsistent sidx for track {}", sidx.reference_id);
                    }

                    *sidx_timespan = next;
                }
                AtomType::MediaData | AtomType::MovieFragment => {
                    // The mdat atom contains the codec bitstream data. For fragmented streams, a
                    // moof + mdat pair is required. If the ftyp and moov atoms have been read, then
                    // the top-level atom scan can exit here and begin playback immediately as an
                    // optimization. If not, then the scan must continue.
                    //
                    // The scan must also exit if the source is unseekable because in that case
                    // the format reader cannot skip past these atoms without dropping packets.
                    let is_playable = moov.is_some() && ftyp.is_some();

                    if is_playable || !is_seekable {
                        if !is_playable {
                            warn!("mp4 is not streamable.");
                        }
                        break;
                    }
                }
                AtomType::Meta => {
                    // Read the metadata atom and append it to the log.
                    let mut meta = it.read_atom::<MetaAtom>()?;

                    // ID3v2 metadata is pushed first so the preferred ilst metadata is current.
                    if let Some(rev) = meta.take_id3v2() {
                        metadata.push(rev);
                    }

                    if let Some(rev) = meta.take_metadata() {
                        metadata.push(rev);
                    }
                }
                AtomType::Free => (),
                AtomType::Skip => (),
                _ => {
                    info!("skipping top-level atom: {:?}.", header.atom_type());
                }
            }
        }

        if ftyp.is_none() {
            return unsupported_error("isomp4: missing ftyp atom");
        }

        if moov.is_none() {
            return unsupported_error("isomp4: missing moov atom");
        }

        // If the top-level atom scan iterated across the entire source (e.g., if moov was the last
        // atom), then the iterator must return to the first moof or mdat atom. This is only
        // possible if the source is seekable. If it's not, then the media will be effectively
        // unplayable.
        if is_seekable && it.pending().is_none() {
            let mut mss = it.into_inner();
            mss.seek(SeekFrom::Start(0))?;

            it = AtomIterator::new(mss, total_len);

            while let Some(header) = it.next_header()? {
                if let AtomType::MovieFragment | AtomType::MediaData = header.atom_type() {
                    break;
                }
            }
        }

        // Fragments (moof + mdat pairs) are streamed. So if the pending atom is a moof, seek the
        // iterator to the start of the moof atom.
        if let Some(atom) = it.pending() {
            if atom.atom_type() == AtomType::MovieFragment {
                it.seek_atom_start()?;
            }
        }

        let mut moov = moov.expect("moov is checked for None above");

        if moov.is_fragmented() {
            if !sidx_timespans.is_empty() {
                info!("stream is segmented with a segment index.");
            }
            else {
                info!("stream is segmented without a segment index.");
            }
        }

        // ID3v2 metadata is pushed first so the preferred ilst metadata is current.
        let mut revisions = Vec::new();

        while let Some(rev) = moov.take_id3v2() {
            revisions.push(rev);
        }

        while let Some(rev) = moov.take_metadata() {
            revisions.push(rev);
        }

        // Track-level metadata is attached to the preferred (last) revision.
        let per_track: Vec<_> =
            moov.traks.iter_mut().filter_map(|trak| trak.take_metadata()).collect();

        if !per_track.is_empty() {
            if revisions.is_empty() {
                revisions.push(MetadataBuilder::new(ISOMP4_METADATA_INFO).build());
            }

            if let Some(rev) = revisions.last_mut() {
                rev.per_track.extend(per_track);
            }
        }

        for rev in revisions {
            metadata.push(rev);
        }

        // Create a track and track state for each Track (trak) atom.
        let mut tracks = Vec::with_capacity(moov.traks.len());
        let mut track_states = Vec::with_capacity(moov.traks.len());

        let itunes_gapless = moov.gapless();
        for (t, trak) in moov.traks.iter().enumerate() {
            // Determine the duration of the track in ticks of the media timescale, if known.
            let timescale = trak.mdia.mdhd.timescale;

            let (timescale, duration) = if moov.is_fragmented() {
                // If fragmented, prefer the duration from the sidx, if it is provided. Otherwise,
                // fallback to the mdhd, and the fragment duration of the mvex. These durations are
                // usually 0 (unknown) for a stream that is written as it is produced.
                match sidx_timespans.get(&trak.tkhd.id).copied().flatten() {
                    Some(sidx_tspan) => (sidx_tspan.timescale, Some(sidx_tspan.duration)),
                    None => {
                        let mehd_duration = moov
                            .mvex
                            .as_ref()
                            .and_then(|mvex| mvex.mehd.as_ref())
                            .map(|mehd| mehd.fragment_duration)
                            .and_then(|dur| {
                                // The fragment duration is in the movie timescale.
                                let dur = u128::from(dur) * u128::from(timescale.get())
                                    / u128::from(moov.mvhd.timescale.get());
                                u64::try_from(dur).ok()
                            });

                        let duration = Some(trak.mdia.mdhd.duration)
                            .filter(|&dur| dur != 0)
                            .or(mehd_duration)
                            .filter(|&dur| dur != 0)
                            .map(Duration::from);

                        (timescale, duration)
                    }
                }
            }
            else {
                // If non-fragmented, use the total duration (media timescale) from the track's
                // stts atom: the duration of all samples that will be yielded, including any
                // delay and padding trimmed by the edit list.
                (timescale, Some(Duration::from(trak.mdia.minf.stbl.stts.total_duration)))
            };

            let (track_state, track) =
                TrackState::make(t, trak, timescale, duration, moov.mvhd.timescale, itunes_gapless);

            tracks.push(track);
            track_states.push(track_state);
        }

        // The number of tracks specified in the moov atom must match the number in the mvex atom.
        if let Some(mvex) = &moov.mvex {
            if mvex.trexs.len() != moov.traks.len() {
                return decode_error("isomp4: mvex and moov track number mismatch");
            }
        }

        // The moov atom will be shared among all segments and the demuxer using an Arc.
        let moov = Arc::new(moov);

        let segs: Vec<Box<dyn StreamSegment>> = vec![Box::new(MoovSegment::new(moov.clone()))];

        // Populate media information.
        let mut media_info = MediaInfo::new();
        media_info.with_time_base(TimeBase::from_recip(moov.mvhd.timescale));

        // The duration of a fragmented stream that is written as it is produced is unknown (0).
        if moov.mvhd.duration != 0 || !moov.is_fragmented() {
            media_info.with_duration(Duration::new(moov.mvhd.duration));
        }

        // Prefer the Nero chapter list, then a QuickTime chapter track.
        let chapters = match moov.chapters() {
            Some(chapters) => Some(chapters),
            None if is_seekable => read_chapter_track(&mut it, segs[0].as_ref(), &moov)?,
            None => None,
        }
        .or(opts.external_data.chapters);

        Ok(IsoMp4Reader {
            iter: it,
            media_info,
            tracks,
            metadata,
            chapters,
            track_states,
            segs,
            moov,
        })
    }

    /// Idempotently gets information regarding the next sample of the media stream. This function
    /// selects the next sample with the lowest timestamp of all tracks.
    fn next_sample_info(&self) -> Result<Option<NextSampleInfo>> {
        let mut earliest = None;

        // TODO: Consider returning samples based on lowest byte position in the track instead of
        // timestamp. This may be important if video tracks are ever decoded (i.e., DTS vs. PTS).

        for state in self.track_states.iter() {
            // Get the timebase of the media timestamps used to calculate the presentation time.
            let tb = state.media_time_base;

            // Get the next timestamp for the next sample of the current track. The next sample may
            // be in a future segment.
            for (seg_idx_delta, seg) in self.segs[state.cur_seg..].iter().enumerate() {
                // Try to get the timestamp for the next sample of the track from the segment.
                if let Some(timing) = seg.sample_timing(state.track_num, state.next_sample)? {
                    // Calculate the presentation time using the timestamp.
                    let Some(ts) = timing.ts.try_into().ok()
                    else {
                        return Ok(None);
                    };

                    let Some(sample_time) = tb.calc_time(ts)
                    else {
                        return Ok(None);
                    };

                    // Compare the presentation time of the sample from this track to other tracks,
                    // and select the track with the earliest presentation time.
                    match earliest {
                        Some(NextSampleInfo { time, .. }) if time <= sample_time => {
                            // Earliest is less than or equal to the track's next sample
                            // presentation time. No need to update earliest.
                        }
                        _ => {
                            // Earliest was either None, or greater than the track's next sample
                            // presentation time. Update earliest.
                            earliest = Some(NextSampleInfo {
                                track_num: state.track_num,
                                track_id: state.track_id,
                                ts,
                                time: sample_time,
                                dur: Duration::from(timing.dur),
                                seg_idx: seg_idx_delta + state.cur_seg,
                            });
                        }
                    }

                    // Either the next sample of the track had the earliest presentation time seen
                    // thus far, or it was greater than those from other tracks, but there is no
                    // reason to check samples in future segments.
                    break;
                }
            }
        }

        Ok(earliest)
    }

    fn consume_next_sample(&mut self, info: &NextSampleInfo) -> Result<Option<SampleDataInfo>> {
        // Get the track state.
        let track = &mut self.track_states[info.track_num];

        // Get the segment associated with the sample.
        let seg = &self.segs[info.seg_idx];

        // Get the sample data descriptor.
        let sample_data_desc = seg.sample_data(track.track_num, track.next_sample, false)?;

        // The sample base position in the sample data descriptor remains constant if the sample
        // followed immediately after the previous sample. In this case, the track state's
        // next_sample_pos is the position of the current sample. If the base position has jumped,
        // then the base position is the position of current the sample.
        let pos = if sample_data_desc.base_pos > track.next_sample_pos {
            sample_data_desc.base_pos
        }
        else {
            track.next_sample_pos
        };

        // Advance the track's current segment to the next sample's segment.
        track.cur_seg = info.seg_idx;

        // Advance the track's next sample number and position.
        track.next_sample += 1;
        track.next_sample_pos = pos + u64::from(sample_data_desc.size);

        Ok(Some(SampleDataInfo { pos, len: sample_data_desc.size }))
    }

    fn try_read_more_segments(&mut self) -> Result<bool> {
        // If all tracks ended in the last segment, then do not try to read anymore segments.
        //
        // Note, there will always be one segment because the moov atom was converted into a segment
        // when the reader was instantiated.
        let last_seg = self.segs.last().expect("always at least one segment from moov atom");
        if last_seg.all_tracks_ended() {
            return Ok(false);
        }

        // Continue iterating over atoms until a segment (a moof + mdat atom pair) is found. All
        // other atoms will be ignored.
        loop {
            let header = match self.iter.next_header() {
                Ok(Some(header)) => header,
                Ok(None) => break,
                // If fragmented, an EOF is the only way to truly detect the end of stream.
                Err(AtomError::Other(Error::IoError(err)))
                    if self.moov.is_fragmented()
                        && err.kind() == std::io::ErrorKind::UnexpectedEof =>
                {
                    break;
                }
                // Passthrough other errors.
                Err(err) => return Err(err.into()),
            };

            match header.atom_type() {
                AtomType::MediaData => {
                    return Ok(true);
                }
                AtomType::MovieFragment => {
                    let moof = self.iter.read_atom::<MoofAtom>()?;

                    // A moof segment can only be created if the media is fragmented.
                    if self.moov.is_fragmented() {
                        // Get the last segment.
                        let last_seg =
                            self.segs.last().expect("always at least one segment from moov atom");

                        // Create a new segment for the moof atom.
                        let seg = MoofSegment::new(moof, self.moov.clone(), last_seg.as_ref())?;

                        // Segments should have a monotonic sequence number.
                        if seg.sequence_num() <= last_seg.sequence_num() {
                            warn!("moof fragment has a non-monotonic sequence number.");
                        }

                        // Push the segment.
                        self.segs.push(Box::new(seg));
                    }
                    else {
                        return decode_error("isomp4: moof atom present without mvex atom");
                    }
                }
                _ => {
                    trace!("skipping atom: {:?}.", header.atom_type());
                }
            }
        }

        // If no atoms were returned above, then the end-of-stream has been reached.
        Ok(false)
    }

    fn seek_track_by_time(&mut self, track_num: usize, time: Time) -> Result<SeekedTo> {
        // Convert time to timestamp for the track.
        if let Some(track) = self.tracks.get(track_num) {
            let tb = track.time_base.expect("track always created with a timebase");
            let ts = tb.calc_timestamp(time).ok_or(Error::SeekError(SeekErrorKind::OutOfRange))?;
            self.seek_track_by_ts(track_num, ts)
        }
        else {
            seek_error(SeekErrorKind::Unseekable)
        }
    }

    fn seek_track_by_ts(&mut self, track_num: usize, ts: Timestamp) -> Result<SeekedTo> {
        debug!("seeking track_num={track_num} to frame_ts={ts}");

        struct SeekLocation {
            seg_idx: usize,
            sample_num: u32,
        }

        // Can only seek to 0 or positive timestamps.
        if ts.is_negative() {
            return seek_error(SeekErrorKind::OutOfRange);
        }

        // The timestamp of the seek is on the timeline of the packets of the track: in decoded
        // frames, and excluding the encoder delay. The timestamps of the samples in the sample
        // table are in ticks of the media timescale, and include the delay.
        let delay = u64::from(self.tracks[track_num].delay.unwrap_or(0));
        let frames_per_tick = self.track_states[track_num].frames_per_tick;

        let start_offset = self.track_states[track_num].start_offset;

        // A target within the initial empty edit is the start of the media.
        let media_ts =
            (ts.get() as u64).saturating_sub(start_offset).saturating_add(delay) / frames_per_tick;

        let mut seg_skip = 0;

        let seek_loc = 'locate: loop {
            // Iterate over all segments and attempt to find the segment and sample number that
            // contains the desired timestamp. Skip segments already examined.
            for (seg_idx, seg) in self.segs.iter().enumerate().skip(seg_skip) {
                if let Some(sample_num) = seg.ts_sample(track_num, media_ts)? {
                    break 'locate SeekLocation { seg_idx, sample_num };
                }

                // Mark the segment as examined.
                seg_skip = seg_idx + 1;
            }

            // Otherwise, try to read more segments from the stream.
            if !self.try_read_more_segments()? {
                return seek_error(SeekErrorKind::OutOfRange);
            }
        };

        let seg = &self.segs[seek_loc.seg_idx];

        // Start decoding a number of samples before the target sample so that the decoder has
        // converged by the time the target is reached. This is done for coarse seeks as well: a
        // decoder started at the target sample outputs wrong audio (at best a glitch, for AAC
        // without SBR the first frame; for SBR until the state has settled, with the wrong noise
        // phase forever) and, in particular, a seek to the start would not be the same as playing
        // from the start. The pre-roll is limited to the segment containing the target sample.
        let sample_num = self.track_states[track_num]
            .seek_preroll
            .start_sample(seek_loc.sample_num, seek_loc.seg_idx == 0);

        // Get the sample timing.
        let timing = match seg.sample_timing(track_num, sample_num)? {
            Some(t) => t,
            None => return seek_error(SeekErrorKind::OutOfRange),
        };

        // Convert the sample timing to a timestamp on the timeline of the packets.
        let actual_ts = match i64::try_from(timing.ts)
            .ok()
            .and_then(|ts| ts.checked_mul(i64::try_from(frames_per_tick).ok()?))
            .and_then(|ts| ts.checked_sub(i64::try_from(delay).ok()?))
            .and_then(|ts| ts.checked_add(i64::try_from(start_offset).ok()?))
        {
            Some(ts) => Timestamp::new(ts),
            _ => return seek_error(SeekErrorKind::OutOfRange),
        };

        // Get the sample information.
        let data_desc = seg.sample_data(track_num, sample_num, true)?;

        // Update the track's next sample information to point to the seeked sample.
        let track = &mut self.track_states[track_num];

        track.cur_seg = seek_loc.seg_idx;
        track.next_sample = sample_num;
        track.next_sample_pos = data_desc.base_pos
            + match data_desc.offset {
                Some(o) => o,
                None => return seek_error(SeekErrorKind::OutOfRange),
            };

        debug!(
            "seeked track_num={} (track_id={}) to packet_ts={} (delta={})",
            track_num,
            track.track_id,
            actual_ts,
            actual_ts.saturating_delta(ts),
        );

        Ok(SeekedTo { track_id: track.track_id, required_ts: ts, actual_ts })
    }
}

impl Scoreable for IsoMp4Reader<'_> {
    fn score(_src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        Ok(Score::Supported(255))
    }
}

impl ProbeableFormat<'_> for IsoMp4Reader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + '_>> {
        Ok(Box::new(IsoMp4Reader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(
            ISOMP4_FORMAT_INFO,
            &["mp4", "m4a", "m4p", "m4b", "m4r", "m4v", "mov"],
            &["video/mp4", "audio/m4a"],
            &[b"ftyp"] // Top-level atoms
        )]
    }
}

impl FormatReader for IsoMp4Reader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &ISOMP4_FORMAT_INFO
    }

    fn media_info(&self) -> &MediaInfo {
        &self.media_info
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        // Get the index of the track with the next-nearest (minimum) timestamp.
        let next_sample_info = loop {
            // Using the current set of segments, try to get the next sample info.
            if let Some(info) = self.next_sample_info()? {
                break info;
            }
            else {
                // The inner reader of the atom iterator has been used/seeked around to read
                // packets, so resync the reader and iterator by seeking to the end of the current
                // pending atom. Under regular circumstances, no actual expensive seek operation is
                // performed since the reader should be at the end of the last iterated atom if we
                // are trying to read another.
                match self.iter.seek_atom_end() {
                    Ok(_) | Err(AtomError::NoPendingAtom) => (),
                    Err(_) => return decode_error("sync lost"),
                };

                // No more segments. If the stream is unseekable, it may be the case that there are
                // more segments coming. If the stream is seekable it might be fragmented and no
                // segments are found in the moov atom. Iterate atoms until a new segment is found
                // or the end-of-stream is reached
                if !self.try_read_more_segments()? {
                    return Ok(None);
                }
            }
        };

        // The index of the sample.
        let sample_idx = self.track_states[next_sample_info.track_num].next_sample;

        // Get the position and length information of the next sample.
        let sample_info = match self.consume_next_sample(&next_sample_info)? {
            Some(s) => s,
            None => return decode_error("isomp4: failed to get next sample data"),
        };

        let data =
            self.iter.read_raw_boxed_slice_exact(sample_info.pos, sample_info.len as usize)?;

        let track = self
            .tracks
            .iter()
            .find(|track| track.id == next_sample_info.track_id)
            .expect("track exists for the returned track_id");

        let delay = u64::from(track.delay.unwrap_or(0));

        // Determine the "nominal" (undivided) per-sample duration used by the sample table. The
        // final `stts` entry is often deliberately shortened to encode the true valid tail
        // duration, but the decoder will still produce a full frame's worth of raw samples; the
        // difference must be trimmed. Fixed-frame-size codecs (a prerequisite of the
        // `is_audio_frame_accurate` gate used to populate `delay`/`padding`) always emit
        // `nominal` samples per packet except for this potentially-shortened final entry.
        //
        // Codecs that signal the number of frames of every packet in the bitstream (ALAC, FLAC)
        // already output only the shortened number of frames, so there is nothing to trim.
        let stts_nominal = self.track_states[next_sample_info.track_num].stts_nominal;

        let nominal_dur = nominal_packet_dur(
            decoder_signals_frame_count(&track.codec_params),
            stts_nominal,
            next_sample_info.dur,
        );

        // Without delay or padding, only a shortened final sample of a track with a known number
        // of frames needs to be trimmed.
        let is_shortened = track.num_frames.is_some() && nominal_dur > next_sample_info.dur;

        // The timeline of the track is in decoded frames, which are `frames_per_tick` per tick of
        // the media timescale.
        let frames_per_tick = self.track_states[next_sample_info.track_num].frames_per_tick;
        let ratio = i64::try_from(frames_per_tick).unwrap_or(1);

        let media_pts = Timestamp::new(next_sample_info.ts.get().saturating_mul(ratio));

        // A shortened sample at the start of the track encodes an encoder delay (e.g., Nero
        // encodes the priming samples of HE-AAC with shortened durations, in the absence of an
        // edit list): the valid frames are the last ones of the decoded block, which therefore
        // starts before the timestamp of the sample.
        let is_leading_shortened = is_shortened
            && sample_idx < MAX_SHORTENED_LEADING_SAMPLES
            && track.duration.is_none_or(|dur| {
                let end = media_pts.get() as u64 + next_sample_info.dur.get() * frames_per_tick;
                end < dur.get()
            });

        // The offset of the start of the track due to an initial empty edit.
        let start_offset =
            Duration::new(self.track_states[next_sample_info.track_num].start_offset);

        let offset_pts = |pts: Timestamp| pts.checked_add(start_offset).unwrap_or(pts);

        if delay == 0 && track.padding.unwrap_or(0) == 0 && !is_shortened {
            return Ok(Some(Packet::new(
                next_sample_info.track_id,
                offset_pts(media_pts),
                Duration::new(next_sample_info.dur.get().saturating_mul(frames_per_tick)),
                data,
            )));
        }

        // Shift the presentation timeline so that sample 0 corresponds to the first valid
        // (non-delay) frame, then let `trimmed_dur` compute trim_start/trim_end from the shifted
        // PTS and the track's valid frame count.
        let mut pts = media_pts.saturating_sub(Duration::new(delay));

        if is_leading_shortened {
            let missing = nominal_dur.get().saturating_sub(next_sample_info.dur.get());
            pts = pts.saturating_sub(Duration::new(missing.saturating_mul(frames_per_tick)));
        }

        let block_dur = Duration::new(nominal_dur.get().saturating_mul(frames_per_tick));

        let end_pts = track
            .num_frames
            .map(Duration::from)
            .and_then(|dur| dur.timestamp_from(Timestamp::ZERO));

        let mut packet = PacketBuilder::new()
            .track_id(next_sample_info.track_id)
            .pts(pts)
            .trimmed_dur(block_dur, end_pts)
            .data(data)
            .build();

        // The trim was calculated relative to the start of the media, so the offset of the start
        // of the track is applied last.
        packet.pts = offset_pts(packet.pts);

        Ok(Some(packet))
    }

    fn chapters(&self) -> Option<&ChapterGroup> {
        self.chapters.as_ref()
    }

    fn metadata(&mut self) -> Metadata<'_> {
        self.metadata.metadata()
    }

    fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    fn seek(&mut self, _mode: SeekMode, to: SeekTo) -> Result<SeekedTo> {
        if self.tracks.is_empty() {
            return seek_error(SeekErrorKind::Unseekable);
        }

        match to {
            SeekTo::Timestamp { ts, track_id } => {
                // The seek timestamp is in timebase units specific to the selected track. Get the
                // selected track and use the timebase to convert the timestamp into time units so
                // that the other tracks can be seeked.
                if let Some((track_num, track)) =
                    self.tracks.iter().enumerate().find(|(_, track)| track.id == track_id)
                {
                    // Convert to time units.
                    let tb = match track.time_base {
                        Some(tb) => tb,
                        None => return seek_error(SeekErrorKind::Unseekable),
                    };
                    let time =
                        tb.calc_time(ts).ok_or(Error::SeekError(SeekErrorKind::Unseekable))?;

                    // Seek all tracks excluding the primary track to the desired time.
                    for t in 0..self.track_states.len() {
                        if t != track_num {
                            self.seek_track_by_time(t, time)?;
                        }
                    }

                    // Seek the primary track and return the result.
                    self.seek_track_by_ts(track_num, ts)
                }
                else {
                    seek_error(SeekErrorKind::InvalidTrack)
                }
            }
            SeekTo::Time { time, track_id } => {
                // If provided, find the track number of the track with the desired track_id, or
                // default to the first track.
                let track_num = match track_id {
                    Some(id) => self
                        .tracks
                        .iter()
                        .position(|track| track.id == id)
                        .ok_or(Error::SeekError(SeekErrorKind::InvalidTrack))?,
                    None => 0,
                };

                // Seek all tracks excluding the selected track and discard the result.
                for t in 0..self.track_states.len() {
                    if t != track_num {
                        self.seek_track_by_time(t, time)?;
                    }
                }

                // Seek the primary track and return the result.
                self.seek_track_by_time(track_num, time)
            }
        }
    }

    fn into_inner<'s>(self: Box<Self>) -> MediaSourceStream<'s>
    where
        Self: 's,
    {
        self.iter.into_inner()
    }
}

impl ReadAtom for MediaSourceStream<'_> {}

impl From<AtomError> for Error {
    fn from(value: AtomError) -> Self {
        // Map all atom iteration errors to decode errors.
        let msg = match value {
            AtomError::InvalidAtomSize => "isomp4: invalid atom size",
            AtomError::InvalidUtf8 => "isomp4: invalid utf-8 string",
            AtomError::MaximumDepthReached => "isomp4: maximum recursion depth reached",
            AtomError::NoParentAtom => "isomp4: no parent atom",
            AtomError::NoPendingAtom => "isomp4: no atom pending read",
            AtomError::Overrun => "isomp4: overrun while reading atom",
            AtomError::SeekOutOfRange => "isomp4: out-of-bounds seek for a non-seekable stream",
            AtomError::UnexpectedEndOfAtom => "isomp4: unexpected end of atom",
            AtomError::UnexpectedPosition => "isomp4: unexpected position",
            AtomError::UnexpectedReadOperation => "isomp4: unexpected read operation",
            AtomError::UnknownAtomSize => "isomp4: unknown atom size",
            AtomError::Other(err) => return err,
        };
        Error::DecodeError(msg)
    }
}

// fn convert_timescale(
//     duration: u64,
//     src_timescale: NonZero<u32>,
//     dst_timescale: NonZero<u32>,
// ) -> Duration {
//     if src_timescale == dst_timescale {
//         return Duration::from(duration);
//     }
//     Duration::from(
//         ((duration as u128 * dst_timescale.get() as u128) / src_timescale.get() as u128) as u64,
//     )
// }

#[cfg(test)]
mod aac_timeline_tests {
    use std::io::Cursor;

    use symphonia_core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
    use symphonia_core::io::MediaSourceStream;
    use symphonia_core::units::Timestamp;

    use crate::IsoMp4Reader;

    fn atom(fourcc: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut buf = (body.len() as u32 + 8).to_be_bytes().to_vec();
        buf.extend_from_slice(fourcc);
        buf.extend_from_slice(body);
        buf
    }

    /// An `esds` atom for AAC with the given audio specific config.
    fn esds(asc: &[u8]) -> Vec<u8> {
        // Object type, stream type, buffer size (3), max bitrate (4), average bitrate (4).
        let mut dec_config = vec![0x40, 0x15, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        dec_config.extend_from_slice(&[0x05, asc.len() as u8]);
        dec_config.extend_from_slice(asc);

        let mut es = vec![0, 1, 0];
        es.extend_from_slice(&[0x04, dec_config.len() as u8]);
        es.extend_from_slice(&dec_config);
        es.extend_from_slice(&[0x06, 0x01, 0x02]);

        let mut body = vec![0; 4];
        body.extend_from_slice(&[0x03, es.len() as u8]);
        body.extend_from_slice(&es);
        atom(b"esds", &body)
    }

    /// Build an MP4 with one AAC track. Every sample is 4 bytes and has its index as its first
    /// byte. `stts` is a list of (count, delta) and `elst` an optional (media_time,
    /// segment_duration) edit, both in ticks of `timescale`.
    fn mp4(asc: &[u8], timescale: u32, stts: &[(u32, u32)], elst: Option<(i32, u32)>) -> Vec<u8> {
        let edits: Vec<_> = elst.into_iter().collect();
        mp4_with_edits(asc, timescale, stts, &edits, &[])
    }

    /// As `mp4`, with extra atoms (e.g. `udta`) appended to the `moov` atom.
    fn mp4_with_moov_extra(
        asc: &[u8],
        timescale: u32,
        stts: &[(u32, u32)],
        elst: Option<(i32, u32)>,
        moov_extra: &[u8],
    ) -> Vec<u8> {
        let edits: Vec<_> = elst.into_iter().collect();
        mp4_with_edits(asc, timescale, stts, &edits, moov_extra)
    }

    /// As `mp4`, with a list of (media_time, segment_duration) edits, and extra atoms (e.g.
    /// `udta`) appended to the `moov` atom.
    fn mp4_with_edits(
        asc: &[u8],
        timescale: u32,
        stts: &[(u32, u32)],
        edits: &[(i32, u32)],
        moov_extra: &[u8],
    ) -> Vec<u8> {
        let n_samples: u32 = stts.iter().map(|(count, _)| count).sum();
        let duration: u32 = stts.iter().map(|(count, delta)| count * delta).sum();

        let mut entry = vec![0; 6];
        entry.extend_from_slice(&1u16.to_be_bytes());
        entry.extend_from_slice(&[0; 8]);
        entry.extend_from_slice(&2u16.to_be_bytes());
        entry.extend_from_slice(&16u16.to_be_bytes());
        entry.extend_from_slice(&[0; 4]);
        entry.extend_from_slice(&((timescale.min(65535)) << 16).to_be_bytes());
        entry.extend_from_slice(&esds(asc));

        let mut stsd = vec![0; 4];
        stsd.extend_from_slice(&1u32.to_be_bytes());
        stsd.extend_from_slice(&atom(b"mp4a", &entry));

        let mut stts_body = vec![0; 4];
        stts_body.extend_from_slice(&(stts.len() as u32).to_be_bytes());
        for (count, delta) in stts {
            stts_body.extend_from_slice(&count.to_be_bytes());
            stts_body.extend_from_slice(&delta.to_be_bytes());
        }

        let mut stsc = vec![0; 4];
        stsc.extend_from_slice(&1u32.to_be_bytes());
        for v in [1u32, n_samples, 1] {
            stsc.extend_from_slice(&v.to_be_bytes());
        }

        let mut stsz = vec![0; 4];
        stsz.extend_from_slice(&4u32.to_be_bytes());
        stsz.extend_from_slice(&n_samples.to_be_bytes());

        let build = |chunk_offset: u32| {
            let mut stco = vec![0; 4];
            stco.extend_from_slice(&1u32.to_be_bytes());
            stco.extend_from_slice(&chunk_offset.to_be_bytes());

            let stbl = atom(
                b"stbl",
                &[
                    atom(b"stsd", &stsd),
                    atom(b"stts", &stts_body),
                    atom(b"stsc", &stsc),
                    atom(b"stsz", &stsz),
                    atom(b"stco", &stco),
                ]
                .concat(),
            );

            let minf = atom(b"minf", &[atom(b"smhd", &[0; 8]), stbl].concat());

            let mut mdhd = vec![0; 4];
            mdhd.extend_from_slice(&[0; 8]);
            mdhd.extend_from_slice(&timescale.to_be_bytes());
            mdhd.extend_from_slice(&duration.to_be_bytes());
            mdhd.extend_from_slice(&[0; 4]);

            let mut hdlr = vec![0; 8];
            hdlr.extend_from_slice(b"soun");
            hdlr.extend_from_slice(&[0; 12]);

            let mdia = atom(b"mdia", &[atom(b"mdhd", &mdhd), atom(b"hdlr", &hdlr), minf].concat());

            let mut tkhd = vec![0; 4];
            tkhd.extend_from_slice(&[0; 8]);
            tkhd.extend_from_slice(&1u32.to_be_bytes());
            tkhd.extend_from_slice(&[0; 8]);
            tkhd.extend_from_slice(&[0; 14]);

            let mut trak = vec![atom(b"tkhd", &tkhd)];

            if !edits.is_empty() {
                let mut elst = vec![0; 4];
                elst.extend_from_slice(&(edits.len() as u32).to_be_bytes());
                for (media_time, segment_duration) in edits {
                    elst.extend_from_slice(&segment_duration.to_be_bytes());
                    elst.extend_from_slice(&media_time.to_be_bytes());
                    elst.extend_from_slice(&0x0001_0000u32.to_be_bytes());
                }
                trak.push(atom(b"edts", &atom(b"elst", &elst)));
            }

            trak.push(mdia);

            let mut mvhd = vec![0; 4];
            mvhd.extend_from_slice(&[0; 8]);
            mvhd.extend_from_slice(&timescale.to_be_bytes());
            mvhd.extend_from_slice(&duration.to_be_bytes());
            mvhd.extend_from_slice(&0x0001_0000u32.to_be_bytes());
            mvhd.extend_from_slice(&0x0100u16.to_be_bytes());

            let moov = atom(
                b"moov",
                &[atom(b"mvhd", &mvhd), atom(b"trak", &trak.concat()), moov_extra.to_vec()]
                    .concat(),
            );

            let mut ftyp = b"M4A ".to_vec();
            ftyp.extend_from_slice(&[0; 4]);
            ftyp.extend_from_slice(b"M4A mp42");

            [atom(b"ftyp", &ftyp), moov].concat()
        };

        // The chunk offset is the length of everything that precedes the media data (mdat header
        // included), which does not depend on its value.
        let head_len = build(0).len() as u32 + 8;
        let mut data = build(head_len);

        let mut mdat = vec![];
        for i in 0..n_samples {
            mdat.extend_from_slice(&[i as u8, 0, 0, 0]);
        }
        data.extend_from_slice(&atom(b"mdat", &mdat));
        data
    }

    const LC_ASC: [u8; 2] = [0x12, 0x10];

    fn id3v23_title_tag(title: &str) -> Vec<u8> {
        let mut body = vec![3u8];
        body.extend_from_slice(title.as_bytes());
        let mut frame = b"TIT2".to_vec();
        frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
        frame.extend_from_slice(&[0, 0]);
        frame.extend_from_slice(&body);
        let n = frame.len();
        let mut tag = vec![b'I', b'D', b'3', 3, 0, 0];
        tag.extend_from_slice(&[
            ((n >> 21) & 0x7f) as u8,
            ((n >> 14) & 0x7f) as u8,
            ((n >> 7) & 0x7f) as u8,
            (n & 0x7f) as u8,
        ]);
        tag.extend_from_slice(&frame);
        tag
    }

    /// An `ID32` box: version/flags, a packed language code ("und"), then the ID3v2 tag.
    fn id32(title: &str) -> Vec<u8> {
        let mut body = vec![0; 4];
        body.extend_from_slice(&0x55c4u16.to_be_bytes());
        body.extend_from_slice(&id3v23_title_tag(title));
        atom(b"ID32", &body)
    }

    fn titles(reader: &mut IsoMp4Reader<'_>) -> Vec<String> {
        use symphonia_core::meta::StandardTag;

        let mut metadata = reader.metadata();
        let mut titles = Vec::new();
        loop {
            if let Some(rev) = metadata.current() {
                titles.extend(rev.media.tags.iter().filter_map(|t| match &t.std {
                    Some(StandardTag::TrackTitle(v)) => Some(v.to_string()),
                    _ => None,
                }));
            }
            if metadata.pop().is_none() {
                break;
            }
        }
        titles
    }

    #[test]
    fn id32_box_in_udta_meta_is_read() {
        let mut meta_body = vec![0; 4];
        meta_body.extend_from_slice(&id32("In meta"));
        let udta = atom(b"udta", &atom(b"meta", &meta_body));

        let mut reader = open(mp4_with_moov_extra(&LC_ASC, 44_100, &[(10, 1024)], None, &udta));
        assert_eq!(titles(&mut reader), ["In meta"]);
        assert_eq!(reader.tracks().len(), 1);
    }

    fn data_atom(text: &str) -> Vec<u8> {
        let mut body = vec![0, 0, 0, 1, 0, 0, 0, 0];
        body.extend_from_slice(text.as_bytes());
        atom(b"data", &body)
    }

    #[test]
    fn quicktime_keyed_metadata_in_moov_meta_is_read() {
        use symphonia_core::meta::StandardTag;

        let key = b"com.apple.quicktime.title";
        let mut keys = vec![0; 4];
        keys.extend_from_slice(&2u32.to_be_bytes());
        keys.extend_from_slice(&(8 + key.len() as u32).to_be_bytes());
        keys.extend_from_slice(b"mdta");
        keys.extend_from_slice(key);
        keys.extend_from_slice(&(8 + 4u32).to_be_bytes());
        keys.extend_from_slice(b"mdta");
        keys.extend_from_slice(b"mood");

        let ilst = [
            atom(&1u32.to_be_bytes(), &data_atom("Keyed title")),
            atom(&2u32.to_be_bytes(), &data_atom("Calm")),
        ]
        .concat();

        // A QuickTime meta atom is not a full box: its first child follows the atom header.
        let hdlr = atom(b"hdlr", &[vec![0; 8], b"mdta".to_vec(), vec![0; 12]].concat());
        let meta = atom(b"meta", &[hdlr, atom(b"keys", &keys), atom(b"ilst", &ilst)].concat());

        let mut reader = open(mp4_with_moov_extra(&LC_ASC, 44_100, &[(10, 1024)], None, &meta));
        assert_eq!(titles(&mut reader), ["Keyed title"]);

        let rev = reader.metadata().current().expect("revision").media.tags.clone();
        assert!(rev.iter().any(|t| t.raw.key == "mood" && t.std.is_none()));
        assert!(rev.iter().any(|t| matches!(&t.std, Some(StandardTag::TrackTitle(_)))));
    }

    #[test]
    fn quicktime_and_3gpp_user_data_text_is_read() {
        let mut qt = vec![0, 8, 0x55, 0xc4];
        qt.extend_from_slice(b"QT title");

        let mut gpp = vec![0; 4];
        gpp.extend_from_slice(&0x55c4u16.to_be_bytes());
        gpp.extend_from_slice(b"3GPP title\0");

        let udta = atom(b"udta", &[atom(b"\xa9nam", &qt), atom(b"titl", &gpp)].concat());

        let mut reader = open(mp4_with_moov_extra(&LC_ASC, 44_100, &[(10, 1024)], None, &udta));
        assert_eq!(titles(&mut reader), ["QT title", "3GPP title"]);
    }

    #[test]
    fn unknown_size_atom_in_sized_parent_is_clamped() {
        // A zero size means "to the end of the file", which writers sometimes (wrongly) use within
        // the moov atom.
        let mut extra = vec![0, 0, 0, 0];
        extra.extend_from_slice(b"free");
        extra.extend_from_slice(&[0xaa; 16]);

        let mut reader = open(mp4_with_moov_extra(&LC_ASC, 44_100, &[(10, 1024)], None, &extra));
        assert_eq!(reader.tracks().len(), 1);
        assert_eq!(reader.next_packet().unwrap().unwrap().data.len(), 4);
    }

    #[test]
    fn id32_box_in_udta_is_read() {
        let udta = atom(b"udta", &id32("In udta"));

        let mut reader = open(mp4_with_moov_extra(&LC_ASC, 44_100, &[(10, 1024)], None, &udta));
        assert_eq!(titles(&mut reader), ["In udta"]);
    }

    #[test]
    fn malformed_id32_box_is_ignored() {
        let mut body = vec![0; 4];
        body.extend_from_slice(&0x55c4u16.to_be_bytes());
        body.extend_from_slice(b"not an id3 tag");
        let udta = atom(b"udta", &atom(b"ID32", &body));

        let mut reader = open(mp4_with_moov_extra(&LC_ASC, 44_100, &[(10, 1024)], None, &udta));
        assert!(titles(&mut reader).is_empty());
        assert_eq!(reader.tracks().len(), 1);
    }

    fn open(data: Vec<u8>) -> IsoMp4Reader<'static> {
        let mss = MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default());
        IsoMp4Reader::try_new(mss, FormatOptions::default()).expect("mp4")
    }

    /// HE-AAC v1: 22.05 kHz stereo core, SBR at 44.1 kHz.
    const HE_AAC_ASC: [u8; 5] = [0x13, 0x90, 0x56, 0xe5, 0xa0];

    #[test]
    fn he_aac_timeline_is_in_decoded_frames() {
        // 200 samples of 1024 ticks, with an edit that removes 2048 ticks from the start and
        // 2048 from the end.
        let data = mp4(&HE_AAC_ASC, 22_050, &[(200, 1024)], Some((2048, 200 * 1024 - 4096)));
        let mut reader = open(data);

        let track = &reader.tracks()[0];
        let tb = track.time_base.expect("time base");
        assert_eq!((tb.numer.get(), tb.denom.get()), (1, 44_100));
        assert_eq!(track.duration.map(|d| d.get()), Some(200 * 2048));
        assert_eq!(track.delay, Some(4096));
        assert_eq!(track.padding, Some(4096));
        assert_eq!(track.num_frames, Some(200 * 2048 - 8192));

        // The first 2 samples (4096 frames) are the delay.
        let p = reader.next_packet().unwrap().unwrap();
        assert_eq!((p.pts.get(), p.dur.get(), p.trim_start.get()), (-4096, 0, 2048));
        let p = reader.next_packet().unwrap().unwrap();
        assert_eq!((p.pts.get(), p.dur.get(), p.trim_start.get()), (-2048, 0, 2048));
        let p = reader.next_packet().unwrap().unwrap();
        assert_eq!((p.pts.get(), p.dur.get(), p.trim_start.get()), (0, 2048, 0));
    }

    #[test]
    fn initial_empty_edit_offsets_the_timeline_and_seeks() {
        // 100 samples of 1024 frames at 44.1 kHz, preceded by 21026 frames of silence.
        let data =
            mp4_with_edits(&LC_ASC, 44_100, &[(100, 1024)], &[(-1, 21_026), (0, 100 * 1024)], &[]);
        let mut reader = open(data);

        assert_eq!(reader.tracks()[0].start_ts.get(), 21_026);

        let p = reader.next_packet().unwrap().unwrap();
        assert_eq!(p.pts.get(), 21_026);
        assert_eq!(p.trim_start.get(), 0);

        // A seek into the silence is the start of the media.
        let seeked = reader
            .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(100), track_id: 1 })
            .unwrap();
        assert_eq!(seeked.actual_ts.get(), 21_026);

        // A seek into the media.
        let ts = Timestamp::new(21_026 + 10 * 1024 + 5);
        let seeked =
            reader.seek(SeekMode::Accurate, SeekTo::Timestamp { ts, track_id: 1 }).unwrap();
        let p = reader.next_packet().unwrap().unwrap();
        assert_eq!(p.pts, seeked.actual_ts);
        assert!(seeked.actual_ts <= ts);
        assert!(seeked.actual_ts.get() > 21_026);
    }

    #[test]
    fn he_aac_seek_starts_before_the_target_at_an_aligned_sample() {
        let data = mp4(&HE_AAC_ASC, 22_050, &[(200, 1024)], Some((2048, 200 * 1024 - 4096)));
        let mut reader = open(data);

        // The position is on the timeline of the packets, which starts after the delay: 4 s is
        // sample (4 * 44100 + 4096) / 2048 = 88.
        let ts = Timestamp::new(4 * 44_100);

        for mode in [SeekMode::Accurate, SeekMode::Coarse] {
            let seeked = reader.seek(mode, SeekTo::Timestamp { ts, track_id: 1 }).unwrap();

            // Start at the sample that is a multiple of 16, at least 41 samples before: 32.
            assert_eq!(seeked.required_ts, ts);
            assert_eq!(seeked.actual_ts.get(), 32 * 2048 - 4096);

            let p = reader.next_packet().unwrap().unwrap();
            assert_eq!(p.pts, seeked.actual_ts);
            assert_eq!(&p.data[..1], &[32]);
        }

        // A seek to the start is the same as playing from the start.
        let seeked = reader
            .seek(SeekMode::Coarse, SeekTo::Timestamp { ts: Timestamp::new(0), track_id: 1 })
            .unwrap();
        assert_eq!(seeked.actual_ts.get(), -4096);
        assert_eq!(&reader.next_packet().unwrap().unwrap().data[..1], &[0]);
    }

    #[test]
    fn nero_shortened_leading_samples_encode_the_delay() {
        // Nero encodes the priming of HE-AAC without an edit list, as shortened leading samples:
        // 2048 frame samples of 0, 512, then full length ones, and a shortened last sample.
        let data = mp4(&HE_AAC_ASC, 44_100, &[(1, 0), (1, 512), (20, 2048), (1, 762)], None);
        let mut reader = open(data);

        let track = &reader.tracks()[0];
        assert_eq!(track.num_frames, Some(512 + 20 * 2048 + 762));

        // The valid frames of the decoded block are the last ones.
        let p = reader.next_packet().unwrap().unwrap();
        assert_eq!((p.pts.get(), p.dur.get(), p.trim_start.get()), (-2048, 0, 2048));
        let p = reader.next_packet().unwrap().unwrap();
        assert_eq!((p.pts.get(), p.dur.get(), p.trim_start.get()), (-1536, 512, 1536));
        // From then on the packets are placed at their timestamps.
        let p = reader.next_packet().unwrap().unwrap();
        assert_eq!((p.pts.get(), p.dur.get(), p.trim_start.get()), (512, 2048, 0));

        // The last sample is shortened at the end.
        let mut last = None;
        while let Some(p) = reader.next_packet().unwrap() {
            last = Some(p);
        }
        let last = last.unwrap();
        assert_eq!((last.dur.get(), last.trim_start.get(), last.trim_end.get()), (762, 0, 1286));
    }
}
