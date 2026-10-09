// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Audio Data Interchange Format (ADIF).
//!
//! ADIF (ISO/IEC 13818-7 §6.2, ISO/IEC 14496-3 §1.A.2) is an `adif_header()` followed by
//! back-to-back `raw_data_block()`s. Unlike ADTS, the blocks have no frame header and no length:
//! the end of a block is only known by parsing it, up to its `ID_END` element. The reader finds
//! the boundaries of blocks by parsing their syntax with the AAC decoder's parser, which is
//! therefore as much work as decoding the spectral data (but not the synthesis) of a stream.
//!
//! The timeline is not stored either, so the duration of a stream is estimated from the average
//! size of its first blocks. Seeking is by block: the reader remembers the positions of the
//! blocks it has parsed (every [`INDEX_STRIDE`] blocks), and a seek to a block that was not
//! parsed yet parses forward to it. Seeking a long stream for the first time is thus linear in
//! its length.

use std::io::{Seek, SeekFrom};

use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::codecs::audio::well_known::CODEC_ID_AAC;
use symphonia_core::errors::{
    Error, Result, SeekErrorKind, decode_error, seek_error, unsupported_error,
};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::formats::well_known::FORMAT_ID_ADIF;
use symphonia_core::io::*;
use symphonia_core::meta::{Metadata, MetadataLog};
use symphonia_core::support_format;

use symphonia_common::mpeg::audio::{
    AudioSpecificConfig, Mpeg4AudioSampleRate, ProgramConfig, aac_seek_start_frame,
    get_audio_codec_profile, get_mpeg4_audio_sample_rate_by_index,
};

use log::{debug, info};

use crate::aac::AacDecoder;
use crate::implicit_sbr::{
    MAX_IMPLICIT_SBR_CORE_RATE, MAX_PROBE_BLOCKS, build_asc, detect, may_have_implicit_sbr,
};

/// The number of samples per AAC frame (at the core sample rate).
const SAMPLES_PER_AAC_PACKET: u64 = 1024;

/// The `adif_id`.
const ADIF_ID: [u8; 4] = *b"ADIF";

/// The maximum size of an `adif_header()`: with 16 program config elements, 20 bits of buffer
/// fullness, and the 255 byte comment of each.
const MAX_HEADER_LEN: usize = 8 * 1024;

/// The number of bytes of the stream that are looked at to find the end of a block. A
/// `raw_data_block()` has at most 6144 bits per channel, and there are at most 8 channels
/// (without SBR payloads, which are small).
const BLOCK_WINDOW_LEN: usize = 16 * 1024;

/// The number of blocks examined when opening a stream, to estimate its duration.
const ESTIMATE_BLOCKS: usize = 64;

/// The maximum number of bytes buffered, for streams that cannot be rewound, when opening a
/// stream.
const MAX_OPEN_LEN: usize = 256 * 1024;

/// The position of one in this many blocks is remembered, for seeking.
const INDEX_STRIDE: u64 = 32;

const ADIF_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FORMAT_ID_ADIF,
    short_name: "adif",
    long_name: "Audio Data Interchange Format (AAC)",
};

/// An `adif_header()`.
#[derive(Debug)]
struct AdifHeader {
    /// The length of the header in bytes.
    len: usize,
    /// True if the bit rate is variable.
    variable_bitrate: bool,
    /// The bit rate in bits per second: the constant bit rate, or the maximum for a variable one.
    bitrate: u32,
    /// The program config elements, one for each program of the stream.
    programs: Vec<ProgramConfig>,
}

impl AdifHeader {
    /// Parse the header at the start of `data`, which must be the start of the stream: the byte
    /// alignment of the program config elements is relative to it.
    fn read(data: &[u8]) -> Result<AdifHeader> {
        let mut bs = BitReaderLtr::new(data);

        if bs.read_bits_leq32(32)? != u32::from_be_bytes(ADIF_ID) {
            return decode_error("adif: missing adif_id");
        }

        // copyright_id_present, copyright_id
        if bs.read_bool()? {
            bs.ignore_bits(72)?;
        }

        // original_copy, home
        bs.ignore_bits(2)?;

        let variable_bitrate = bs.read_bool()?;
        let bitrate = bs.read_bits_leq32(23)?;
        let num_program_config_elements = bs.read_bits_leq32(4)? + 1;

        let mut programs = Vec::with_capacity(num_program_config_elements as usize);

        for _ in 0..num_program_config_elements {
            if !variable_bitrate {
                // adif_buffer_fullness
                bs.ignore_bits(20)?;
            }

            programs.push(ProgramConfig::read(&mut bs)?);
        }

        // The header ends with the comment field of the last program config element, so that it
        // is byte aligned.
        let len = ((data.len() as u64) * 8 - bs.bits_left()).div_ceil(8) as usize;

        Ok(AdifHeader { len, variable_bitrate, bitrate, programs })
    }
}

