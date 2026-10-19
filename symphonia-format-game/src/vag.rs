// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Demuxers of the Sony PlayStation ADPCM (VAG) family.
//!
//! The layouts follow FFmpeg's demuxers of the same names (`libavformat/vag.c`, `svag.c`, `vpk.c`,
//! `ads.c`, `xvag.c` and `msf.c`). All carry PlayStation ADPCM, with 16 byte frames of 28 samples
//! per channel; the data is made of blocks, which hold the same number of consecutive frames of
//! each channel one channel after the other:
//!
//! | Format | Magic | Header | Block of | Other codecs |
//! |---|---|---|---|---|
//! | Sony PS2 VAG | `VAGp` | 0x30 (mono) or 0x80 (stereo), big-endian | 1 frame per channel, or 0x1000 bytes per channel | |
//! | Konami SVAG | `Svag` | 0x800, little-endian | interleave of the header | |
//! | Sony PS2 VPK | `VPK ` | at the offset of the header | block size of the header | |
//! | Sony PS2 ADS/SS2 | `SShd` | 40 bytes | interleave of the header | planar PCM 16-bit |
//! | Sony PS3 XVAG | `XVAG` | at the offset of the header | 1 frame per channel | |
//! | Sony PS3 MSF | `MSF` | 0x40, big-endian | 1 frame per channel | PCM 16-bit |
//!
//! The MP3 payload of XVAG and MSF files, and the ATRAC3 and ATRAC3plus payloads of MSF files are
//! rejected as unsupported.
//!
//! Like FFmpeg, a stereo VAG file with the magic again at 0x1000 is read from its start, in blocks
//! of 0x1000 bytes per channel: the headers of the channels are inside the first block, and the
//! decoder rejects it.

use std::io::{Seek, SeekFrom};

use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::well_known::{
    CODEC_ID_ADPCM_PSX, CODEC_ID_PCM_S16BE, CODEC_ID_PCM_S16LE, CODEC_ID_PCM_S16LE_PLANAR,
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

const VAG_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"VAGp")),
    short_name: "vag",
    long_name: "Sony PS2 VAG",
};

const SVAG_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"Svag")),
    short_name: "svag",
    long_name: "Konami PS2 SVAG",
};

const VPK_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"VPK ")),
    short_name: "vpk",
    long_name: "Sony PS2 VPK",
};

const ADS_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"SShd")),
    short_name: "ads",
    long_name: "Sony PS2 ADS",
};

const XVAG_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"XVAG")),
    short_name: "xvag",
    long_name: "Sony PS3 XVAG",
};

const MSF_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FormatId::new(FourCc::new(*b"MSF\0")),
    short_name: "msf",
    long_name: "Sony PS3 MSF",
};

/// The size in bytes of a frame of one channel.
const FRAME_BYTES: u64 = 16;

/// The number of samples in a frame of one channel.
const FRAME_SAMPLES: u64 = 28;

/// The largest number of channels of the ADPCM (`ff_adpcm_decode_init`, `libavcodec/adpcm.c`).
const MAX_ADPCM_CHANNELS: u64 = 8;

/// The largest number of channels of the PCM (`FF_SANE_NB_CHANNELS`, `libavcodec/internal.h`).
const MAX_PCM_CHANNELS: u64 = 64;

/// The number of bytes of the header that are read at once. It covers the second magic of the
/// stereo VAG files at 0x1000.
const HEAD_LEN: usize = 0x1004;

/// The packets of the frame interleaved layouts hold blocks up to this size, in bytes.
const PACKET_BYTES: u64 = 4096;

/// The largest block in bytes, of all the channels. The files in the wild use blocks of a few
/// kilobytes per channel; the header fields are untrusted and size the buffers of the packets.
const MAX_BLOCK_BYTES: u64 = 1 << 22;

/// The number of frames decoded and discarded before the requested position of an accurate seek.
/// The history of the ADPCM is not stored in the stream; its response to a wrong history decays
/// with the number of samples.
const SEEK_PREROLL_FRAMES: u64 = 224;

