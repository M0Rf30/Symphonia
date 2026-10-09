// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::codecs::CodecParameters;
use symphonia_core::support_format;

use symphonia_core::codecs::audio::well_known::{
    CODEC_ID_AAC, CODEC_ID_ALAC, CODEC_ID_FLAC, CODEC_ID_OPUS,
};
use symphonia_core::errors::{
    Error, Result, SeekErrorKind, decode_error, seek_error, unsupported_error,
};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::formats::well_known::FORMAT_ID_ISOMP4;
use symphonia_core::io::*;
use symphonia_core::meta::{ChapterGroup, Metadata, MetadataLog};
use symphonia_core::units::Time;

use std::collections::HashMap;
use std::io::{Seek, SeekFrom};
use std::num::NonZero;
use std::sync::Arc;

use crate::atoms::{AtomError, AtomIterator, AtomType, ReadAtom};
use crate::atoms::{FtypAtom, GaplessInfo, MetaAtom, MoofAtom, MoovAtom, SidxAtom, TrakAtom};
use symphonia_common::mpeg::audio::{
    AAC_SEEK_MAX_PREROLL_FRAMES, AudioSpecificConfig, aac_seek_start_frame,
};

use crate::stream::*;

use log::{debug, info, trace, warn};

/// Opus seek pre-roll in frames at 48 kHz. The packets are not inspected (the sample table is
/// all that is known when seeking), so this is the pre-roll of a CELT-only stream, 1.5 s, which
/// brings a reset decoder to a bit-identical continuous decode (RFC 7845 section 4.6 only
/// mandates 80 ms). See `symphonia_common::xiph::audio::opus::SEEK_PREROLL_CELT`.
const OPUS_SEEK_PREROLL_FRAMES: u64 = symphonia_common::xiph::audio::opus::SEEK_PREROLL_CELT;

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
        timespan: &TimeSpan,
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
        track
            .with_time_base(TimeBase::from_recip(timespan.timescale))
            .with_duration(timespan.duration);

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
                Some(rate) if rate == timespan.timescale.get() => Some(1),
                Some(rate)
                    if audio.codec == CODEC_ID_AAC && rate % timespan.timescale.get() == 0 =>
                {
                    Some(u64::from(rate / timespan.timescale.get()))
                }
                _ => None,
            },
            _ => None,
        };

        if let Some(frames_per_tick) = frames_per_tick {
            // The total number of frames, including any delay and padding.
            let total_frames = timespan.duration.get().saturating_mul(frames_per_tick);

            // If a tick of the media timescale is more than one decoded frame (e.g., HE-AAC, where
            // the media timescale is the sample rate of the core codec), express the timeline of
            // the track in decoded frames: the same unit as the delay, padding, number of frames,
            // and the trim of packets.
            if frames_per_tick > 1 {
                if let Some(timescale) = u32::try_from(frames_per_tick)
                    .ok()
                    .and_then(|ratio| timespan.timescale.get().checked_mul(ratio))
                    .and_then(NonZero::new)
                {
                    track
                        .with_time_base(TimeBase::from_recip(timescale))
                        .with_duration(Duration::new(total_frames));
                }
            }

            track.with_num_frames(total_frames);

            // Derive gapless (encoder delay/padding) information for the track. An iTunes
            // `iTunSMPB` freeform tag, if present, takes precedence over the edit list since it is
            // the more explicit and widely honoured source (matching common player behaviour).
            // Both are expressed in media timescale ticks, whereas the delay and padding of a
            // track are decoded frames.
            let elst_gapless =
                trak.edts.as_ref().and_then(|edts| edts.elst.as_ref()).and_then(|elst| {
                    derive_gapless_from_elst(
                        elst,
                        timespan.duration.get(),
                        timespan.timescale,
                        movie_timescale,
                    )
                });

            let gapless = itunes_gapless.or(elst_gapless).and_then(|gapless| {
                let ratio = u32::try_from(frames_per_tick).ok()?;
                Some(GaplessInfo {
                    delay: gapless.delay.checked_mul(ratio)?,
                    padding: gapless.padding.checked_mul(ratio)?,
                })
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
                let discard = u64::from(gapless.delay) + u64::from(gapless.padding);
                let valid_frames = total_frames.saturating_sub(discard);
                track.with_num_frames(valid_frames);
            }
        }

        let state = Self {
            track_num,
            track_id: trak.tkhd.id,
            cur_seg: 0,
            next_sample: 0,
            next_sample_pos: 0,
            frames_per_tick: frames_per_tick.unwrap_or(1),
            media_time_base: TimeBase::from_recip(timespan.timescale),
            stts_nominal: nominal_sample_delta(trak),
            seek_preroll: SeekPreroll::new(&track, nominal_sample_delta(trak), timespan.timescale),
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
    /// [`aac_seek_start_frame`].
    Aac { sbr: bool },
    /// Opus. Start decoding a fixed number of packets before the target, covering at least
    /// [`OPUS_SEEK_PREROLL_FRAMES`] (1.5 s at 48 kHz).
    Opus { packets: u32 },
}

impl SeekPreroll {
    fn new(track: &Track, nominal_dur: Option<u64>, timescale: NonZero<u32>) -> Self {
        match &track.codec_params {
            Some(CodecParameters::Audio(audio)) if audio.codec == CODEC_ID_AAC => {
                let sbr = audio
                    .extra_data
                    .as_deref()
                    .and_then(|extra_data| AudioSpecificConfig::read(extra_data).ok())
                    .is_some_and(|asc| asc.sbr_present);

                SeekPreroll::Aac { sbr }
            }
            Some(CodecParameters::Audio(audio)) if audio.codec == CODEC_ID_OPUS => {
                // The pre-roll does not depend on the OpusHead pre-skip (which is excluded from
                // the seek timeline). Convert to packets using the nominal packet duration (in
                // ticks of the media timescale), assuming 20 ms packets if unknown.
                let ticks =
                    (u64::from(timescale.get()) * OPUS_SEEK_PREROLL_FRAMES).div_ceil(48_000);
                let nominal = nominal_dur.filter(|&d| d > 0).unwrap_or(ticks.div_ceil(4).max(1));
                let packets = ticks.div_ceil(nominal).min(u64::from(u16::MAX));
                SeekPreroll::Opus { packets: packets as u32 }
            }
            _ => SeekPreroll::None,
        }
    }

    /// Get the number of the sample to start decoding from to reach `target`, where `from_start`
    /// is true if the segment containing the target starts at the beginning of the track.
    fn start_sample(self, target: u32, from_start: bool) -> u32 {
        match self {
            SeekPreroll::None => target,
            SeekPreroll::Opus { packets } => target.saturating_sub(packets),
            // The state of an SBR decoder is a function of the frames since the start of the
            // stream (the noise phase), so audio is only correct if decoding starts at a frame
            // from which a reset decoder converges. Therefore, an SBR stream is pre-rolled
            // regardless of the seek mode.
            //
            // The pre-roll is aligned to the stream start, which is only known for a segment
            // that starts the track. Otherwise, use the maximum pre-roll.
            SeekPreroll::Aac { sbr: true } if !from_start => {
                target.saturating_sub(AAC_SEEK_MAX_PREROLL_FRAMES as u32)
            }
            SeekPreroll::Aac { sbr } => {
                u32::try_from(aac_seek_start_frame(u64::from(target), sbr)).unwrap_or(target)
            }
        }
    }
}

/// Derive gapless delay/padding from a single-entry edit list. Returns `None` for edit lists that
/// are empty, have more than one entry, or whose single entry is an "empty edit" (`media_time ==
/// -1`), none of which are handled generically here.
fn derive_gapless_from_elst(
    elst: &crate::atoms::ElstAtom,
    total_media_frames: u64,
    media_timescale: NonZero<u32>,
    movie_timescale: NonZero<u32>,
) -> Option<GaplessInfo> {
    if elst.entries.len() != 1 {
        return None;
    }

    let entry = &elst.entries[0];

    if entry.media_time < 0 {
        // An empty edit (or otherwise unsupported edit); ignore.
        return None;
    }

    let delay = entry.media_time as u64;

    // `segment_duration` is expressed in the *movie* timescale; convert it to the *media*
    // timescale (sample frames) to compute the padding.
    let seg_dur_media = (u128::from(entry.segment_duration) * u128::from(media_timescale.get()))
        / u128::from(movie_timescale.get());
    let seg_dur_media = u64::try_from(seg_dur_media).ok()?;

    let padding = total_media_frames.saturating_sub(delay).saturating_sub(seg_dur_media);

    let delay = u32::try_from(delay).ok()?;
    let padding = u32::try_from(padding).ok()?;

    Some(GaplessInfo { delay, padding })
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
        assert_eq!(p, SeekPreroll::Opus { packets: 75 });
        assert_eq!(p.start_sample(100, false), 25);
        assert_eq!(p.start_sample(2, true), 0);

        // 60 ms packets: 25 packets (1.5 s).
        assert_eq!(
            SeekPreroll::new(&opus_track(), Some(2880), ts),
            SeekPreroll::Opus { packets: 25 }
        );
        // 10 ms packets: 150 packets.
        assert_eq!(
            SeekPreroll::new(&opus_track(), Some(480), ts),
            SeekPreroll::Opus { packets: 150 }
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
    use super::derive_gapless_from_elst;
    use crate::atoms::{ElstAtom, ElstEntry};
    use std::num::NonZero;

    fn ts(hz: u32) -> NonZero<u32> {
        NonZero::new(hz).unwrap()
    }

    #[test]
    fn single_entry_delay_and_padding() {
        // media_timescale == movie_timescale (common case): no conversion needed.
        let elst = ElstAtom {
            entries: vec![ElstEntry {
                segment_duration: 100_000,
                media_time: 1024,
                media_rate_int: 1,
                media_rate_frac: 0,
            }],
        };

        // Total media frames = delay + segment_duration + padding.
        let total_media_frames = 1024 + 100_000 + 512;

        let info =
            derive_gapless_from_elst(&elst, total_media_frames, ts(44_100), ts(44_100)).unwrap();

        assert_eq!(info.delay, 1024);
        assert_eq!(info.padding, 512);
    }

    #[test]
    fn converts_segment_duration_across_timescales() {
        // Movie timescale of 600, media timescale of 44100 (a common combination).
        let movie_timescale = 600u32;
        let media_timescale = 44_100u32;

        let delay = 1024u64;
        let valid_media_frames = 44_100u64;
        let padding = 256u64;
        let total_media_frames = delay + valid_media_frames + padding;

        // segment_duration expressed in the movie timescale.
        let segment_duration =
            valid_media_frames * u64::from(movie_timescale) / u64::from(media_timescale);

        let elst = ElstAtom {
            entries: vec![ElstEntry {
                segment_duration,
                media_time: delay as i64,
                media_rate_int: 1,
                media_rate_frac: 0,
            }],
        };

        let info = derive_gapless_from_elst(
            &elst,
            total_media_frames,
            ts(media_timescale),
            ts(movie_timescale),
        )
        .unwrap();

        assert_eq!(info.delay, 1024);
        assert_eq!(info.padding, 256);
    }

    #[test]
    fn empty_edit_is_ignored() {
        let elst = ElstAtom {
            entries: vec![ElstEntry {
                segment_duration: 100,
                media_time: -1,
                media_rate_int: 1,
                media_rate_frac: 0,
            }],
        };

        assert!(derive_gapless_from_elst(&elst, 1000, ts(44_100), ts(44_100)).is_none());
    }

    #[test]
    fn multi_entry_edit_list_is_ignored() {
        let entry = ElstEntry {
            segment_duration: 100,
            media_time: 0,
            media_rate_int: 1,
            media_rate_frac: 0,
        };

        let elst = ElstAtom { entries: vec![entry.clone_for_test(), entry] };

        assert!(derive_gapless_from_elst(&elst, 1000, ts(44_100), ts(44_100)).is_none());
    }

    impl ElstEntry {
        fn clone_for_test(&self) -> Self {
            ElstEntry {
                segment_duration: self.segment_duration,
                media_time: self.media_time,
                media_rate_int: self.media_rate_int,
                media_rate_frac: self.media_rate_frac,
            }
        }
    }
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
#[derive(Debug)]
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
        // index.
        let mut sidx_timespans: HashMap<u32, TimeSpan> = HashMap::new();

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

                    // Calculate the total duration, per track, from the segment index atoms.
                    let sidx_timespan = sidx_timespans
                        .entry(sidx.reference_id)
                        .or_insert(TimeSpan::new(sidx.timescale, Duration::ZERO));

                    if sidx_timespan.timescale != sidx.timescale {
                        return unsupported_error(
                            "isomp4: different sidx timescale for the same track",
                        );
                    }

                    // Don't overflow.
                    // TODO: Duration should maybe be None since SIDX is non-authoritative.
                    sidx_timespan.duration = sidx_timespan
                        .duration
                        .checked_add(Duration::new(sidx.total_duration))
                        .ok_or(Error::DecodeError("isomp4: sidx total duration overflow"))?
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

        if let Some(rev) = moov.take_metadata() {
            metadata.push(rev);
        }

        // Create a track and track state for each Track (trak) atom.
        let mut tracks = Vec::with_capacity(moov.traks.len());
        let mut track_states = Vec::with_capacity(moov.traks.len());

        let itunes_gapless = moov.gapless();
        for (t, trak) in moov.traks.iter().enumerate() {
            // Determine the timespan of the track.
            let timespan = if moov.is_fragmented() {
                // If fragmented, prefer the duration from the sidx, if it is provided. Otherwise,
                // fallback to the mdhd.
                sidx_timespans
                    .get(&trak.tkhd.id)
                    .map(|sidx_tspan| TimeSpan::new(sidx_tspan.timescale, sidx_tspan.duration))
                    .unwrap_or_else(|| {
                        TimeSpan::new(trak.mdia.mdhd.timescale, trak.mdia.mdhd.duration.into())
                    })
            }
            else {
                // If non-fragmented, use the total duration (media timescale) from the track's
                // stts atom. Since edits are not currently supported, this is the duration of all
                // samples that will be yielded.
                //
                // TODO: Support edits. Once supported, prefer the tkhd duration.
                let duration = Duration::from(trak.mdia.minf.stbl.stts.total_duration);

                TimeSpan::new(trak.mdia.mdhd.timescale, duration)
            };

            let (track_state, track) =
                TrackState::make(t, trak, &timespan, moov.mvhd.timescale, itunes_gapless);

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
        media_info.with_duration(Duration::new(moov.mvhd.duration));

        let chapters = moov.chapters().or(opts.external_data.chapters);

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
                        let seg = MoofSegment::new(moof, self.moov.clone(), last_seg.as_ref());

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

        let media_ts = (ts.get() as u64).saturating_add(delay) / frames_per_tick;

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

        if delay == 0 && track.padding.unwrap_or(0) == 0 && !is_shortened {
            return Ok(Some(Packet::new(
                next_sample_info.track_id,
                media_pts,
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

        let packet = PacketBuilder::new()
            .track_id(next_sample_info.track_id)
            .pts(pts)
            .trimmed_dur(block_dur, end_pts)
            .data(data)
            .build();

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
            AtomError::UnexpectedUnknownSizeAtom => "isomp4: unknown size atom has sized parent",
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

            if let Some((media_time, segment_duration)) = elst {
                let mut elst = vec![0; 4];
                elst.extend_from_slice(&1u32.to_be_bytes());
                elst.extend_from_slice(&segment_duration.to_be_bytes());
                elst.extend_from_slice(&media_time.to_be_bytes());
                elst.extend_from_slice(&0x0001_0000u32.to_be_bytes());
                trak.push(atom(b"edts", &atom(b"elst", &elst)));
            }

            trak.push(mdia);

            let mut mvhd = vec![0; 4];
            mvhd.extend_from_slice(&[0; 8]);
            mvhd.extend_from_slice(&timescale.to_be_bytes());
            mvhd.extend_from_slice(&duration.to_be_bytes());
            mvhd.extend_from_slice(&0x0001_0000u32.to_be_bytes());
            mvhd.extend_from_slice(&0x0100u16.to_be_bytes());

            let moov =
                atom(b"moov", &[atom(b"mvhd", &mvhd), atom(b"trak", &trak.concat())].concat());

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
