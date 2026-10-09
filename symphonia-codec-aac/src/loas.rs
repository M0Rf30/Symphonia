// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::VecDeque;
use std::io::{Seek, SeekFrom};
use std::num::NonZero;

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

use symphonia_common::mpeg::audio::*;

use log::{debug, info};

/// The number of samples per AAC frame (at the core sample rate).
const SAMPLES_PER_AAC_PACKET: Duration = Duration::new(1024);

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

/// The parts of a LATM `StreamMuxConfig()` that are required to demultiplex a single program and
/// layer.
#[derive(Clone, Debug)]
struct StreamMuxConfig {
    /// The audio specific config of the stream.
    asc: AudioSpecificConfig,
    /// The audio specific config as bytes, for the decoder.
    extra_data: Box<[u8]>,
    /// The number of sub-frames (payloads) in each audio mux element, minus 1.
    num_sub_frames: usize,
    /// `frameLengthType` of the stream.
    frame_length_type: u8,
    /// `frameLength` of the stream, if `frame_length_type` is 1.
    frame_length: usize,
}

impl StreamMuxConfig {
    /// Read a `StreamMuxConfig()` (ISO/IEC 14496-3 §1.7.3.1). Only streams of a single program
    /// and layer are supported.
    fn read<B: ReadBitsLtr + FiniteBitStream>(bs: &mut B, data: &[u8]) -> Result<Self> {
        let audio_mux_version = bs.read_bool()?;

        // audioMuxVersionA is reserved for future use.
        if audio_mux_version && bs.read_bool()? {
            return unsupported_error("loas: unsupported audio mux version");
        }

        if audio_mux_version {
            // taraBufferFullness
            let _ = Self::read_latm_value(bs)?;
        }

        // allStreamsSameTimeFraming
        let _ = bs.read_bool()?;
        let num_sub_frames = bs.read_bits_leq32(6)? as usize;
        let num_program = bs.read_bits_leq32(4)? + 1;
        let num_layer = bs.read_bits_leq32(3)? + 1;

        if num_program != 1 || num_layer != 1 {
            return unsupported_error("loas: only a single program and layer is supported");
        }

        // The first (and only) program and layer always has its own audio specific config.
        let asc_len = if audio_mux_version { Some(Self::read_latm_value(bs)?) } else { None };

        let asc_start = (data.len() * 8) as u64 - bs.bits_left();
        let mut asc = AudioSpecificConfig::read_core_from(bs)?;

        // The audio specific config is not the last element of the stream mux config, so look for
        // its optional extension without consuming the bits after it.
        let mut peek = BitReaderLtr::new(data);
        peek.ignore_bits(((data.len() * 8) as u64 - bs.bits_left()) as u32)?;
        let ext_bits = asc.read_sync_extension(&mut peek)?;
        bs.ignore_bits(ext_bits as u32)?;

        let asc_end = (data.len() * 8) as u64 - bs.bits_left();

        let mut asc_bits = asc_end - asc_start;

        if let Some(asc_len) = asc_len {
            // The audio specific config is followed by fill bits up to its signalled length.
            let asc_len = u64::from(asc_len);

            if asc_len < asc_bits {
                return decode_error("loas: invalid audio specific config length");
            }

            bs.ignore_bits((asc_len - asc_bits) as u32)?;
            asc_bits = asc_len;
        }

        let extra_data = copy_bits(data, asc_start as usize, asc_bits as usize);

        let frame_length_type = bs.read_bits_leq32(3)? as u8;
        let mut frame_length = 0;

        match frame_length_type {
            0 => {
                // latmBufferFullness
                let _ = bs.read_bits_leq32(8)?;
            }
            1 => {
                // The payload length is 20 bytes plus the frame length.
                frame_length = bs.read_bits_leq32(9)? as usize + 20;
            }
            _ => return unsupported_error("loas: unsupported frame length type"),
        }

        // otherDataPresent
        if bs.read_bool()? {
            if audio_mux_version {
                let _ = Self::read_latm_value(bs)?;
            }
            else {
                loop {
                    let escape = bs.read_bool()?;
                    let _ = bs.read_bits_leq32(8)?;

                    if !escape {
                        break;
                    }
                }
            }
        }

        // crcCheckPresent
        if bs.read_bool()? {
            let _crc = bs.read_bits_leq32(8)?;
        }

        // Validate the audio specific config is something that can be decoded as a LATM stream.
        if asc.channels.is_none() {
            return decode_error("loas: missing channel configuration");
        }

        Ok(StreamMuxConfig { asc, extra_data, num_sub_frames, frame_length_type, frame_length })
    }