/// The layout of the audio of a stream.
struct Layout {
    codec: AudioCodecId,
    channels: u64,
    sample_rate: u32,
    /// The size in bytes of a frame of one channel (a sample of the PCM).
    unit_bytes: u64,
    /// The number of samples in a frame of one channel.
    unit_frames: u64,
    /// The size in bytes of a block.
    block_bytes: u64,
    /// The number of blocks in a packet.
    packet_blocks: u64,
    /// The position of the first block, from the start of the file.
    data_start: u64,
    /// The number of frames that the header declares.
    declared_frames: Option<u64>,
    /// The last block of a VPK file, whose end is cut at the declared length.
    vpk: Option<VpkTail>,
}

#[derive(Clone, Copy)]
struct VpkTail {
    /// The number of blocks, including the last, which may be short.
    block_count: u64,
    /// The size in bytes of the short block, or 0 if the last block is whole.
    last_bytes: u64,
}

impl Layout {
    /// The number of frames of a packet of `len` bytes.
    fn frames_in(&self, len: u64) -> u64 {
        len / (self.channels * self.unit_bytes) * self.unit_frames
    }

    fn block_frames(&self) -> u64 {
        self.frames_in(self.block_bytes)
    }

    fn is_psx(&self) -> bool {
        self.codec == CODEC_ID_ADPCM_PSX
    }
}

/// A big-endian or little-endian reader of the header bytes.
struct Head<'a>(&'a [u8]);

impl Head<'_> {
    fn bytes<const N: usize>(&self, off: usize) -> Result<[u8; N]> {
        self.0
            .get(off..off + N)
            .and_then(|b| b.try_into().ok())
            .map_or_else(|| decode_error("vag: truncated header"), Ok)
    }

    fn le(&self, off: usize) -> Result<u32> {
        self.bytes(off).map(u32::from_le_bytes)
    }

    fn be(&self, off: usize) -> Result<u32> {
        self.bytes(off).map(u32::from_be_bytes)
    }

    fn tag(&self, off: usize, tag: &[u8]) -> bool {
        self.0.get(off..off + tag.len()) == Some(tag)
    }
}

/// The layout of a PlayStation ADPCM stream of blocks of `interleave` bytes per channel.
fn psx_layout(channels: u64, rate: u32, interleave: u64, data_start: u64) -> Result<Layout> {
    if channels == 0 || channels > MAX_ADPCM_CHANNELS {
        return unsupported_error("vag: unsupported number of channels");
    }

    // The decoder requires whole frames in the block of each channel.
    if interleave == 0 || interleave % FRAME_BYTES != 0 {
        return decode_error("vag: invalid block size");
    }

    let block_bytes = interleave * channels;
    if block_bytes > MAX_BLOCK_BYTES {
        return limit_error("vag: block size is too large");
    }

    Ok(Layout {
        codec: CODEC_ID_ADPCM_PSX,
        channels,
        sample_rate: rate,
        unit_bytes: FRAME_BYTES,
        unit_frames: FRAME_SAMPLES,
        block_bytes,
        // Blocks of a frame per channel are batched; batching changes nothing for the decoder.
        packet_blocks: (PACKET_BYTES / block_bytes).max(1),
        data_start,
        declared_frames: None,
        vpk: None,
    })
}

/// A PCM stream of 16-bit samples.
fn pcm_layout(
    codec: AudioCodecId,
    channels: u64,
    rate: u32,
    block_bytes: u64,
    packet_blocks: u64,
    data_start: u64,
) -> Result<Layout> {
    if channels == 0 || channels > MAX_PCM_CHANNELS {
        return unsupported_error("vag: unsupported number of channels");
    }

    if block_bytes == 0 || block_bytes % (2 * channels) != 0 {
        return decode_error("vag: invalid block size");
    }

    if block_bytes > MAX_BLOCK_BYTES {
        return limit_error("vag: block size is too large");
    }

    Ok(Layout {
        codec,
        channels,
        sample_rate: rate,
        unit_bytes: 2,
        unit_frames: 1,
        block_bytes,
        packet_blocks,
        data_start,
        declared_frames: None,
        vpk: None,
    })
}

