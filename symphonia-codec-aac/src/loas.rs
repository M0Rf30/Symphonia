// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::VecDeque;
use std::io::{Seek, SeekFrom};

use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::codecs::audio::well_known::CODEC_ID_AAC;
use symphonia_core::errors::{
    Error, Result, SeekErrorKind, decode_error, seek_error, unsupported_error,
};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::formats::well_known::FORMAT_ID_LOAS;
use symphonia_core::io::*;
use symphonia_core::meta::{Metadata, MetadataLog};
use symphonia_core::support_format;

use symphonia_common::mpeg::audio::latm::{StreamMuxConfig, read_audio_mux_element};
use symphonia_common::mpeg::audio::*;

use log::{debug, info};

use crate::implicit_sbr::{
    MAX_IMPLICIT_SBR_CORE_RATE, MAX_PROBE_BLOCKS, detect, may_have_implicit_sbr, with_explicit_sbr,
};

/// The 11-bit LOAS sync word `0x2b7` and the 13 bit length that follows it.
const LOAS_SYNC_MASK: u16 = 0xffe0;
const LOAS_SYNC: u16 = 0x56e0;
const LOAS_MAX_FRAME_LEN: usize = 0x1fff;
/// The length of the sync word and the frame length.
const LOAS_HEADER_LEN: u64 = 3;

/// The maximum number of frames to search for a stream mux config when opening a stream.
const MAX_CONFIG_SEARCH_FRAMES: usize = 32;

const LOAS_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FORMAT_ID_LOAS,
    short_name: "loas",
    long_name: "Low Overhead Audio Stream (AAC in LATM)",
};

/// Read the sync word and length of a LOAS frame, resynchronising to the next sync word if the
/// reader is not at one. Returns the length of the frame's audio mux element in bytes.
fn read_frame_len<B: ReadBytes>(reader: &mut B) -> Result<usize> {
    let mut sync = 0u16;

    while sync & LOAS_SYNC_MASK != LOAS_SYNC {
        sync = (sync << 8) | u16::from(reader.read_u8()?);
    }

    // The sync word is followed by a 13-bit length, of which the first 5 bits are in the second
    // byte of the sync word.
    Ok(usize::from(sync & 0x1f) << 8 | usize::from(reader.read_u8()?))
}

/// Low Overhead Audio Stream (LOAS) format reader.
///
/// `LoasReader` implements a demuxer for AAC in LATM with LOAS framing (`AudioSyncStream()`), as
/// used by DVB and DAB+ and some internet radio streams. A single program with a single layer is
/// supported.
pub struct LoasReader<'s> {
    reader: MediaSourceStream<'s>,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    chapters: Option<ChapterGroup>,
    metadata: MetadataLog,
    first_frame_pos: u64,
    next_packet_ts: Timestamp,
    config: Option<StreamMuxConfig>,
    pending: VecDeque<Packet>,
    /// True if the stream uses SBR.
    sbr: bool,
    /// The number of previous frames the output of a frame depends on.
    overlap: u64,
    /// The period of the SBR phase in frames.
    phase_period: u64,
    /// The duration of a packet in decoded frames: 1024 per frame of the core codec, doubled for
    /// dual-rate SBR.
    packet_dur: Duration,
}