/// Read as many bytes as are available, up to the length of the buffer. Returns the number of
/// bytes read.
fn read_available<B: ReadBytes>(reader: &mut B, buf: &mut [u8]) -> Result<usize> {
    let mut n = 0;

    while n < buf.len() {
        match reader.read_buf(&mut buf[n..]) {
            Ok(0) => break,
            Ok(len) => n += len,
            Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(err) => return Err(err.into()),
        }
    }

    Ok(n)
}

/// Audio Data Interchange Format (ADIF) format reader.
///
/// `AdifReader` implements a demuxer for AAC in ADIF: one program of AAC-LC. If the stream uses
/// SBR or parametric stereo (HE-AAC), which ADIF cannot signal, the reader finds it in the first
/// blocks of the stream and the codec parameters and timeline describe the decoded output, as for
/// [`AdtsReader`](crate::AdtsReader).
pub struct AdifReader<'s> {
    reader: MediaSourceStream<'s>,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    chapters: Option<ChapterGroup>,
    metadata: MetadataLog,
    /// The parser of the syntax of blocks.
    parser: AacDecoder,
    /// The position of the first block.
    first_block_pos: u64,
    /// The index of the next block.
    next_block: u64,
    /// The positions of blocks `k * INDEX_STRIDE`, for as far as the stream was parsed.
    index: Vec<u64>,
    /// The sample rate of the core codec.
    core_rate: u32,
    /// True if the stream was found to use SBR.
    sbr: bool,
    /// The duration of a packet in decoded frames.
    packet_dur: Duration,
}