fn nonzero_rate(rate: u32) -> Result<u32> {
    if rate == 0 { decode_error("vag: invalid sample rate") } else { Ok(rate) }
}

/// Parses the header of a Sony PS2 VAG (`libavformat/vag.c`).
fn parse_vag(h: &Head<'_>) -> Result<Layout> {
    let channels = 1 + u64::from(h.be(4)? == 4);
    let size = u64::from(h.be(12)?);
    let rate = nonzero_rate(h.be(16)?)?;

    // A second header at 0x1000 means that the file is stored in blocks of 0x1000 bytes per
    // channel, from its start.
    if h.tag(0x1000, b"VAGp") {
        return psx_layout(channels, rate, 0x1000, 0);
    }

    let mut layout =
        psx_layout(channels, rate, FRAME_BYTES, if channels > 1 { 0x80 } else { 0x30 })?;
    // The size is the data size of a mono stream. FFmpeg derives nothing from that of a stereo
    // stream.
    if channels == 1 {
        layout.declared_frames = Some(size / FRAME_BYTES * FRAME_SAMPLES);
    }
    Ok(layout)
}

/// Parses the header of a Konami PS2 SVAG (`libavformat/svag.c`).
fn parse_svag(h: &Head<'_>) -> Result<Layout> {
    let size = u64::from(h.le(4)?);
    let rate = nonzero_rate(h.le(8)?)?;
    let channels = u64::from(h.le(12)?);
    let align = u64::from(h.le(16)?);

    if channels == 0 || channels > MAX_ADPCM_CHANNELS {
        return decode_error("vag: invalid number of channels");
    }

    let mut layout = psx_layout(channels, rate, align, 0x800)?;
    layout.declared_frames = Some(size / (FRAME_BYTES * channels) * FRAME_SAMPLES);
    Ok(layout)
}

/// Parses the header of a Sony PS2 VPK (`libavformat/vpk.c`).
fn parse_vpk(h: &Head<'_>) -> Result<Layout> {
    // FFmpeg computes this in 32 bits, which wraps for the sizes above 153 MB.
    let duration = u64::from(h.le(4)?) * FRAME_SAMPLES / FRAME_BYTES;
    let offset = u64::from(h.le(8)?);
    let block_align = u64::from(h.le(12)?);
    let rate = nonzero_rate(h.le(16)?)?;
    let channels = u64::from(h.le(20)?);

    if channels == 0 || channels > MAX_ADPCM_CHANNELS {
        return decode_error("vag: invalid number of channels");
    }

    if block_align == 0 || block_align % (FRAME_BYTES * channels) != 0 {
        return decode_error("vag: invalid block size");
    }

    if offset < 24 {
        return decode_error("vag: invalid data offset");
    }

    let mut layout = psx_layout(channels, rate, block_align / channels, offset)?;
    // One block per packet: the last block is read differently.
    layout.packet_blocks = 1;

    let block_frames = layout.block_frames();
    let mut block_count = duration.div_ceil(block_frames);

    // A last block that is cut short holds only the frames of the declared length. FFmpeg also
    // cuts a last block that is whole and so loses it, which is not done here. Only whole frames
    // are stored: a tail of less than one frame per channel holds no audio and is dropped (a
    // zero-size chunk per channel would also be invalid).
    let last_frames = duration % block_frames;
    let last_bytes = last_frames / FRAME_SAMPLES * FRAME_BYTES * channels;
    if last_frames != 0 && last_bytes == 0 {
        block_count -= 1;
    }

    layout.declared_frames = Some(duration);
    layout.vpk = Some(VpkTail { block_count, last_bytes });
    Ok(layout)
}

