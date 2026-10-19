// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Parsing of the RealAudio stream headers: the type specific data of an `MDPR` chunk, and the
//! header of a headerless `.ra` file.
//!
//! The layout and the validation follow FFmpeg's `libavformat/rmdec.c`
//! (`rm_read_audio_stream_info`, `ff_rm_read_mdpr_codecdata`) and the FourCC table of
//! `libavformat/rm.c`.

use symphonia_core::codecs::audio::AudioCodecId;
use symphonia_core::codecs::audio::well_known::{
    CODEC_ID_AAC, CODEC_ID_AC3, CODEC_ID_ATRAC3, CODEC_ID_COOK, CODEC_ID_RA10, CODEC_ID_RA20,
    CODEC_ID_RALF, CODEC_ID_SIPR,
};
use symphonia_core::errors::{Result, decode_error, limit_error, unsupported_error};
use symphonia_core::io::ReadBytes;

/// The largest codec specific data that is read. Real streams carry a few tens of bytes.
const MAX_EXTRA_DATA_LEN: u32 = 1 << 20;

/// The largest buffer of the de-interleaver (`frame_size * sub_packet_h`).
pub(crate) const MAX_GROUP_LEN: u64 = 1 << 24;

/// The size of the packets of a RealAudio 1.0 (14.4k) stream that are read from a `.ra` file, in
/// FFmpeg's `RAW_PACKET_SIZE`. It is a multiple of the frame size of 20 bytes.
pub(crate) const RAW_PACKET_LEN: usize = 1000;

/// The size of a RealAudio 1.0 (14.4k, `lpcJ`) frame in bytes: 160 samples of 8 kHz audio.
pub(crate) const RA144_FRAME_LEN: usize = 20;

/// The size of the packets of SIPR for each flavor of the codec, in FFmpeg's
/// `ff_sipr_subpk_size` (`libavformat/rmsipr.c`).
const SIPR_SUB_PACKET_LEN: [usize; 4] = [29, 19, 37, 20];

/// The method that interleaves the audio frames in the packets of a stream.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum Interleave {
    /// Not interleaved (`Int0`, or the stream has no interleaver).
    None,
    /// Cook and ATRAC3 (`genr`).
    Genr,
    /// RealAudio 2.0 (28.8k, `Int4`).
    Int4,
    /// SIPR (`sipr`): the frames are bit-swapped across the packets of a group.
    Sipr,
    /// Variable bit rate frames with a table of frame lengths (`vbrs`, `vbrf`), used by AAC.
    Vbr,
}

/// The parameters of an audio stream.
#[derive(Clone, Debug)]
pub(crate) struct AudioInfo {
    /// The codec, or `None` if the FourCC of the stream is not known.
    pub codec: Option<AudioCodecId>,
    pub sample_rate: u32,
    pub channels: u32,
    /// The bit rate in bits per second, or 0 if it is not known.
    pub bit_rate: u32,
    /// The codec specific data, exactly as FFmpeg passes it to the decoder.
    pub extra: Option<Box<[u8]>>,
    pub interleave: Interleave,
    /// The size of the frame of the stream in the container (`audio_framesize` of FFmpeg).
    pub frame_size: usize,
    /// The number of packets that make up a group of interleaved frames (`sub_packet_h`).
    pub sub_packet_h: usize,
    /// The size of the sub packets of Cook and ATRAC3 (`sub_packet_size`).
    pub sub_packet_size: usize,
    /// The size of the coded frame of RealAudio 2.0 (`coded_framesize`).
    pub coded_frame_size: usize,
    /// The size of the frames that are given to the decoder; 0 if the packets are not split.
    pub block_align: usize,
    /// The title, author, copyright, and comment strings of a `.ra` file header, if not empty.
    pub text: Vec<(&'static str, Vec<u8>)>,
}

impl AudioInfo {
    fn new() -> Self {
        AudioInfo {
            codec: None,
            sample_rate: 0,
            channels: 0,
            bit_rate: 0,
            extra: None,
            interleave: Interleave::None,
            frame_size: 0,
            sub_packet_h: 0,
            sub_packet_size: 0,
            coded_frame_size: 0,
            block_align: 0,
            text: Vec::new(),
        }
    }