impl<'s> LoasReader<'s> {
    pub fn try_new(mut mss: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
        let first_frame_pos = mss.pos();

        let mut config = None;
        let mut payloads: Vec<Box<[u8]>> = Vec::new();

        // Read frames until the stream mux config is found. The frames before it are kept: they
        // use the same config if they are complete.
        let mut early_frames = vec![];

        for _ in 0..MAX_CONFIG_SEARCH_FRAMES {
            let len = read_frame_len(&mut mss)?;
            let data = mss.read_boxed_slice_exact(len)?;

            let frame_payloads = match read_audio_mux_element(&data, &mut config) {
                Ok(payloads) => payloads,
                // A truncated frame at the start of the stream.
                Err(_) if payloads.is_empty() => vec![],
                Err(err) => return Err(err),
            };

            if config.is_none() {
                early_frames.push(data);
                continue;
            }

            // Frames before the one with the config.
            for early in early_frames.drain(..) {
                if let Ok(early_payloads) = read_audio_mux_element(&early, &mut config) {
                    payloads.extend(early_payloads);
                }
            }

            payloads.extend(frame_payloads);
            break;
        }

        let Some(config) = config
        else {
            return decode_error("loas: no stream mux config found");
        };

        // The sample rate of the core codec is the timebase, whereas the codec parameters
        // describe the decoded output.
        if config.asc.sample_rate == 0 {
            return decode_error("loas: invalid sample rate");
        }

        // The stream mux config cannot signal SBR or parametric stereo of an AAC-LC stream
        // implicitly (HE-AAC without the extension in the audio specific config): look for them
        // in the first payloads, reading ahead as many frames as required.
        let mut asc = config.asc.clone();
        let mut extra_data = config.extra_data.clone();

        let mut stream_config = Some(config.clone());

        if may_have_implicit_sbr(&asc) {
            while payloads.len() < MAX_PROBE_BLOCKS {
                let Ok(len) = read_frame_len(&mut mss)
                else {
                    break;
                };

                let Ok(data) = mss.read_boxed_slice_exact(len)
                else {
                    break;
                };

                match read_audio_mux_element(&data, &mut stream_config) {
                    Ok(more) => payloads.extend(more),
                    Err(_) => break,
                }
            }

            let ext = detect(&config.extra_data, asc.sample_rate, payloads.iter().map(|p| &p[..]));

            if ext.sbr {
                if let Some(new_extra_data) =
                    with_explicit_sbr(&config.extra_data, asc.sample_rate.saturating_mul(2), ext.ps)
                {
                    if let Ok(new_asc) = AudioSpecificConfig::read(&new_extra_data) {
                        info!(
                            "loas: stream has {}, output is {} Hz",
                            if ext.ps { "sbr and parametric stereo" } else { "sbr" },
                            new_asc.output_sample_rate()
                        );

                        asc = new_asc;
                        extra_data = new_extra_data;
                    }
                }
            }
        }

        // The timeline of the track is in decoded frames, which are `ratio` per frame of the core
        // codec.
        let ratio = u64::from(asc.output_sample_rate() / asc.sample_rate).max(1);
        let packet_dur = Duration::new(asc.samples as u64 * ratio);

        let mut codec_params = AudioCodecParameters::new();

        codec_params
            .for_codec(CODEC_ID_AAC)
            .with_sample_rate(asc.output_sample_rate())
            .with_extra_data(extra_data);

        if let Some(channels) = asc.output_channels() {
            codec_params.with_channels(channels);
        }

        if let Some(profile) = get_audio_codec_profile(&asc) {
            codec_params.with_profile(profile);
        }

        let mut track = Track::new(0);
        track.with_codec_params(CodecParameters::Audio(codec_params));

        // The frames of the stream up to the first one with a stream mux config were read.
        // Estimate the duration from the average frame size.
        if let Some(n_frames) = approximate_frame_count(&mut mss, first_frame_pos)? {
            let n_frames = n_frames * (config.num_sub_frames as u64 + 1);
            let n_frames = n_frames * packet_dur.get();

            info!("estimating duration from bitrate, may be inaccurate for vbr streams");

            track.with_duration(Duration::new(n_frames));
            track.with_num_frames(n_frames);
        }

        // Build the packets that were read.
        let mut pending = VecDeque::with_capacity(payloads.len());
        let mut next_packet_ts = Timestamp::new(0);

        for payload in payloads {
            pending.push_back(Packet::new(0, next_packet_ts, packet_dur, payload));
            next_packet_ts = next_packet_ts.saturating_add(packet_dur);
        }

        Ok(LoasReader {
            reader: mss,
            media_info: MediaInfo::from_track(&track),
            tracks: vec![track],
            chapters: opts.external_data.chapters,
            metadata: opts.external_data.metadata.unwrap_or_default(),
            first_frame_pos,
            next_packet_ts,
            sbr: asc.sbr_present,
            overlap: aac_overlap_frames(asc.object_type),
            phase_period: aac_sbr_phase_period(&asc),
            config: stream_config,
            pending,
            packet_dur,
        })
    }