/// Parses the header of a Sony PS2 ADS (`libavformat/ads.c`).
fn parse_ads(h: &Head<'_>) -> Result<Layout> {
    let codec = h.le(8)?;
    let rate = nonzero_rate(h.le(12)?)?;
    let channels = u64::from(h.le(16)?);
    let align = u64::from(h.le(20)?);

    if channels == 0 || align == 0 {
        return decode_error("vag: invalid header");
    }

    if codec == 1 {
        // The channels of a block are consecutive: a block is a packet.
        pcm_layout(CODEC_ID_PCM_S16LE_PLANAR, channels, rate, align * channels, 1, 40)
    }
    else {
        psx_layout(channels, rate, align, 40)
    }
}

/// Parses the header of a Sony PS3 XVAG (`libavformat/xvag.c`).
fn parse_xvag(h: &Head<'_>) -> Result<Layout> {
    // The byte order is that for which the offset to the data is the smaller.
    let offset = h.le(4)?;
    let big = offset > offset.swap_bytes();
    let read = |off| if big { h.be(off) } else { h.le(off) };

    let offset = u64::from(if big { offset.swap_bytes() } else { offset });
    let codec = read(36)?;
    let channels = u64::from(read(40)?);
    let duration = u64::from(read(48)?);
    let rate = nonzero_rate(read(60)?)?;

    if channels == 0 || channels > MAX_PCM_CHANNELS {
        return decode_error("vag: invalid number of channels");
    }

    if codec != 0x1c {
        return unsupported_error("vag: unsupported XVAG codec");
    }

    if offset < 64 {
        return decode_error("vag: invalid data offset");
    }

    // The data of the MP3 streams starts with a frame header.
    if h.tag(offset as usize, &[0xff, 0xfb]) {
        return unsupported_error("vag: XVAG MP3 streams are unsupported");
    }

    let mut layout = psx_layout(channels, rate, FRAME_BYTES, offset)?;
    layout.declared_frames = Some(duration);
    Ok(layout)
}

/// Parses the header of a Sony PS3 MSF (`libavformat/msf.c`).
fn parse_msf(h: &Head<'_>) -> Result<Layout> {
    let codec = h.be(4)?;
    let channels = u64::from(h.be(8)?);
    let size = u64::from(h.be(12)?);
    let rate = nonzero_rate(h.be(16)?)?;

    if channels == 0 || channels >= i32::MAX as u64 / 1024 {
        return decode_error("vag: invalid number of channels");
    }

    // The PCM is read in packets of 1024 bytes per channel.
    let mut layout = match codec {
        0 => pcm_layout(CODEC_ID_PCM_S16BE, channels, rate, 1024 * channels, 1, 0x40)?,
        1 => pcm_layout(CODEC_ID_PCM_S16LE, channels, rate, 1024 * channels, 1, 0x40)?,
        3 => psx_layout(channels, rate, FRAME_BYTES, 0x40)?,
        4..=6 => return unsupported_error("vag: MSF ATRAC3 streams are unsupported"),
        7 => return unsupported_error("vag: MSF MP3 streams are unsupported"),
        _ => return unsupported_error("vag: unsupported MSF codec"),
    };

    layout.declared_frames = Some(layout.frames_in(size));
    Ok(layout)
}

/// A function that parses the header of a variant of the format.
type ParseHead = fn(&Head<'_>) -> Result<Layout>;

fn identify(head: &[u8]) -> Option<(&'static FormatInfo, ParseHead)> {
    let h = Head(head);

    if h.tag(0, b"VAGp") {
        Some((&VAG_FORMAT_INFO, parse_vag))
    }
    else if h.tag(0, b"Svag") {
        Some((&SVAG_FORMAT_INFO, parse_svag))
    }
    else if h.tag(0, b"VPK ") {
        Some((&VPK_FORMAT_INFO, parse_vpk))
    }
    else if h.tag(0, b"SShd") {
        Some((&ADS_FORMAT_INFO, parse_ads))
    }
    else if h.tag(0, b"XVAG") {
        Some((&XVAG_FORMAT_INFO, parse_xvag))
    }
    else if h.tag(0, b"MSF") {
        Some((&MSF_FORMAT_INFO, parse_msf))
    }
    else {
        None
    }
}

/// Reader of the Sony PlayStation ADPCM (VAG) family of files.
pub struct VagReader<'s> {
    reader: MediaSourceStream<'s>,
    info: &'static FormatInfo,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    metadata: MetadataLog,
    layout: Layout,
    /// The position of the first block, from the start of the source.
    data_start: u64,
    /// The index of the next block to read.
    next_block: u64,
    /// The number of frames, if it is known.
    num_frames: Option<u64>,
    /// Set once the end of the audio was read.
    eof: bool,
}

