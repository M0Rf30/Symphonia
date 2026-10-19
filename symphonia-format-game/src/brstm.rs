// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Demuxer of the Nintendo stream files: BRSTM (Wii, `RSTM`), BFSTM (Wii U and Switch, `FSTM`)
//! and BCSTM (3DS, `CSTM`), after FFmpeg's `libavformat/brstm.c`.
//!
//! A stream holds PCM 8-bit, PCM 16-bit or DSP-ADPCM audio in blocks, each of which holds the
//! data of every channel one after the other. The header is read from random positions, so the
//! source must be seekable.
//!
//! The DSP-ADPCM streams are normalised to the packets that the ADPCM decoder takes (see
//! `symphonia-codec-adpcm`'s `codec_nintendo_dsp`): the coefficients of the channels are the extra
//! data, and the first packet after the start or a seek holds the history of the channels, read
//! from the table of the stream (`ADPC` or `SEEK` chunk). Unlike FFmpeg, the history of a seek
//! is the one of its block.

use std::io::{Seek, SeekFrom};

use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::well_known::{
    CODEC_ID_ADPCM_NINTENDO_DSP, CODEC_ID_PCM_S8_PLANAR, CODEC_ID_PCM_S16BE_PLANAR,
    CODEC_ID_PCM_S16LE_PLANAR,
};
use symphonia_core::codecs::audio::{AudioCodecId, AudioCodecParameters};
use symphonia_core::common::FourCc;
use symphonia_core::errors::{
    Result, SeekErrorKind, decode_error, limit_error, seek_error, unsupported_error,
};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::io::*;
use symphonia_core::meta::{Metadata, MetadataLog};
use symphonia_core::support_format;
use symphonia_core::units::{Duration, Timestamp};

use crate::common::{channels_for, frame_time_base, read_up_to, required_timestamp, unseekable};

const BRSTM_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"BRST")),
    short_name: "brstm",
    long_name: "BRSTM (Binary Revolution Stream)",
};

const BFSTM_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"BFST")),
    short_name: "bfstm",
    long_name: "BFSTM, BCSTM (Binary Cafe and Citrus Streams)",
};

/// The largest number of channels.
const MAX_CHANNELS: usize = 64;

/// The largest block of one channel in bytes.
const MAX_BLOCK_SIZE: u64 = 1 << 20;

/// The number of samples in a full DSP-ADPCM frame, and its size in bytes.
const DSP_FRAME_SAMPLES: u64 = 14;
const DSP_FRAME_BYTES: u64 = 8;

/// The number of blocks decoded and discarded before the requested position of an accurate seek.
const SEEK_PREROLL_BLOCKS: u64 = 1;

/// The size in bytes of the data of `samples` samples of one DSP-ADPCM channel.
fn dsp_len(samples: u64) -> u64 {
    let partial = samples % DSP_FRAME_SAMPLES;
    DSP_FRAME_BYTES * (samples / DSP_FRAME_SAMPLES)
        + if partial == 0 { 0 } else { 1 + partial.div_ceil(2) }
}

/// Reads the fields of the file at a position, in the byte order of the file.
struct Fields<'a, 's> {
    src: &'a mut MediaSourceStream<'s>,
    base: u64,
    little_endian: bool,
}

impl Fields<'_, '_> {
    fn seek(&mut self, pos: u64) -> Result<()> {
        self.src.seek(SeekFrom::Start(self.base + pos))?;
        Ok(())
    }

    fn pos(&self) -> u64 {
        self.src.pos() - self.base
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.src.read_byte()?)
    }

    fn u16(&mut self) -> Result<u16> {
        let b = self.src.read_double_bytes()?;
        Ok(if self.little_endian { u16::from_le_bytes(b) } else { u16::from_be_bytes(b) })
    }

    fn u32(&mut self) -> Result<u32> {
        let b = self.src.read_quad_bytes()?;
        Ok(if self.little_endian { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) })
    }

    fn tag(&mut self) -> Result<[u8; 4]> {
        Ok(self.src.read_quad_bytes()?)
    }
}

/// The codec of the stream.
#[derive(Clone, Copy, PartialEq)]
enum Codec {
    Pcm8,
    Pcm16,
    Dsp,
}