    /// Returns true if the stream may use SBR.
    fn may_use_sbr(&self) -> bool {
        self.sbr
            || self.config.as_ref().is_some_and(|c| {
                c.asc.sbr_present || c.asc.sample_rate <= MAX_IMPLICIT_SBR_CORE_RATE
            })
    }
}

/// Estimate the number of audio mux elements in the stream from the average length of the first
/// frames, and the length of the stream.
fn approximate_frame_count(
    mss: &mut MediaSourceStream<'_>,
    first_frame_pos: u64,
) -> Result<Option<u64>> {
    let Some(byte_len) = mss.byte_len()
    else {
        return Ok(None);
    };

    if !mss.is_seekable() {
        return Ok(None);
    }

    let original_pos = mss.pos();

    let mut n_frames = 0u64;
    let mut n_bytes = 0u64;

    mss.seek(SeekFrom::Start(first_frame_pos))?;

    while n_frames < 100 {
        let Ok(len) = read_frame_len(mss)
        else {
            break;
        };

        if mss.ignore_bytes(len as u64).is_err() {
            break;
        }

        n_frames += 1;
        n_bytes += len as u64 + LOAS_HEADER_LEN;
    }

    mss.seek(SeekFrom::Start(original_pos))?;

    if n_frames == 0 {
        return Ok(None);
    }

    Ok(Some(byte_len.saturating_sub(first_frame_pos) * n_frames / n_bytes))
}

impl Scoreable for LoasReader<'_> {
    fn score(mut src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        // The stream must start with a sync word.
        let sync = src.read_be_u16()?;

        if sync & LOAS_SYNC_MASK != LOAS_SYNC {
            return Ok(Score::Unsupported);
        }

        // The 11-bit sync word has little structure, and the first frame of a stream may also be a
        // continuation of a stream mux config that was not seen. A single valid-looking frame is
        // therefore not enough to claim a stream: a stray sync word in a tag or in other data that
        // precedes the audio of another format would otherwise be selected before that format is
        // found. The frame must be followed by another sync word.
        let len = usize::from(sync & 0x1f) << 8 | usize::from(src.read_u8()?);

        if len == 0 || len > LOAS_MAX_FRAME_LEN || src.bytes_available() < len as u64 {
            return Ok(Score::Unsupported);
        }

        let data = src.read_boxed_slice_exact(len)?;

        // The first frame must carry a valid stream mux config, or be a continuation.
        let mut config = None;
        let _ = read_audio_mux_element(&data, &mut config)?;

        match src.read_be_u16() {
            Ok(sync) if sync & LOAS_SYNC_MASK == LOAS_SYNC => {
                Ok(Score::Supported(if config.is_some() { 255 } else { 96 }))
            }
            // A single frame at the end of the stream.
            Err(_) if config.is_some() => Ok(Score::Supported(127)),
            _ => Ok(Score::Unsupported),
        }
    }
}

impl ProbeableFormat<'_> for LoasReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + '_>> {
        Ok(Box::new(LoasReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(
            LOAS_FORMAT_INFO,
            &["loas", "latm", "aac"],
            &["audio/aac", "audio/mp4a-latm"],
            &[
                // The sync word is 0x2b7 followed by the high 5 bits of the frame length.
                &[0x56, 0xe0],
                &[0x56, 0xe1],
                &[0x56, 0xe2],
                &[0x56, 0xe3],
                &[0x56, 0xe4],
                &[0x56, 0xe5],
                &[0x56, 0xe6],
                &[0x56, 0xe7],
                &[0x56, 0xe8],
                &[0x56, 0xe9],
                &[0x56, 0xea],
                &[0x56, 0xeb],
                &[0x56, 0xec],
                &[0x56, 0xed],
                &[0x56, 0xee],
                &[0x56, 0xef],
                &[0x56, 0xf0],
                &[0x56, 0xf1],
                &[0x56, 0xf2],
                &[0x56, 0xf3],
                &[0x56, 0xf4],
                &[0x56, 0xf5],
                &[0x56, 0xf6],
                &[0x56, 0xf7],
                &[0x56, 0xf8],
                &[0x56, 0xf9],
                &[0x56, 0xfa],
                &[0x56, 0xfb],
                &[0x56, 0xfc],
                &[0x56, 0xfd],
                &[0x56, 0xfe],
                &[0x56, 0xff],
            ]
        )]
    }
}