impl<'s> AdifReader<'s> {
    pub fn try_new(mut mss: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
        // Read the header.
        let start_pos = mss.pos();

        let mut window = vec![0u8; MAX_HEADER_LEN];
        let n = read_available(&mut mss, &mut window)?;
        window.truncate(n);

        let header = AdifHeader::read(&window)?;

        mss.seek_buffered_rev(n - header.len);

        let first_block_pos = start_pos + header.len as u64;

        // Only a single program is supported.
        if header.programs.len() != 1 {
            return unsupported_error("adif: only a single program is supported");
        }

        let pce = &header.programs[0];

        let sample_rate = match get_mpeg4_audio_sample_rate_by_index(pce.sampling_frequency_index) {
            Mpeg4AudioSampleRate::SampleRate(rate) => rate,
            _ => return decode_error("adif: invalid sample rate"),
        };

        // Build the audio specific config for the decoder from the program config.
        let object_type = pce.object_type + 1;
        let asc_bytes = build_asc(object_type, sample_rate, 0, Some(pce), None);
        let asc = AudioSpecificConfig::read(&asc_bytes)?;

        let Some(channels) = asc.channels.clone()
        else {
            return decode_error("adif: missing channel configuration");
        };

        let mut codec_params = AudioCodecParameters::new();

        codec_params
            .for_codec(CODEC_ID_AAC)
            .with_sample_rate(sample_rate)
            .with_channels(channels)
            .with_extra_data(asc_bytes.clone());

        if let Some(profile) = get_audio_codec_profile(&asc) {
            codec_params.with_profile(profile);
        }

        // The parser that finds the end of blocks.
        let mut parser = AacDecoder::try_new_syntax_parser(&codec_params)?;

        // Parse the first blocks to estimate the duration and look for SBR.
        let ensure_rewind = !mss.is_seekable();

        if ensure_rewind {
            mss.ensure_seekback_buffer(MAX_OPEN_LEN);
        }

        let mut blocks: Vec<Box<[u8]>> = Vec::with_capacity(MAX_PROBE_BLOCKS);
        let mut n_blocks = 0usize;
        let mut n_bytes = 0usize;

        while n_blocks < ESTIMATE_BLOCKS && n_bytes < MAX_OPEN_LEN {
            let Some(block) = read_block(&mut mss, &mut parser)?
            else {
                break;
            };

            n_bytes += block.len();
            n_blocks += 1;

            if blocks.len() < MAX_PROBE_BLOCKS {
                blocks.push(block);
            }
        }

        if n_blocks == 0 {
            return decode_error("adif: no raw data block found");
        }

        // Return to the first block.
        if mss.is_seekable() {
            mss.seek(SeekFrom::Start(first_block_pos))?;
        }
        else {
            mss.seek_buffered(first_block_pos);
        }

        // Look for SBR and parametric stereo.
        let mut ratio = 1;
        let mut sbr = false;

        if may_have_implicit_sbr(&asc) {
            let ext = detect(&asc_bytes, sample_rate, blocks.iter().map(|b| &b[..]));

            if ext.sbr {
                let output_rate = sample_rate.saturating_mul(2);
                let extra_data =
                    build_asc(object_type, sample_rate, 0, Some(pce), Some((output_rate, ext.ps)));

                if let Ok(asc) = AudioSpecificConfig::read(&extra_data) {
                    info!(
                        "adif: stream has {}, output is {} Hz",
                        if ext.ps { "sbr and parametric stereo" } else { "sbr" },
                        asc.output_sample_rate()
                    );

                    codec_params.with_sample_rate(asc.output_sample_rate());

                    if let Some(channels) = asc.output_channels() {
                        codec_params.with_channels(channels);
                    }

                    if let Some(profile) = get_audio_codec_profile(&asc) {
                        codec_params.with_profile(profile);
                    }

                    codec_params.with_extra_data(extra_data);

                    ratio = u64::from(asc.output_sample_rate() / sample_rate).max(1);
                    sbr = true;
                }
            }
        }

        let packet_dur = Duration::new(SAMPLES_PER_AAC_PACKET * ratio);

        let mut track = Track::new(0);
        track.with_codec_params(CodecParameters::Audio(codec_params));

        // Estimate the duration from the average size of the first blocks. The length of the
        // stream is not known if it is not seekable.
        if let Some(byte_len) = mss.byte_len() {
            let remaining = byte_len.saturating_sub(first_block_pos);

            if let Some(num_blocks) = (remaining * n_blocks as u64).checked_div(n_bytes as u64) {
                info!("estimating duration from the first blocks, may be inaccurate");

                track.with_num_frames(num_blocks * packet_dur.get());
                track.with_duration(Duration::new(num_blocks * packet_dur.get()));
            }
        }

        debug!(
            "adif: {} program, bitrate={} ({}), header_len={}",
            header.programs.len(),
            header.bitrate,
            if header.variable_bitrate { "variable" } else { "constant" },
            header.len
        );

        Ok(AdifReader {
            reader: mss,
            media_info: MediaInfo::from_track(&track),
            tracks: vec![track],
            chapters: opts.external_data.chapters,
            metadata: opts.external_data.metadata.unwrap_or_default(),
            parser,
            first_block_pos,
            next_block: 0,
            index: Vec::new(),
            core_rate: sample_rate,
            sbr,
            packet_dur,
        })
    }

    /// Returns true if the stream may use SBR, which ADIF can only signal implicitly.
    fn may_use_sbr(&self) -> bool {
        self.sbr || self.core_rate <= MAX_IMPLICIT_SBR_CORE_RATE
    }

    /// Read the next block of the stream and advance the position in the timeline. Returns `None`
    /// at the end of the stream.
    fn next_block(&mut self) -> Result<Option<Box<[u8]>>> {
        let pos = self.reader.pos();

        let Some(block) = read_block(&mut self.reader, &mut self.parser)?
        else {
            return Ok(None);
        };

        if self.next_block % INDEX_STRIDE == 0
            && self.index.len() as u64 == self.next_block / INDEX_STRIDE
        {
            self.index.push(pos);
        }

        self.next_block += 1;

        Ok(Some(block))
    }
}