/// Nintendo BRSTM, BFSTM and BCSTM reader.
pub struct BrstmReader<'s> {
    reader: MediaSourceStream<'s>,
    info: &'static FormatInfo,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    metadata: MetadataLog,
    codec: Codec,
    channels: u64,
    data_start: u64,
    block_size: u64,
    block_count: u64,
    samples_per_block: u64,
    last_block_used_bytes: u64,
    last_block_samples: u64,
    last_block_size: u64,
    /// The history `(yn1, yn2)` of every channel at the start of every block (DSP-ADPCM).
    history: Vec<u8>,
    /// Whether the history is stored little-endian.
    history_le: bool,
    /// The index of the next block to read.
    next_block: u64,
    /// Whether the next packet has to carry the history.
    needs_history: bool,
}

impl<'s> BrstmReader<'s> {
    pub fn try_new(mut source: MediaSourceStream<'s>, _options: FormatOptions) -> Result<Self> {
        if !source.is_seekable() {
            return unsupported_error("brstm: the source must be seekable");
        }

        let base = source.pos();
        let total_len = source.byte_len();

        let mut head = [0u8; 6];
        source.read_buf_exact(&mut head)?;

        let bfstm = match &head[..4] {
            b"RSTM" => false,
            b"FSTM" | b"CSTM" => true,
            _ => return unsupported_error("brstm: not a stream file"),
        };

        // The byte order mark, as it reads in big-endian order.
        let little_endian = match u16::from_be_bytes([head[4], head[5]]) {
            0xfeff => false,
            0xfffe => true,
            _ => return decode_error("brstm: invalid byte order"),
        };

        let mut f = Fields { src: &mut source, base, little_endian };

        let (mut start, mut major, mut minor) = (0u64, 1u8, 0u8);
        let pos;

        if !bfstm {
            major = f.u8()?;
            minor = f.u8()?;
            f.seek(12)?;
            let size = u64::from(f.u16()?);
            if size < 14 {
                return decode_error("brstm: invalid header size");
            }
            pos = size;
            f.seek(pos)?;
            if &f.tag()? != b"HEAD" {
                return decode_error("brstm: missing HEAD chunk");
            }
        }
        else {
            f.seek(6)?;
            let header_size = u64::from(f.u16()?);
            f.seek(16)?;
            let section_count = f.u16()?;
            f.seek(20)?;

            let mut info_offset = 0u64;

            let mut i = 0;
            while f.pos() < header_size && !(start != 0 && info_offset != 0) && i < section_count {
                let flag = f.u16()?;
                f.seek(f.pos() + 2)?;
                match flag {
                    0x4000 => {
                        info_offset = u64::from(f.u32()?);
                        f.seek(f.pos() + 4)?;
                    }
                    0x4002 => {
                        start = u64::from(f.u32()?) + 8;
                        f.seek(f.pos() + 4)?;
                    }
                    0x4001 | 0x4003 => f.seek(f.pos() + 8)?,
                    _ => (),
                }
                i += 1;
            }

            if info_offset == 0 || start == 0 {
                return decode_error("brstm: missing INFO or DATA section");
            }

            pos = info_offset;
            f.seek(pos)?;
            if &f.tag()? != b"INFO" {
                return decode_error("brstm: missing INFO chunk");
            }
        }

        let size = u64::from(f.u32()?);
        if size < 40 {
            return decode_error("brstm: invalid header size");
        }
        f.seek(pos + 12)?;
        let h1offset = u64::from(f.u32()?);
        if h1offset > size {
            return decode_error("brstm: invalid header offset");
        }
        f.seek(pos + 28)?;
        let mut toffset = u64::from(f.u32()?) + 16;
        if toffset > size {
            return decode_error("brstm: invalid table offset");
        }

        f.seek(pos + h1offset + 8)?;
        let codec_id = f.u8()?;
        let codec = match codec_id {
            0 => Codec::Pcm8,
            1 => Codec::Pcm16,
            2 => Codec::Dsp,
            _ => return unsupported_error("brstm: unsupported codec"),
        };

        let _loop = f.u8()?;
        let channels = u64::from(f.u8()?);
        if channels == 0 || channels as usize > MAX_CHANNELS {
            return decode_error("brstm: invalid number of channels");
        }
        f.u8()?;

        let rate = if bfstm {
            f.u32()?
        }
        else {
            u32::from(f.u16()?) // Followed by padding.
        };
        if rate == 0 || rate > i32::MAX as u32 {
            return decode_error("brstm: invalid sample rate");
        }
        if !bfstm {
            f.u16()?;
        }
        // The loop start.
        f.u32()?;

        let duration = u64::from(f.u32()?);
        if !bfstm {
            start = u64::from(f.u32()?);
        }

        let block_count = u64::from(f.u32()?);
        if block_count > u64::from(u16::MAX) {
            return decode_error("brstm: too many blocks");
        }

        let block_size = u64::from(f.u32()?);
        let samples_per_block = u64::from(f.u32()?);
        let last_block_used_bytes = u64::from(f.u32()?);
        let last_block_samples = u64::from(f.u32()?);
        let last_block_size = u64::from(f.u32()?);

        // The sizes are untrusted and size the buffers of the packets.
        if block_size > MAX_BLOCK_SIZE || last_block_size > MAX_BLOCK_SIZE {
            return limit_error("brstm: block size is too large");
        }

        if last_block_used_bytes > last_block_size {
            return decode_error("brstm: invalid last block");
        }

        if block_count == 0 || block_size == 0 || samples_per_block == 0 {
            return decode_error("brstm: invalid block layout");
        }

        // The coefficients, normalised to little-endian.
        let mut coeffs = Vec::new();

        if codec == Codec::Dsp {
            f.seek(pos + toffset)?;

            if !bfstm {
                toffset = u64::from(f.u32()?) + 16;
            }
            else {
                toffset = toffset + u64::from(f.u32()?) + channels * 8 - 8;
            }
            if toffset > size {
                return decode_error("brstm: invalid table offset");
            }

            let mut offsets = Vec::with_capacity(channels as usize);

            if !bfstm {
                f.seek((pos + toffset).saturating_sub(8 * (channels + 1)))?;
                for _ in 0..channels {
                    f.u32()?;
                    offsets.push(u64::from(f.u32()?));
                }
                offsets.sort_unstable();
            }

            f.seek(pos + toffset)?;

            for ch in 0..channels as usize {
                // The tables are in the order of their offsets (the `offsets` of a BFSTM are
                // empty: its tables follow one another).
                if let Some(&offset) = offsets.get(ch) {
                    f.seek(pos + 16 + offset)?;
                }

                for _ in 0..16 {
                    let c = f.u16()?;
                    coeffs.extend_from_slice(&c.to_le_bytes());
                }

                if bfstm {
                    f.seek(f.pos() + 14)?;
                }
            }

            if size < f.pos() - pos {
                return decode_error("brstm: invalid table");
            }
        }

        // The chunks that follow the header.
        f.seek(pos + size)?;

        let mut history = Vec::new();
        let mut history_le = little_endian;
        let data_start;

        loop {
            let chunk_pos = f.pos();
            if total_len.is_some_and(|len| base + chunk_pos + 8 > len) {
                return decode_error("brstm: missing DATA chunk");
            }

            let chunk = f.tag()?;
            let chunk_size = u64::from(f.u32()?);
            if chunk_size < 8 {
                return decode_error("brstm: invalid chunk size");
            }
            let body = chunk_size - 8;

            match &chunk {
                b"SEEK" | b"ADPC" if codec == Codec::Dsp => {
                    let asize = block_count * channels * 4;
                    if body < asize {
                        return decode_error("brstm: invalid history table");
                    }
                    if history.is_empty() {
                        // The big-endian BFSTM files have a little-endian table.
                        history_le = little_endian || bfstm;
                        history = vec![0u8; asize as usize];
                        f.src.read_buf_exact(&mut history)?;
                    }
                }
                b"DATA" => {
                    if start < f.pos() || (codec == Codec::Dsp && history.is_empty()) {
                        return decode_error("brstm: invalid DATA chunk");
                    }
                    data_start = start + if bfstm && codec == Codec::Dsp { 24 } else { 0 };
                    break;
                }
                _ => (),
            }

            f.seek(chunk_pos + chunk_size)?;
        }

        if !bfstm && (major != 1 || minor != 0) {
            log::warn!("brstm: version {major}.{minor}");
        }

        let (codec_id, extra): (AudioCodecId, Option<Vec<u8>>) = match codec {
            Codec::Pcm8 => (CODEC_ID_PCM_S8_PLANAR, None),
            Codec::Pcm16 if little_endian => (CODEC_ID_PCM_S16LE_PLANAR, None),
            Codec::Pcm16 => (CODEC_ID_PCM_S16BE_PLANAR, None),
            Codec::Dsp => (CODEC_ID_ADPCM_NINTENDO_DSP, Some(coeffs)),
        };

        let mut params = AudioCodecParameters::new();
        params
            .for_codec(codec_id)
            .with_sample_rate(rate)
            .with_channels(channels_for(channels as usize))
            .with_max_frames_per_packet(samples_per_block);
        match codec {
            Codec::Dsp => {
                params.with_bits_per_coded_sample(4);
            }
            Codec::Pcm8 => {
                params.with_bits_per_coded_sample(8);
            }
            Codec::Pcm16 => {
                params.with_bits_per_coded_sample(16);
            }
        }
        if let Some(extra) = extra {
            params.with_extra_data(extra.into_boxed_slice());
        }

        let time_base = frame_time_base(rate);

        let mut track = Track::new(0);
        track.time_base = time_base;
        track.num_frames = Some(duration);
        track.duration = Some(Duration::new(duration));
        track.with_codec_params(CodecParameters::Audio(params));

        let mut media_info = MediaInfo::new();
        media_info.time_base = time_base;
        media_info.duration = Some(Duration::new(duration));

        Ok(BrstmReader {
            reader: source,
            info: if bfstm { &BFSTM_FORMAT_INFO } else { &BRSTM_FORMAT_INFO },
            media_info,
            tracks: vec![track],
            metadata: MetadataLog::default(),
            codec,
            channels,
            data_start: base + data_start,
            block_size,
            block_count,
            samples_per_block,
            last_block_used_bytes,
            last_block_samples,
            last_block_size,
            history,
            history_le,
            next_block: 0,
            needs_history: true,
        })
    }