impl FormatReader for LoasReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &LOAS_FORMAT_INFO
    }

    fn media_info(&self) -> &MediaInfo {
        &self.media_info
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        loop {
            if let Some(packet) = self.pending.pop_front() {
                return Ok(Some(packet));
            }

            let len = match read_frame_len(&mut self.reader) {
                Ok(len) => len,
                Err(Error::IoError(err)) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                    // LOAS streams have no well-defined end, so when no more frames can be read,
                    // consider the stream ended.
                    return Ok(None);
                }
                Err(err) => return Err(err),
            };

            let data = match self.reader.read_boxed_slice_exact(len) {
                Ok(data) => data,
                Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
                Err(err) => return Err(err.into()),
            };

            for payload in read_audio_mux_element(&data, &mut self.config)? {
                self.pending.push_back(Packet::new(
                    0,
                    self.next_packet_ts,
                    self.packet_dur,
                    payload,
                ));

                self.next_packet_ts = match self.next_packet_ts.checked_add(self.packet_dur) {
                    Some(ts) => ts,
                    None => return Ok(None),
                };
            }
        }
    }

    fn metadata(&mut self) -> Metadata<'_> {
        self.metadata.metadata()
    }

    fn chapters(&self) -> Option<&ChapterGroup> {
        self.chapters.as_ref()
    }

    fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    fn seek(&mut self, _mode: SeekMode, to: SeekTo) -> Result<SeekedTo> {
        // Only streams with a single payload per frame can be seeked by frame.
        if !matches!(&self.config, Some(config) if config.num_sub_frames == 0) {
            return seek_error(SeekErrorKind::Unseekable);
        }

        // Get the timestamp of the desired audio frame.
        let required_ts = match to {
            SeekTo::Timestamp { ts, .. } => ts,
            SeekTo::Time { time, .. } => {
                let tb =
                    self.tracks[0].time_base.ok_or(Error::SeekError(SeekErrorKind::Unseekable))?;

                tb.calc_timestamp(time).ok_or(Error::SeekError(SeekErrorKind::OutOfRange))?
            }
        };

        debug!("seeking to ts={required_ts}");

        // Packets that were already parsed are no longer the next ones. The reader and the next
        // packet timestamp are positioned after them.
        self.pending.clear();

        // The frame to start decoding from. For a decoder to reproduce a continuous decode at the
        // required timestamp, it must be fed some frames before it (MDCT overlap, SBR state).
        // This applies to coarse seeks too: a decoder started at the required timestamp outputs
        // wrong audio, and, with SBR, never recovers the right noise phase.
        let sbr = self.may_use_sbr();
        let packet_dur = self.packet_dur.get();
        let required_frame = u64::try_from(required_ts.get()).unwrap_or(0) / packet_dur;
        let mut start_ts = Timestamp::new(
            (aac_seek_start_frame_with_period(required_frame, sbr, self.overlap, self.phase_period)
                * packet_dur) as i64,
        );

        // If the frame to start from is before the next packet, attempt to seek to the start of
        // the stream.
        if start_ts < self.next_packet_ts {
            if self.reader.is_seekable() {
                self.reader.seek(SeekFrom::Start(self.first_frame_pos))?;
                self.next_packet_ts = Timestamp::new(0);
            }
            else if required_ts < self.next_packet_ts {
                return seek_error(SeekErrorKind::ForwardOnly);
            }
            else {
                start_ts = self.next_packet_ts;
            }
        }

        // Parse frames from the stream until the frame to start from is reached.
        loop {
            let len = match read_frame_len(&mut self.reader) {
                Ok(len) => len,
                Err(Error::IoError(err)) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return seek_error(SeekErrorKind::OutOfRange);
                }
                Err(err) => return Err(err),
            };

            let next_packet_ts = match self.next_packet_ts.checked_add(self.packet_dur) {
                Some(ts) if ts <= start_ts => ts,
                // The frame contains the timestamp to start from: rewind to its start.
                _ => {
                    self.reader.seek_buffered_rev(LOAS_HEADER_LEN as usize);
                    break;
                }
            };

            self.reader.ignore_bytes(len as u64)?;
            self.next_packet_ts = next_packet_ts;
        }

        debug!(
            "seeked to ts={} (delta={})",
            self.next_packet_ts,
            required_ts.saturating_delta(self.next_packet_ts),
        );

        Ok(SeekedTo { track_id: 0, required_ts, actual_ts: self.next_packet_ts })
    }

    fn into_inner<'s>(self: Box<Self>) -> MediaSourceStream<'s>
    where
        Self: 's,
    {
        self.reader
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A MSB-first bit writer.
    #[derive(Default)]
    struct BitWriter {
        bytes: Vec<u8>,
        n_bits: usize,
    }

    impl BitWriter {
        fn put(&mut self, value: u32, n: usize) {
            for i in (0..n).rev() {
                if self.n_bits % 8 == 0 {
                    self.bytes.push(0);
                }
                *self.bytes.last_mut().unwrap() |=
                    (((value >> i) & 1) as u8) << (7 - self.n_bits % 8);
                self.n_bits += 1;
            }
        }
    }

    /// Build an audio mux element for AAC-LC 48 kHz stereo (audioMuxVersion 0), carrying the
    /// given raw data block(s), one per sub-frame.
    fn mux_element(with_config: bool, num_sub_frames: u32, blocks: &[&[u8]]) -> Vec<u8> {
        let mut bw = BitWriter::default();

        // useSameStreamMux
        bw.put(!with_config as u32, 1);

        if with_config {
            bw.put(0, 1); // audioMuxVersion
            bw.put(1, 1); // allStreamsSameTimeFraming
            bw.put(num_sub_frames, 6);
            bw.put(0, 4); // numProgram - 1
            bw.put(0, 3); // numLayer - 1
            // AudioSpecificConfig: AAC-LC, 48 kHz, stereo.
            bw.put(2, 5);
            bw.put(3, 4);
            bw.put(2, 4);
            bw.put(0, 3);
            bw.put(0, 3); // frameLengthType
            bw.put(0xff, 8); // latmBufferFullness
            bw.put(0, 1); // otherDataPresent
            bw.put(0, 1); // crcCheckPresent
        }

        for block in blocks {
            // PayloadLengthInfo
            let mut len = block.len();
            while len >= 255 {
                bw.put(255, 8);
                len -= 255;
            }
            bw.put(len as u32, 8);

            for &byte in *block {
                bw.put(u32::from(byte), 8);
            }
        }

        bw.bytes
    }

    fn loas_frame(element: &[u8]) -> Vec<u8> {
        let len = element.len() as u16;
        let mut frame = vec![0x56, 0xe0 | (len >> 8) as u8, len as u8];
        frame.extend_from_slice(element);
        frame
    }

    #[test]
    fn reads_stream_mux_config_and_payloads() {
        let block_a: Vec<u8> = (0..20).collect();
        let block_b: Vec<u8> = (100..400).map(|v| v as u8).collect(); // Longer than 255 bytes.

        let mut config = None;
        let payloads =
            read_audio_mux_element(&mux_element(true, 0, &[&block_a]), &mut config).unwrap();

        let config = config.as_ref().expect("stream mux config");
        assert_eq!(config.asc.sample_rate, 48_000);
        assert_eq!(config.asc.channels.as_ref().map(|c| c.count()), Some(2));
        // The audio specific config is an AAC-LC, 48 kHz, stereo.
        assert_eq!(&config.extra_data[..], &[0x11, 0x90]);
        assert_eq!(payloads, vec![block_a.clone().into_boxed_slice()]);

        // A continuation of the same stream mux.
        let mut config = Some(config.clone());
        let payloads =
            read_audio_mux_element(&mux_element(false, 0, &[&block_b]), &mut config).unwrap();
        assert_eq!(payloads, vec![block_b.into_boxed_slice()]);
    }

    /// Scores the data. Scoring errors (e.g. a frame running past the end of the data) are
    /// treated as unsupported by the probe.
    fn score_of(data: Vec<u8>) -> Score {
        let mut mss =
            MediaSourceStream::new(Box::new(std::io::Cursor::new(data)), Default::default());
        LoasReader::score(ScopedStream::new(&mut mss, 16 * 1024)).unwrap_or(Score::Unsupported)
    }

    #[test]
    fn scores_a_stream_of_frames() {
        let element = mux_element(true, 0, &[&[1, 2, 3, 4]]);
        let mut data = loas_frame(&element);
        data.extend(loas_frame(&mux_element(false, 0, &[&[5, 6, 7]])));
        assert!(matches!(score_of(data), Score::Supported(255)));
    }

    #[test]
    fn scores_a_single_frame_at_the_end_of_the_stream() {
        let data = loas_frame(&mux_element(true, 0, &[&[1, 2, 3, 4]]));
        assert!(matches!(score_of(data), Score::Supported(127)));
    }

    #[test]
    fn stray_sync_word_is_not_a_stream() {
        // A sync word that is followed by a plausible continuation frame, but no other sync word,
        // as found in the data of a tag preceding the audio of another format.
        let mut data = loas_frame(&mux_element(false, 0, &[&[1, 2, 3, 4]]));
        data.extend_from_slice(&[0u8; 64]);
        assert!(matches!(score_of(data), Score::Unsupported));

        // A zero length frame.
        assert!(matches!(score_of(vec![0x56, 0xe0, 0x00, 0x01, 0x02, 0x03]), Score::Unsupported));

        // A frame that is longer than the available data.
        assert!(matches!(score_of(vec![0x56, 0xe1, 0xff, 0x01, 0x02, 0x03]), Score::Unsupported));
    }

    #[test]
    fn reads_multiple_sub_frames() {
        let mut config = None;
        let payloads =
            read_audio_mux_element(&mux_element(true, 1, &[&[1, 2, 3], &[4, 5]]), &mut config)
                .unwrap();
        assert_eq!(payloads.len(), 2);
        assert_eq!(&payloads[0][..], &[1, 2, 3]);
        assert_eq!(&payloads[1][..], &[4, 5]);
    }

    #[test]
    fn truncated_payload_is_an_error() {
        let mut element = mux_element(true, 0, &[&[1, 2, 3, 4, 5, 6, 7, 8]]);
        element.truncate(element.len() - 4);

        let mut config = None;
        assert!(read_audio_mux_element(&element, &mut config).is_err());
    }

    #[test]
    fn element_without_a_config_yields_no_payloads_until_one_is_seen() {
        let mut config = None;
        let payloads =
            read_audio_mux_element(&mux_element(false, 0, &[&[1, 2, 3]]), &mut config).unwrap();
        assert!(payloads.is_empty());
        assert!(config.is_none());
    }

    #[test]
    fn demuxes_a_loas_stream() {
        let mut data = vec![];
        // The first frame does not carry the config, but is read with the one that follows.
        data.extend(loas_frame(&mux_element(false, 0, &[&[9, 9, 9]])));
        data.extend(loas_frame(&mux_element(true, 0, &[&[1, 2, 3]])));
        data.extend(loas_frame(&mux_element(false, 0, &[&[4, 5, 6, 7]])));
        data.extend(loas_frame(&mux_element(false, 0, &[&[8]])));

        let mss = MediaSourceStream::new(Box::new(std::io::Cursor::new(data)), Default::default());
        let mut reader = LoasReader::try_new(mss, Default::default()).expect("loas stream");

        let track = &reader.tracks()[0];
        assert!(matches!(
            &track.codec_params,
            Some(CodecParameters::Audio(params)) if params.sample_rate == Some(48_000)
        ));

        let mut blocks = vec![];
        while let Some(packet) = reader.next_packet().unwrap() {
            blocks.push((packet.pts.get(), packet.data.to_vec()));
        }

        assert_eq!(
            blocks,
            vec![
                (0, vec![9, 9, 9]),
                (1024, vec![1, 2, 3]),
                (2048, vec![4, 5, 6, 7]),
                (3072, vec![8])
            ]
        );

        // Seek back to the second frame. An accurate seek of AAC-LC starts 1 frame early.
        let seeked = reader
            .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(1500), track_id: 0 })
            .unwrap();
        assert_eq!(seeked.actual_ts.get(), 0);
        assert_eq!(reader.next_packet().unwrap().unwrap().pts.get(), 0);
    }
}