    /// Read a `LatmGetValue()`.
    fn read_latm_value<B: ReadBitsLtr>(bs: &mut B) -> Result<u32> {
        let num_bytes = bs.read_bits_leq32(2)? + 1;
        let mut value = 0u32;

        for _ in 0..num_bytes {
            value = (value << 8) | bs.read_bits_leq32(8)?;
        }

        Ok(value)
    }
}

/// Copy `n_bits` bits from `data` starting at the bit offset `bit_offset` into a new
/// byte-aligned buffer.
fn copy_bits(data: &[u8], bit_offset: usize, n_bits: usize) -> Box<[u8]> {
    let mut out = vec![0u8; n_bits.div_ceil(8)];

    for i in 0..n_bits {
        let src = bit_offset + i;
        let bit = (data[src / 8] >> (7 - src % 8)) & 1;
        out[i / 8] |= bit << (7 - i % 8);
    }

    out.into_boxed_slice()
}

/// Read an `AudioMuxElement(1)` (ISO/IEC 14496-3 §1.7.3.2) from the body of a LOAS frame. If the
/// element carries a stream mux config, it replaces `config`. Returns the raw data block of every
/// sub-frame in the element.
fn read_audio_mux_element(
    data: &[u8],
    config: &mut Option<StreamMuxConfig>,
) -> Result<Vec<Box<[u8]>>> {
    let mut bs = BitReaderLtr::new(data);

    let use_same_stream_mux = bs.read_bool()?;

    if !use_same_stream_mux {
        *config = Some(StreamMuxConfig::read(&mut bs, data)?);
    }

    // A stream mux config must have been received.
    let Some(config) = config.as_ref()
    else {
        return Ok(vec![]);
    };

    let mut payloads = Vec::with_capacity(config.num_sub_frames + 1);

    for _ in 0..=config.num_sub_frames {
        // PayloadLengthInfo()
        let len = match config.frame_length_type {
            0 => {
                let mut len = 0usize;

                loop {
                    let tmp = bs.read_bits_leq32(8)?;
                    len += tmp as usize;

                    if tmp != 255 {
                        break;
                    }
                }

                len
            }
            _ => config.frame_length,
        };

        // PayloadMux(): the raw data block is not necessarily byte-aligned in the mux element.
        if bs.bits_left() < (len as u64) * 8 {
            return decode_error("loas: payload exceeds the audio mux element");
        }

        let mut payload = vec![0u8; len];

        for byte in payload.iter_mut() {
            *byte = bs.read_bits_leq32(8)? as u8;
        }

        payloads.push(payload.into_boxed_slice());
    }

    Ok(payloads)
}

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
}

impl<'s> LoasReader<'s> {
    pub fn try_new(mut mss: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
        let first_frame_pos = mss.pos();

        let mut config = None;
        let mut pending = VecDeque::new();
        let mut next_packet_ts = Timestamp::new(0);

        // Read frames until the stream mux config is found. The frames before it are kept: they
        // use the same config if they are complete.
        let mut early_frames = vec![];

        for _ in 0..MAX_CONFIG_SEARCH_FRAMES {
            let len = read_frame_len(&mut mss)?;
            let data = mss.read_boxed_slice_exact(len)?;

            let payloads = match read_audio_mux_element(&data, &mut config) {
                Ok(payloads) => payloads,
                // A truncated frame at the start of the stream.
                Err(_) if pending.is_empty() => vec![],
                Err(err) => return Err(err),
            };

            if config.is_none() {
                early_frames.push(data);
                continue;
            }

            // Frames before the one with the config.
            for early in early_frames.drain(..) {
                if let Ok(payloads) = read_audio_mux_element(&early, &mut config) {
                    for payload in payloads {
                        pending.push_back(Packet::new(
                            0,
                            next_packet_ts,
                            SAMPLES_PER_AAC_PACKET,
                            payload,
                        ));
                        next_packet_ts = next_packet_ts.saturating_add(SAMPLES_PER_AAC_PACKET);
                    }
                }
            }

            for payload in payloads {
                pending.push_back(Packet::new(0, next_packet_ts, SAMPLES_PER_AAC_PACKET, payload));
                next_packet_ts = next_packet_ts.saturating_add(SAMPLES_PER_AAC_PACKET);
            }

            break;
        }

        let Some(config) = config
        else {
            return decode_error("loas: no stream mux config found");
        };

        // The sample rate of the core codec is the timebase, whereas the codec parameters
        // describe the decoded output.
        let core_rate = NonZero::new(config.asc.sample_rate)
            .ok_or(Error::DecodeError("loas: invalid sample rate"))?;

        let mut codec_params = AudioCodecParameters::new();

        codec_params
            .for_codec(CODEC_ID_AAC)
            .with_sample_rate(config.asc.output_sample_rate())
            .with_extra_data(config.extra_data.clone());

        if let Some(channels) = config.asc.output_channels() {
            codec_params.with_channels(channels);
        }

        if let Some(profile) = get_audio_codec_profile(&config.asc) {
            codec_params.with_profile(profile);
        }

        let mut track = Track::new(0);
        track.with_codec_params(CodecParameters::Audio(codec_params));
        track.with_time_base(TimeBase::from_recip(core_rate));

        // The frames of the stream up to the first one with a stream mux config were read.
        // Estimate the duration from the average frame size.
        if let Some(n_frames) = approximate_frame_count(&mut mss, first_frame_pos)? {
            let n_frames = n_frames * (config.num_sub_frames as u64 + 1);
            let ratio = u64::from(config.asc.output_sample_rate() / config.asc.sample_rate.max(1));

            info!("estimating duration from bitrate, may be inaccurate for vbr streams");

            track.with_duration(Duration::new(n_frames * SAMPLES_PER_AAC_PACKET.get()));
            track.with_num_frames(n_frames * SAMPLES_PER_AAC_PACKET.get() * ratio.max(1));
        }

        Ok(LoasReader {
            reader: mss,
            media_info: MediaInfo::from_track(&track),
            tracks: vec![track],
            chapters: opts.external_data.chapters,
            metadata: opts.external_data.metadata.unwrap_or_default(),
            first_frame_pos,
            next_packet_ts,
            config: Some(config),
            pending,
        })
    }