impl<'s> VagReader<'s> {
    pub fn try_new(mut source: MediaSourceStream<'s>, _options: FormatOptions) -> Result<Self> {
        let base = source.pos();

        let mut head = vec![0u8; HEAD_LEN];
        let len = read_up_to(&mut source, &mut head)?;
        head.truncate(len);

        let Some((info, parse)) = identify(&head)
        else {
            return unsupported_error("vag: not a VAG stream");
        };

        let layout = parse(&Head(&head))?;
        let data_start = base + layout.data_start;

        // Move to the first block, which is behind the header and may be before the read bytes.
        if layout.data_start <= len as u64 {
            if source.seek_buffered(data_start) != data_start {
                return decode_error("vag: failed to rewind to the data");
            }
        }
        else {
            source.ignore_bytes(layout.data_start - len as u64)?;

            // The data of an XVAG file that is behind the read bytes: check for MP3 here.
            if info.short_name == "xvag" {
                let mut first = [0u8; 2];
                if read_up_to(&mut source, &mut first)? == 2 && first == [0xff, 0xfb] {
                    return unsupported_error("vag: XVAG MP3 streams are unsupported");
                }
                source.seek_buffered(data_start);
            }
        }

        // The number of frames in the data.
        let available = source.byte_len().map(|total| {
            let bytes = total.saturating_sub(data_start);
            let blocks = bytes / layout.block_bytes;
            let rest = bytes % layout.block_bytes;
            blocks * layout.block_frames() + layout.frames_in(rest)
        });

        let num_frames = match (layout.declared_frames, available) {
            (Some(declared), Some(available)) => Some(declared.min(available)),
            (declared, available) => declared.or(available),
        };

        let mut params = AudioCodecParameters::new();
        params
            .for_codec(layout.codec)
            .with_sample_rate(layout.sample_rate)
            .with_channels(channels_for(layout.channels as usize))
            .with_max_frames_per_packet(layout.block_frames() * layout.packet_blocks);

        if layout.is_psx() {
            params.with_bits_per_coded_sample(4).with_frames_per_block(layout.block_frames());
        }
        else {
            params.with_bits_per_coded_sample(16);
        }

        let time_base = frame_time_base(layout.sample_rate);

        let mut track = Track::new(0);
        track.time_base = time_base;
        track.num_frames = num_frames;
        track.duration = num_frames.map(Duration::new);
        track.with_codec_params(CodecParameters::Audio(params));

        let mut media_info = MediaInfo::new();
        media_info.time_base = time_base;
        media_info.duration = num_frames.map(Duration::new);

        Ok(VagReader {
            reader: source,
            info,
            media_info,
            tracks: vec![track],
            metadata: MetadataLog::default(),
            layout,
            data_start,
            next_block: 0,
            num_frames,
            eof: false,
        })
    }

    /// Reads the cut short last block of a VPK file: the frames of each channel are at the start
    /// of the block of the channel.
    fn read_vpk_tail(&mut self, tail: VpkTail) -> Result<Vec<u8>> {
        let channels = self.layout.channels;
        let chunk = tail.last_bytes / channels;
        let skip = (self.layout.block_bytes - tail.last_bytes) / channels;

        let mut buf = vec![0u8; (chunk * channels) as usize];

        for ch in buf.chunks_exact_mut(chunk as usize) {
            if read_up_to(&mut self.reader, ch)? < ch.len() {
                self.eof = true;
                return Ok(Vec::new());
            }
            // The last channel may be at the end of the stream.
            let _ = self.reader.ignore_bytes(skip);
        }

        Ok(buf)
    }
}