    /// The history of the channels at the start of `block`, as the little-endian 16-bit integers
    /// of the packets.
    fn block_history(&self, block: u64) -> Vec<u8> {
        let n = 4 * self.channels as usize;
        let from = block as usize * n;
        let mut out = Vec::with_capacity(n);

        for pair in self.history[from..from + n].chunks_exact(2) {
            let v = if self.history_le { [pair[0], pair[1]] } else { [pair[1], pair[0]] };
            out.extend_from_slice(&v);
        }

        out
    }
}

/// The probe of FFmpeg (`probe`, `probe_bfstm`).
fn probe_score(head: &[u8; 6], bfstm: bool) -> bool {
    let tags: &[&[u8; 4]] = if bfstm { &[b"FSTM", b"CSTM"] } else { &[b"RSTM"] };
    tags.iter().any(|t| head[..4] == **t) && matches!(head[4..6], [0xff, 0xfe] | [0xfe, 0xff])
}

impl Scoreable for BrstmReader<'_> {
    fn score(mut src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        let mut head = [0u8; 6];

        if read_up_to(&mut src, &mut head)? < head.len() {
            return Ok(Score::Unsupported);
        }

        Ok(if probe_score(&head, false) || probe_score(&head, true) {
            Score::Supported(170)
        }
        else {
            Score::Unsupported
        })
    }
}