    /// Returns true if frames of the stream are put in groups (de-interleaved).
    pub fn is_grouped(&self) -> bool {
        matches!(self.interleave, Interleave::Genr | Interleave::Int4 | Interleave::Sipr)
    }
}

/// The names of the strings of the `CONT` chunk, and of the header of a `.ra` file.
pub(crate) const TEXT_KEYS: [&str; 4] = ["title", "author", "copyright", "comment"];

/// Map the FourCC of a codec to a codec ID. FFmpeg's `ff_rm_codec_tags`.
fn codec_from_fourcc(tag: &[u8; 4]) -> Option<AudioCodecId> {
    match tag {
        b"dnet" => Some(CODEC_ID_AC3),
        b"lpcJ" => Some(CODEC_ID_RA10),
        b"28_8" => Some(CODEC_ID_RA20),
        b"cook" => Some(CODEC_ID_COOK),
        b"atrc" => Some(CODEC_ID_ATRAC3),
        b"sipr" => Some(CODEC_ID_SIPR),
        b"raac" | b"racp" => Some(CODEC_ID_AAC),
        b"LSD:" => Some(CODEC_ID_RALF),
        _ => None,
    }
}

/// Read a string with a one byte length.
fn read_str8<R: ReadBytes>(src: &mut R) -> Result<Box<[u8]>> {
    let len = usize::from(src.read_u8()?);
    Ok(src.read_boxed_slice_exact(len)?)
}

/// The first four bytes of a string, up to the first NUL, padded with NULs.
fn tag_of(s: &[u8]) -> [u8; 4] {
    let mut tag = [0; 4];

    for (dst, &b) in tag.iter_mut().zip(s.iter().take_while(|&&b| b != 0)) {
        *dst = b;
    }

    tag
}

/// Read the four strings of the metadata (title, author, copyright, comment) with a length of
/// one byte, or two bytes if `wide`. Empty strings are not returned.
pub(crate) fn read_text<R: ReadBytes>(
    src: &mut R,
    wide: bool,
) -> Result<Vec<(&'static str, Vec<u8>)>> {
    let mut text = Vec::new();

    for key in TEXT_KEYS {
        let len = if wide { usize::from(src.read_be_u16()?) } else { usize::from(src.read_u8()?) };

        if len > 0 {
            text.push((key, src.read_boxed_slice_exact(len)?.into_vec()));
        }
    }

    Ok(text)
}

/// Read codec specific data of `len` bytes.
fn read_extra<R: ReadBytes>(src: &mut R, len: u32) -> Result<Option<Box<[u8]>>> {
    if len > MAX_EXTRA_DATA_LEN {
        return limit_error("real: codec data is too large");
    }

    Ok((len > 0).then(|| src.read_boxed_slice_exact(len as usize)).transpose()?)
}

/// Read the RealAudio header that follows the `.ra\xfd` signature (the signature has been read).
///
/// For the `MDPR` chunk, `read_all` is false and `bit_rate` is the average bit rate of the stream.
/// For a `.ra` file, `read_all` is true, and the metadata that follows the header is read.
pub(crate) fn read_audio_header<R: ReadBytes>(
    src: &mut R,
    bit_rate: u32,
    read_all: bool,
) -> Result<AudioInfo> {
    let version = src.read_be_u16()?;

    let mut info = AudioInfo::new();
    info.bit_rate = bit_rate;

    if version == 3 {
        // RealAudio 1.0 (14.4k).
        let header_size = u64::from(src.read_be_u16()?);
        let start = src.pos();

        src.ignore_bytes(8)?;
        let bytes_per_minute = src.read_be_u16()?;
        src.ignore_bytes(4)?;

        info.text = read_text(src, false)?;

        if start + header_size >= src.pos() + 2 {
            // The FourCC (always "lpcJ").
            src.read_u8()?;
            read_str8(src)?;
        }

        // Skip the rest of the header.
        if start + header_size > src.pos() {
            src.ignore_bytes(start + header_size - src.pos())?;
        }

        if bytes_per_minute != 0 {
            info.bit_rate = u32::from(bytes_per_minute) * 8 / 60;
        }

        info.sample_rate = 8000;
        info.channels = 1;
        info.codec = Some(CODEC_ID_RA10);
        info.interleave = Interleave::None;

        return Ok(info);
    }

    if version != 4 && version != 5 {
        return unsupported_error("real: unsupported RealAudio header version");
    }

    // Version 4 and 5.
    src.ignore_bytes(2)?; // Unused.
    src.read_be_u32()?; // ".ra4"
    src.read_be_u32()?; // Data size.
    src.read_be_u16()?; // Version (again).
    src.read_be_u32()?; // Header size.
    let flavor = usize::from(src.read_be_u16()?);
    let coded_frame_size = src.read_be_u32()?;

    if coded_frame_size > i32::MAX as u32 {
        return decode_error("real: invalid coded frame size");
    }

    src.read_be_u32()?;
    let bytes_per_minute = src.read_be_u32()?;

    if version == 4 && bytes_per_minute != 0 {
        info.bit_rate = (u64::from(bytes_per_minute) * 8 / 60).min(u64::from(u32::MAX)) as u32;
    }

    src.read_be_u32()?;
    let sub_packet_h = usize::from(src.read_be_u16()?);
    let frame_size = usize::from(src.read_be_u16()?);
    let sub_packet_size = usize::from(src.read_be_u16()?);
    src.read_be_u16()?;

    if version == 5 {
        src.ignore_bytes(6)?;
    }

    info.sample_rate = u32::from(src.read_be_u16()?);
    src.read_be_u32()?;
    info.channels = u32::from(src.read_be_u16()?);

    let (interleaver, fourcc) = if version == 5 {
        (src.read_quad_bytes()?, src.read_quad_bytes()?)
    }
    else {
        let interleaver = tag_of(&read_str8(src)?);
        (interleaver, tag_of(&read_str8(src)?))
    };

    info.codec = codec_from_fourcc(&fourcc);
    info.coded_frame_size = coded_frame_size as usize;
    info.sub_packet_h = sub_packet_h;
    info.sub_packet_size = sub_packet_size;
    info.block_align = frame_size;

    match info.codec {
        Some(CODEC_ID_RA20) => {
            // The frame size of the container is the size of the group of frames.
            info.frame_size = frame_size;
            info.block_align = info.coded_frame_size;
        }
        Some(CODEC_ID_COOK | CODEC_ID_ATRAC3 | CODEC_ID_SIPR) => {
            let codec_data_len = if read_all {
                0
            }
            else {
                src.read_be_u16()?;
                src.read_u8()?;

                if version == 5 {
                    src.read_u8()?;
                }

                src.read_be_u32()?
            };

            info.frame_size = frame_size;

            if info.codec == Some(CODEC_ID_SIPR) {
                let Some(&len) = SIPR_SUB_PACKET_LEN.get(flavor)
                else {
                    return decode_error("real: invalid SIPR flavor");
                };

                info.block_align = len;
            }
            else {
                if sub_packet_size == 0 {
                    return decode_error("real: invalid sub packet size");
                }

                info.block_align = sub_packet_size;
            }

            info.extra = read_extra(src, codec_data_len)?;
        }
        Some(CODEC_ID_AAC) => {
            src.read_be_u16()?;
            src.read_u8()?;

            if version == 5 {
                src.read_u8()?;
            }

            let codec_data_len = src.read_be_u32()?;

            if codec_data_len >= 1 {
                // The first byte of the codec data is skipped: the rest is the audio specific
                // config.
                src.read_u8()?;
                info.extra = read_extra(src, codec_data_len - 1)?;
            }
        }
        _ => (),
    }

    info.interleave = match &interleaver {
        b"Int4" => Interleave::Int4,
        b"genr" => Interleave::Genr,
        b"sipr" => Interleave::Sipr,
        b"Int0" => Interleave::None,
        b"vbrs" | b"vbrf" => Interleave::Vbr,
        _ => return decode_error("real: unknown interleaver"),
    };

    let (w, h) = (info.frame_size as u64, sub_packet_h as u64);
    let cfs = info.coded_frame_size as u64;

    match info.interleave {
        Interleave::Int4 => {
            if cfs > w || h <= 1 || cfs * h > (2 + (h & 1)) * w {
                return decode_error("real: invalid Int4 interleaver parameters");
            }

            if cfs * h != 2 * w {
                return unsupported_error("real: mismatching Int4 interleaver parameters");
            }
        }
        Interleave::Genr => {
            let sps = info.sub_packet_size as u64;

            if sps == 0 || sps > w || w % sps != 0 {
                return decode_error("real: invalid genr interleaver parameters");
            }
        }
        _ => (),
    }

    if info.is_grouped() {
        let group_len = w * h;

        if info.block_align == 0 || group_len < info.block_align as u64 {
            return decode_error("real: invalid frame size");
        }

        if group_len > MAX_GROUP_LEN {
            return limit_error("real: interleaver group is too large");
        }
    }

    if read_all {
        src.ignore_bytes(3)?;
        info.text = read_text(src, false)?;
    }

    Ok(info)
}

/// Parse the type specific data of a RealAudio Lossless Format (RALF) stream. The data begins
/// with the `LSD:` signature and is the codec specific data of the decoder.
pub(crate) fn read_ralf_header(data: &[u8]) -> AudioInfo {
    let mut info = AudioInfo::new();

    info.codec = Some(CODEC_ID_RALF);
    info.extra = Some(data.into());

    // The channels and sample rate, as the decoder finds them (`libavcodec/ralf.c`).
    if let (Some(ch), Some(rate)) = (data.get(8..10), data.get(12..16)) {
        info.channels = u32::from(u16::from_be_bytes([ch[0], ch[1]]));
        info.sample_rate = u32::from_be_bytes([rate[0], rate[1], rate[2], rate[3]]);
    }

    info
}