impl Scoreable for VagReader<'_> {
    fn score(mut src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        let mut head = [0u8; 40];
        let len = read_up_to(&mut src, &mut head)?;
        let h = Head(&head[..len]);

        // The scores are those of FFmpeg's probes: the VAG, SVAG and XVAG signatures are strong.
        let score = if h.tag(0, b"VAGp\0\0\0")
            || h.tag(0, b"Svag")
            || (h.tag(0, b"XVAG") && h.tag(32, b"fmat"))
        {
            255
        }
        else if h.tag(0, b"VPK ") || (h.tag(0, b"SShd") && h.tag(32, b"SSbd")) {
            170
        }
        else if h.tag(0, b"MSF") && h.be(8).is_ok_and(|c| c > 0) && h.be(16).is_ok_and(|r| r > 0)
        {
            // FFmpeg rates the files with a codec it does not know lower.
            if h.be(4).is_ok_and(|codec| codec > 16) { 50 } else { 170 }
        }
        else {
            return Ok(Score::Unsupported);
        };

        Ok(Score::Supported(score))
    }
}

impl ProbeableFormat<'_> for VagReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + '_>> {
        Ok(Box::new(VagReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[
            support_format!(VAG_FORMAT_INFO, &["vag"], &["audio/x-vag"], &[b"VAGp"]),
            support_format!(SVAG_FORMAT_INFO, &["svag"], &["audio/x-svag"], &[b"Svag"]),
            support_format!(VPK_FORMAT_INFO, &["vpk"], &["audio/x-vpk"], &[b"VPK "]),
            support_format!(ADS_FORMAT_INFO, &["ads", "ss2"], &["audio/x-ads"], &[b"SShd"]),
            support_format!(XVAG_FORMAT_INFO, &["xvag"], &["audio/x-xvag"], &[b"XVAG"]),
            support_format!(MSF_FORMAT_INFO, &["msf"], &["audio/x-msf"], &[b"MSF"]),
        ]
    }
}

impl FormatReader for VagReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        self.info
    }

    fn media_info(&self) -> &MediaInfo {
        &self.media_info
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        if self.eof {
            return Ok(None);
        }

        let mut blocks = self.layout.packet_blocks;
        let mut tail = None;

        if let Some(vpk) = self.layout.vpk {
            if self.next_block >= vpk.block_count {
                self.eof = true;
                return Ok(None);
            }

            let left = vpk.block_count - self.next_block;
            if left == 1 && vpk.last_bytes > 0 {
                tail = Some(vpk);
            }
            blocks = blocks.min(left);
        }

        let buf = if let Some(vpk) = tail {
            let buf = self.read_vpk_tail(vpk)?;
            self.eof = true;
            buf
        }
        else {
            let mut buf = vec![0u8; (blocks * self.layout.block_bytes) as usize];
            let len = read_up_to(&mut self.reader, &mut buf)?;

            if len < buf.len() {
                self.eof = true;
            }

            buf.truncate(len);
            buf
        };

        // Only whole frames are decodable.
        let unit = self.layout.channels * self.layout.unit_bytes;
        let mut buf = buf;
        buf.truncate((buf.len() as u64 / unit * unit) as usize);

        if buf.is_empty() {
            self.eof = true;
            return Ok(None);
        }

        let pts = self.next_block * self.layout.block_frames();
        let frames = self.layout.frames_in(buf.len() as u64);
        self.next_block += blocks;

        // The frames after the declared length are padding.
        let dur = match self.num_frames {
            Some(total) => total.saturating_sub(pts).min(frames),
            None => frames,
        };

        Ok(Some(
            PacketBuilder::new()
                .track_id(0)
                .pts(Timestamp::new(pts as i64))
                .dur(Duration::new(dur))
                .trim_end(Duration::new(frames - dur))
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
        if !self.reader.is_seekable() {
            return unseekable();
        }

        let Some(time_base) = self.media_info.time_base
        else {
            return unseekable();
        };

        let required_ts = required_timestamp(to, time_base)?;

        if self.num_frames.is_some_and(|frames| required_ts >= frames) {
            return seek_error(SeekErrorKind::OutOfRange);
        }

        let block_frames = self.layout.block_frames();
        let mut block = required_ts / block_frames;

        // The history of the ADPCM starts from silence; decode some frames before the required
        // position so that it converges.
        if mode == SeekMode::Accurate && self.layout.is_psx() {
            block = block.saturating_sub(SEEK_PREROLL_FRAMES.div_ceil(block_frames));
        }

        self.reader.seek(SeekFrom::Start(self.data_start + block * self.layout.block_bytes))?;
        self.next_block = block;
        self.eof = false;

        Ok(SeekedTo {
            track_id: 0,
            required_ts: Timestamp::new(required_ts as i64),
            actual_ts: Timestamp::new((block * block_frames) as i64),
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

    fn open(data: Vec<u8>) -> Result<VagReader<'static>> {
        let mss = MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default());
        VagReader::try_new(mss, FormatOptions::default())
    }

    /// A VPK stream of `size` data bytes (as the header counts them), the block alignment
    /// `align`, `channels` channels and `blocks` whole blocks of data.
    fn vpk(size: u32, align: u32, channels: u32, blocks: usize) -> Vec<u8> {
        let mut d = vec![0u8; 0x800];
        d[..4].copy_from_slice(b"VPK ");
        d[4..8].copy_from_slice(&size.to_le_bytes());
        d[8..12].copy_from_slice(&0x800u32.to_le_bytes());
        d[12..16].copy_from_slice(&align.to_le_bytes());
        d[16..20].copy_from_slice(&44100u32.to_le_bytes());
        d[20..24].copy_from_slice(&channels.to_le_bytes());
        d.resize(d.len() + blocks * align as usize, 0);
        d
    }

    #[test]
    fn vpk_tail_below_one_frame_per_channel_is_dropped() {
        // 4 whole frames per channel in one block, then a tail of 1 sample: the tail holds no
        // frame of a channel (this used to panic in `chunks_exact_mut(0)`).
        let align = 2 * 4 * FRAME_BYTES as u32;
        let size = 4 * FRAME_BYTES as u32;
        let mut reader = open(vpk(size, align, 2, 1)).unwrap();
        let mut n = 0;
        while reader.next_packet().unwrap().is_some() {
            n += 1;
        }
        assert_eq!(n, 1);

        // A header whose duration leaves a partial frame in the tail.
        let mut d = vpk(size + 1, align, 2, 2);
        d[4..8].copy_from_slice(&(size + 1).to_le_bytes());
        let mut reader = open(d).unwrap();
        while reader.next_packet().unwrap().is_some() {}
    }

    #[test]
    fn vpk_tail_of_whole_frames_is_read() {
        // One block of 4 frames per channel and a tail of one frame per channel.
        let align = 2 * 4 * FRAME_BYTES as u32;
        let size = 5 * FRAME_BYTES as u32;
        let mut reader = open(vpk(size, align, 2, 2)).unwrap();
        let first = reader.next_packet().unwrap().unwrap();
        let tail = reader.next_packet().unwrap().unwrap();
        assert_eq!(first.data.len(), align as usize);
        assert_eq!(tail.data.len(), 2 * FRAME_BYTES as usize);
        assert!(reader.next_packet().unwrap().is_none());
    }

    #[test]
    fn rejects_a_huge_block_size() {
        let mut d = vpk(1600, 16 * 2, 2, 0);
        d[12..16].copy_from_slice(&0x8000_0000u32.to_le_bytes());
        assert!(matches!(open(d), Err(Error::LimitError(_))));

        // An SVAG stream with a huge alignment.
        let mut d = vec![0u8; 0x1000];
        d[..4].copy_from_slice(b"Svag");
        d[4..8].copy_from_slice(&1000u32.to_le_bytes());
        d[8..12].copy_from_slice(&44100u32.to_le_bytes());
        d[12..16].copy_from_slice(&2u32.to_le_bytes());
        d[16..20].copy_from_slice(&0x8000_0000u32.to_le_bytes());
        assert!(matches!(open(d), Err(Error::LimitError(_))));
    }
}