impl ProbeableFormat<'_> for BrstmReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + '_>> {
        Ok(Box::new(BrstmReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[
            support_format!(BRSTM_FORMAT_INFO, &["brstm"], &["audio/x-brstm"], &[b"RSTM"]),
            support_format!(
                BFSTM_FORMAT_INFO,
                &["bfstm", "bcstm"],
                &["audio/x-bfstm"],
                &[b"FSTM", b"CSTM"]
            ),
        ]
    }
}

impl FormatReader for BrstmReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        self.info
    }

    fn media_info(&self) -> &MediaInfo {
        &self.media_info
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        if self.next_block >= self.block_count {
            return Ok(None);
        }

        let block = self.next_block;
        let last = block + 1 == self.block_count;
        let channels = self.channels;

        let (size, samples) = if last {
            let size = self.last_block_used_bytes;
            let samples = self.last_block_samples;

            // Like FFmpeg, a last block of DSP-ADPCM that has more bytes than its samples need
            // is cut to them.
            if samples < size * 14 / 8 && self.codec == Codec::Dsp {
                (dsp_len(samples), samples)
            }
            else {
                (size, samples)
            }
        }
        else {
            (self.block_size, self.samples_per_block)
        };

        let channel_len = match self.codec {
            Codec::Dsp => dsp_len(samples),
            Codec::Pcm8 => samples,
            Codec::Pcm16 => 2 * samples,
        };

        // A block that does not hold the data of its samples is not audio.
        if channel_len > size {
            return decode_error("brstm: block too short for its samples");
        }

        let mut buf = Vec::new();

        if self.codec == Codec::Dsp {
            if self.needs_history {
                buf.push(1);
                buf.extend_from_slice(&self.block_history(block));
            }
            else {
                buf.push(0);
            }
        }

        let block_start = self.block_size * channels;
        let pos = self.data_start + block * block_start;
        self.reader.seek(SeekFrom::Start(pos))?;

        let mut channel = vec![0u8; channel_len as usize];

        for ch in 0..channels {
            // The data of the channels of a block are one after the other, in blocks of the
            // size of the stream, or of the last block.
            let offset = ch * if last { self.last_block_size } else { self.block_size };
            self.reader.seek(SeekFrom::Start(pos + offset))?;

            if read_up_to(&mut self.reader, &mut channel)? < channel.len() {
                self.next_block = self.block_count;
                return Ok(None);
            }
            buf.extend_from_slice(&channel);
        }

        self.needs_history = false;
        self.next_block += 1;

        Ok(Some(
            PacketBuilder::new()
                .track_id(0)
                .pts(Timestamp::new((block * self.samples_per_block) as i64))
                .dur(Duration::new(samples))
                .data(buf)
                .build(),
        ))
    }

    fn metadata(&mut self) -> Metadata<'_> {
        self.metadata.metadata()
    }

    fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    fn seek(&mut self, mode: SeekMode, to: SeekTo) -> Result<SeekedTo> {
        let Some(time_base) = self.media_info.time_base
        else {
            return unseekable();
        };

        let required_ts = required_timestamp(to, time_base)?;

        let duration = self.media_info.duration.map_or(u64::MAX, |d| d.get());
        if required_ts >= duration {
            return seek_error(SeekErrorKind::OutOfRange);
        }

        let mut block = (required_ts / self.samples_per_block).min(self.block_count - 1);

        if mode == SeekMode::Accurate && self.codec == Codec::Dsp {
            block = block.saturating_sub(SEEK_PREROLL_BLOCKS);
        }

        self.next_block = block;
        self.needs_history = true;

        Ok(SeekedTo {
            track_id: 0,
            required_ts: Timestamp::new(required_ts as i64),
            actual_ts: Timestamp::new((block * self.samples_per_block) as i64),
        })
    }

    fn into_inner<'a>(self: Box<Self>) -> MediaSourceStream<'a>
    where
        Self: 'a,
    {
        self.reader
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use symphonia_core::errors::Error;

    use super::*;

    /// A mono 16-bit BRSTM of one block of `block_size` bytes.
    fn stream(block_size: u32, last_block_size: u32) -> Vec<u8> {
        let mut d = vec![0u8; 0x1000];
        d[..4].copy_from_slice(b"RSTM");
        d[4..6].copy_from_slice(&[0xfe, 0xff]);
        d[6] = 1;
        d[12..14].copy_from_slice(&0x20u16.to_be_bytes());
        d[0x20..0x24].copy_from_slice(b"HEAD");
        d[0x24..0x28].copy_from_slice(&0x100u32.to_be_bytes());
        // The offset of the stream info, from the start of the chunk.
        d[0x2c..0x30].copy_from_slice(&0x20u32.to_be_bytes());
        let h = 0x20 + 0x20 + 8;
        d[h] = 1; // PCM16.
        d[h + 2] = 1; // Channels.
        d[h + 4..h + 6].copy_from_slice(&44100u16.to_be_bytes());
        d[h + 12..h + 16].copy_from_slice(&1000u32.to_be_bytes()); // Duration.
        d[h + 16..h + 20].copy_from_slice(&0x180u32.to_be_bytes()); // Data start.
        d[h + 20..h + 24].copy_from_slice(&1u32.to_be_bytes()); // Block count.
        d[h + 24..h + 28].copy_from_slice(&block_size.to_be_bytes());
        d[h + 28..h + 32].copy_from_slice(&1000u32.to_be_bytes()); // Samples per block.
        d[h + 32..h + 36].copy_from_slice(&2000u32.to_be_bytes()); // Last block bytes.
        d[h + 36..h + 40].copy_from_slice(&1000u32.to_be_bytes()); // Last block samples.
        d[h + 40..h + 44].copy_from_slice(&last_block_size.to_be_bytes());
        d[0x120..0x124].copy_from_slice(b"DATA");
        d[0x124..0x128].copy_from_slice(&0xe0u32.to_be_bytes());
        d
    }

    fn open(data: Vec<u8>) -> Result<BrstmReader<'static>> {
        let mss = MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default());
        BrstmReader::try_new(mss, FormatOptions::default())
    }

    #[test]
    fn accepts_a_sane_block_size() {
        let mut reader = open(stream(2048, 2048)).unwrap();
        assert!(reader.next_packet().unwrap().is_some());
    }

    #[test]
    fn rejects_a_huge_block_size() {
        assert!(matches!(open(stream(0x8000_0000, 2048)), Err(Error::LimitError(_))));
        assert!(matches!(open(stream(2048, 0xffff_ffff)), Err(Error::LimitError(_))));
    }
}