/// Read the `raw_data_block()` at the current position of the stream. Returns `None` at the end
/// of the stream, which is also where the data can no longer be parsed: ADIF has nothing to
/// resynchronise to, and a stream may end with other data (such as a tag), or be cut short.
fn read_block(
    reader: &mut MediaSourceStream<'_>,
    parser: &mut AacDecoder,
) -> Result<Option<Box<[u8]>>> {
    let mut window = vec![0u8; BLOCK_WINDOW_LEN];
    let n = read_available(reader, &mut window)?;

    if n == 0 {
        return Ok(None);
    }

    window.truncate(n);

    let len = match parser.measure_raw_data_block(&window) {
        Ok(len) => len,
        Err(err) => {
            debug!("adif: no raw data block at the byte {}: {err}", reader.pos() - n as u64);
            return Ok(None);
        }
    };

    reader.seek_buffered_rev(n - len);

    window.truncate(len);

    Ok(Some(window.into_boxed_slice()))
}

impl Scoreable for AdifReader<'_> {
    fn score(mut src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        let mut id = [0u8; 4];
        src.read_buf_exact(&mut id)?;

        if id == ADIF_ID { Ok(Score::Supported(255)) } else { Ok(Score::Unsupported) }
    }
}

impl ProbeableFormat<'_> for AdifReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + '_>> {
        Ok(Box::new(AdifReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(ADIF_FORMAT_INFO, &["aac", "adif"], &["audio/aac"], &[b"ADIF"])]
    }
}