    /// Returns true if the stream may use SBR.
    fn may_use_sbr(&self) -> bool {
        /// The highest core sample rate of an SBR stream.
        const MAX_SBR_CORE_RATE: u32 = 32_000;

        self.config
            .as_ref()
            .is_some_and(|c| c.asc.sbr_present || c.asc.sample_rate <= MAX_SBR_CORE_RATE)
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
                    SAMPLES_PER_AAC_PACKET,
                    payload,
                ));

                self.next_packet_ts = match self.next_packet_ts.checked_add(SAMPLES_PER_AAC_PACKET)
                {
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

    fn seek(&mut self, mode: SeekMode, to: SeekTo) -> Result<SeekedTo> {
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

        // If the desired timestamp is less-than the next packet timestamp, attempt to seek to the
        // start of the stream.
        if required_ts < self.next_packet_ts {
            if self.reader.is_seekable() {
                self.reader.seek(SeekFrom::Start(self.first_frame_pos))?;
            }
            else {
                return seek_error(SeekErrorKind::ForwardOnly);
            }

            self.next_packet_ts = Timestamp::new(0);
        }

        // For an accurate seek, the decoder must be fed some frames before the target for it to
        // converge. Remember the position of the most recent frames.
        let sbr = self.may_use_sbr();
        let preroll = match (mode == SeekMode::Accurate, sbr) {
            (false, _) => 0,
            (true, false) => 1,
            (true, true) => AAC_SEEK_MAX_PREROLL_FRAMES as usize,
        };
        let mut recent: VecDeque<(u64, Timestamp)> = VecDeque::with_capacity(preroll + 1);

        // Parse frames from the stream until the frame containing the desired timestamp is
        // reached.
        loop {
            let len = match read_frame_len(&mut self.reader) {
                Ok(len) => len,
                Err(Error::IoError(err)) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return seek_error(SeekErrorKind::OutOfRange);
                }
                Err(err) => return Err(err),
            };

            let next_packet_ts = match self.next_packet_ts.checked_add(SAMPLES_PER_AAC_PACKET) {
                Some(ts) if ts <= required_ts => ts,
                // The frame contains the desired timestamp: rewind to its start.
                _ => {
                    self.reader.seek_buffered_rev(LOAS_HEADER_LEN as usize);
                    break;
                }
            };

            if preroll > 0 {
                if recent.len() == preroll {
                    recent.pop_front();
                }
                recent.push_back((
                    self.reader.pos().saturating_sub(LOAS_HEADER_LEN),
                    self.next_packet_ts,
                ));
            }

            self.reader.ignore_bytes(len as u64)?;
            self.next_packet_ts = next_packet_ts;
        }

        // Rewind to the frame to start decoding from, if possible and not already there.
        if preroll > 0 && self.reader.is_seekable() {
            let target = u64::try_from(self.next_packet_ts.get()).unwrap_or(0)
                / SAMPLES_PER_AAC_PACKET.get();
            let start = aac_seek_start_frame(target, sbr);

            let start_frame = recent.iter().find(|(_, ts)| {
                u64::try_from(ts.get()).ok() == start.checked_mul(SAMPLES_PER_AAC_PACKET.get())
            });

            if let Some(&(pos, ts)) = start_frame {
                if self.reader.seek(SeekFrom::Start(pos)).is_ok() {
                    self.next_packet_ts = ts;
                }
            }
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