impl FormatReader for AdifReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &ADIF_FORMAT_INFO
    }

    fn media_info(&self) -> &MediaInfo {
        &self.media_info
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        let ts = Timestamp::new((self.next_block * self.packet_dur.get()) as i64);

        match self.next_block()? {
            Some(block) => Ok(Some(Packet::new(0, ts, self.packet_dur, block))),
            None => Ok(None),
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

        // The block to start decoding from: a decoder that reproduces a continuous decode at the
        // required timestamp must be fed some blocks before it (MDCT overlap, SBR state). This
        // applies to coarse seeks too.
        let packet_dur = self.packet_dur.get();
        let required_block = u64::try_from(required_ts.get()).unwrap_or(0) / packet_dur;
        let mut start_block = aac_seek_start_frame(required_block, self.may_use_sbr());

        if self.reader.is_seekable() {
            // Jump to the nearest block with a known position before the block to start from, if
            // that is closer than the current position (or the current position is after it).
            let entry =
                ((start_block / INDEX_STRIDE) as usize).min(self.index.len().saturating_sub(1));

            if let Some(&pos) = self.index.get(entry) {
                let block = entry as u64 * INDEX_STRIDE;

                if start_block < self.next_block || block > self.next_block {
                    self.reader.seek(SeekFrom::Start(pos))?;
                    self.next_block = block;
                }
            }
            else if start_block < self.next_block {
                self.reader.seek(SeekFrom::Start(self.first_block_pos))?;
                self.next_block = 0;
            }
        }
        else if required_block < self.next_block {
            return seek_error(SeekErrorKind::ForwardOnly);
        }
        else if start_block < self.next_block {
            // The stream cannot be rewound to the block to start from, but the required block can
            // still be reached.
            start_block = self.next_block;
        }

        // Parse blocks until the block to start from is reached.
        while self.next_block < start_block {
            if self.next_block()?.is_none() {
                return seek_error(SeekErrorKind::OutOfRange);
            }
        }

        let actual_ts = Timestamp::new((self.next_block * packet_dur) as i64);

        debug!("seeked to ts={} (delta={})", actual_ts, required_ts.saturating_delta(actual_ts));

        Ok(SeekedTo { track_id: 0, required_ts, actual_ts })
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

        fn align(&mut self) {
            self.n_bits = self.n_bits.next_multiple_of(8);
        }
    }

    /// Writes the header of a stream with a single stereo AAC-LC program at 44.1 kHz.
    fn stereo_header(copyright: bool, variable_bitrate: bool, comment: &[u8]) -> Vec<u8> {
        let mut bw = BitWriter::default();

        for &b in b"ADIF" {
            bw.put(u32::from(b), 8);
        }

        bw.put(copyright as u32, 1);

        if copyright {
            for _ in 0..9 {
                bw.put(0x41, 8);
            }
        }

        bw.put(0, 2); // original_copy, home
        bw.put(variable_bitrate as u32, 1);
        bw.put(128_000, 23);
        bw.put(0, 4); // num_program_config_elements - 1

        if !variable_bitrate {
            bw.put(0x7ffff, 20); // adif_buffer_fullness
        }

        // program_config_element(): one front channel pair element.
        bw.put(0, 4); // element_instance_tag
        bw.put(1, 2); // profile: AAC LC
        bw.put(4, 4); // sampling_frequency_index: 44100
        bw.put(1, 4); // num_front_channel_elements
        bw.put(0, 4);
        bw.put(0, 4);
        bw.put(0, 2);
        bw.put(0, 3);
        bw.put(0, 4);
        bw.put(0, 3); // No mixdowns.
        bw.put(1, 1); // front_element_is_cpe
        bw.put(0, 4); // front_element_tag_select
        bw.align();
        bw.put(comment.len() as u32, 8);

        for &b in comment {
            bw.put(u32::from(b), 8);
        }

        bw.bytes
    }

    #[test]
    fn reads_the_header() {
        for (copyright, vbr, comment) in [
            (false, false, &b""[..]),
            (true, false, &b"hello"[..]),
            (false, true, &b"x"[..]),
            (true, true, &b""[..]),
        ] {
            let header = stereo_header(copyright, vbr, comment);
            let mut data = header.clone();
            data.extend_from_slice(&[0xff; 16]);

            let parsed = AdifHeader::read(&data).unwrap();

            assert_eq!(parsed.len, header.len(), "copyright={copyright} vbr={vbr}");
            assert_eq!(parsed.variable_bitrate, vbr);
            assert_eq!(parsed.bitrate, 128_000);
            assert_eq!(parsed.programs.len(), 1);
            assert_eq!(parsed.programs[0].object_type, 1);
            assert_eq!(parsed.programs[0].sampling_frequency_index, 4);
            assert_eq!(parsed.programs[0].front_is_cpe, vec![true]);
        }
    }

    #[test]
    fn rejects_other_data() {
        assert!(AdifHeader::read(b"ADTS\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0").is_err());
        assert!(AdifHeader::read(b"ADIF").is_err());
    }

    /// An AAC-LC `raw_data_block()` of a stereo CPE with every band zero (`max_sfb` 0).
    fn silent_block() -> Vec<u8> {
        let mut bw = BitWriter::default();

        bw.put(1, 3); // ID_CPE
        bw.put(0, 4); // element_instance_tag
        bw.put(1, 1); // common_window
        // ics_info(): reserved, ONLY_LONG_SEQUENCE, window shape, max_sfb = 0, no predictor.
        bw.put(0, 1);
        bw.put(0, 2);
        bw.put(0, 1);
        bw.put(0, 6);
        bw.put(0, 1);
        bw.put(0, 2); // ms_mask_present

        for _ in 0..2 {
            bw.put(100, 8); // global_gain
            // No sections and scale factors. pulse_data_present, tns_data_present, and
            // gain_control_data_present.
            bw.put(0, 3);
        }

        bw.put(7, 3); // ID_END
        bw.align();

        bw.bytes
    }

    #[test]
    fn splits_blocks_by_parsing_them() {
        let mut data = stereo_header(false, false, b"");
        let header_len = data.len();

        let blocks: Vec<Vec<u8>> = (0..5).map(|_| silent_block()).collect();

        for block in &blocks {
            data.extend_from_slice(block);
        }

        let mss = MediaSourceStream::new(Box::new(std::io::Cursor::new(data)), Default::default());
        let mut reader = AdifReader::try_new(mss, Default::default()).unwrap();

        let track = &reader.tracks()[0];
        assert!(matches!(
            &track.codec_params,
            Some(CodecParameters::Audio(params)) if params.sample_rate == Some(44_100)
        ));

        let mut n = 0;

        while let Some(packet) = reader.next_packet().unwrap() {
            assert_eq!(packet.pts.get(), n * 1024);
            assert_eq!(packet.dur.get(), 1024);
            assert_eq!(&packet.data[..], &blocks[n as usize][..]);
            n += 1;
        }

        assert_eq!(n, 5);
        assert!(header_len > 0);
    }
}
