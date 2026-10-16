// Symphonia Shorten bundle
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Shorten stream header: the `ajkg` magic and parameters, and the verbatim canonical WAVE/AIFF
//! header that follows them and carries the sample rate.
//!
//! Ported from `read_header`, `decode_wave_header` and `decode_aiff_header` of FFmpeg's
//! `libavcodec/shorten.c` (LGPL-2.1+), see `NOTICE`. The parameter coding is described in T. Robinson,
//! "SHORTEN: Simple lossless and near-lossless waveform compression", Cambridge University
//! Engineering Department technical report CUED/F-INFENG/TR.156, 1994.

use symphonia_core::errors::{Result, decode_error, unsupported_error};

use crate::bits::BitReader;

/// Largest channel count FFmpeg's decoder accepts (`MAX_CHANNELS`).
pub(crate) const MAX_CHANNELS: usize = 8;
/// Largest block size (`MAX_BLOCKSIZE`).
pub(crate) const MAX_BLOCKSIZE: usize = 65535;
/// Block size of version 0 streams, which do not carry one (`DEFAULT_BLOCK_SIZE`).
pub(crate) const DEFAULT_BLOCKSIZE: usize = 256;

/// Rice parameter of the "unsigned long" size prefix of version 1+ parameters (`ULONGSIZE`).
const ULONGSIZE: u32 = 2;
/// Rice parameter of the file type field (`TYPESIZE`).
const TYPESIZE: u32 = 4;
/// Rice parameter of the channel count field (`CHANSIZE`).
const CHANSIZE: u32 = 0;
/// Rice parameter of the maximum LPC order field (`LPCQSIZE`).
pub(crate) const LPCQSIZE: u32 = 2;
/// Rice parameter of the skipped header byte count (`NSKIPSIZE`).
const NSKIPSIZE: u32 = 1;

/// Command function code size (`FNSIZE`).
pub(crate) const FNSIZE: u32 = 2;
/// The `FN_VERBATIM` function code.
pub(crate) const FN_VERBATIM: u32 = 9;
/// Rice parameter of the byte count of a verbatim chunk (`VERBATIM_CKSIZE_SIZE`).
pub(crate) const VERBATIM_CKSIZE_SIZE: u32 = 5;
/// Rice parameter of the bytes of a verbatim chunk (`VERBATIM_BYTE_SIZE`).
pub(crate) const VERBATIM_BYTE_SIZE: u32 = 8;
/// Smallest valid canonical (WAVE) header (`CANONICAL_HEADER_SIZE`).
const CANONICAL_HEADER_SIZE: usize = 44;
/// Largest verbatim header FFmpeg accepts (`OUT_BUFFER_SIZE`).
const MAX_HEADER_SIZE: usize = 16384;

/// The newest stream version of the reference encoder (`MAX_SUPPORTED_VERSION` in
/// shorten-3.6.1).
const MAX_VERSION: u8 = 3;

/// The "file type" parameter: the sample format of the original data (`TYPE_*` in FFmpeg; the
/// numbering is that of the shorten-3.6.1 reference encoder's `include/shorten.h`).
///
/// The byte order of the 16-bit types only describes how the original file stored the samples;
/// the coded values are the same.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FileType {
    S8,
    U8,
    S16,
    U16,
}

impl FileType {
    fn from_code(code: u32) -> Result<Self> {
        match code {
            1 => Ok(FileType::S8),
            2 => Ok(FileType::U8),
            3 | 5 => Ok(FileType::S16),
            4 | 6 => Ok(FileType::U16),
            // The reference encoder also knows (lossless and lossy) u-law and A-law types whose
            // output is companded bytes; FFmpeg does not decode them either.
            _ => unsupported_error("shorten: unsupported audio type"),
        }
    }

    fn code(self) -> u8 {
        match self {
            FileType::S8 => 1,
            FileType::U8 => 2,
            FileType::S16 => 3,
            FileType::U16 => 4,
        }
    }

    /// The initial value of every running mean (`init_offset` / `init_offset` in shorten-3.6.1).
    pub fn initial_mean(self) -> i32 {
        match self {
            FileType::U8 => 0x80,
            FileType::U16 => 0x8000,
            FileType::S8 | FileType::S16 => 0,
        }
    }

    /// Bytes per sample of the original data.
    pub fn bytes_per_sample(self) -> u32 {
        match self {
            FileType::S8 | FileType::U8 => 1,
            FileType::S16 | FileType::U16 => 2,
        }
    }
}

/// The decoding parameters carried by the `ajkg` stream header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Header {
    pub version: u8,
    pub file_type: FileType,
    pub channels: usize,
    /// The initial block size. A `FN_BLOCKSIZE` command may only decrease it.
    pub blocksize: usize,
    pub maxnlpc: usize,
    /// The number of blocks averaged for the running mean, or -1 for version 0 streams (FFmpeg's
    /// value, which makes the mean offset zero for them).
    pub nmean: i32,
}

impl Header {
    /// The number of samples of history each channel carries between blocks (`nwrap`).
    pub fn nwrap(&self) -> usize {
        self.maxnlpc.max(3)
    }

    /// The offset added to the QLPC prediction sum (`lpcqoffset`).
    pub fn lpcqoffset(&self) -> i32 {
        if self.version > 1 { 1 << 5 } else { 0 }
    }

    /// The length of [`Header::to_extra_data`].
    pub const EXTRA_DATA_LEN: usize = 16;

    /// Serializes the header and the byte swap flag of the source format for
    /// `AudioCodecParameters::extra_data`: `version(u8)`, `file_type(u8)`, `channels(u8)`,
    /// `blocksize(u32)`, `maxnlpc(u32)`, `nmean(i32)` (all little-endian) and `swap(u8)`.
    pub fn to_extra_data(self, swap: bool) -> Box<[u8]> {
        let mut out = Vec::with_capacity(Self::EXTRA_DATA_LEN);
        out.push(self.version);
        out.push(self.file_type.code());
        out.push(self.channels as u8);
        out.extend_from_slice(&(self.blocksize as u32).to_le_bytes());
        out.extend_from_slice(&(self.maxnlpc as u32).to_le_bytes());
        out.extend_from_slice(&self.nmean.to_le_bytes());
        out.push(u8::from(swap));
        out.into_boxed_slice()
    }

    /// Parses and validates the output of [`Header::to_extra_data`].
    pub fn from_extra_data(data: &[u8]) -> Result<(Header, bool)> {
        if data.len() < Self::EXTRA_DATA_LEN {
            return decode_error("shorten: extra data too short");
        }
        let word = |at: usize| [data[at], data[at + 1], data[at + 2], data[at + 3]];
        let header = Header {
            version: data[0],
            file_type: FileType::from_code(u32::from(data[1]))?,
            channels: usize::from(data[2]),
            blocksize: u32::from_le_bytes(word(3)) as usize,
            maxnlpc: u32::from_le_bytes(word(7)) as usize,
            nmean: i32::from_le_bytes(word(11)),
        };
        if header.version > MAX_VERSION
            || !(1..=MAX_CHANNELS).contains(&header.channels)
            || !(1..=MAX_BLOCKSIZE).contains(&header.blocksize)
            || header.maxnlpc > 1024
            || !(-1..=32768).contains(&header.nmean)
        {
            return decode_error("shorten: invalid extra data");
        }
        Ok((header, data[15] != 0))
    }
}

/// Reads an unsigned value coded with Rice parameter `k`; version 1+ streams code `k` itself first
/// (`get_uint`).
pub(crate) fn read_uint(br: &mut BitReader<'_>, version: u8, k: u32) -> Result<u32> {
    let k = if version != 0 {
        let k = br.golomb_u(ULONGSIZE)?;
        if k > 31 {
            return decode_error("shorten: invalid parameter width");
        }
        k
    }
    else {
        k
    };
    br.golomb_u(k)
}

/// Reads the `ajkg` magic and the parameters that follow it.
pub(crate) fn read_params(br: &mut BitReader<'_>) -> Result<Header> {
    if br.read_bits(32)? != u32::from_be_bytes(*b"ajkg") {
        return decode_error("shorten: missing magic 'ajkg'");
    }
    let version = br.read_bits(8)? as u8;
    if version > MAX_VERSION {
        return unsupported_error("shorten: unsupported stream version");
    }

    let file_type = FileType::from_code(read_uint(br, version, TYPESIZE)?)?;

    let channels = read_uint(br, version, CHANSIZE)? as usize;
    if !(1..=MAX_CHANNELS).contains(&channels) {
        return decode_error("shorten: invalid channel count");
    }

    let (mut blocksize, mut maxnlpc, mut nmean) = (DEFAULT_BLOCKSIZE, 0, -1);
    if version > 0 {
        blocksize = read_uint(br, version, DEFAULT_BLOCKSIZE.ilog2())? as usize;
        if !(1..=MAX_BLOCKSIZE).contains(&blocksize) {
            return decode_error("shorten: invalid block size");
        }

        maxnlpc = read_uint(br, version, LPCQSIZE)? as usize;
        if maxnlpc > 1024 {
            return decode_error("shorten: invalid maximum LPC order");
        }

        let mean = read_uint(br, version, 0)?;
        if mean > 32768 {
            return decode_error("shorten: invalid mean block count");
        }
        nmean = mean as i32;

        let skip_bytes = read_uint(br, version, NSKIPSIZE)? as usize;
        if skip_bytes > br.bits_left() / 8 {
            return decode_error("shorten: invalid skip byte count");
        }
        for _ in 0..skip_bytes {
            br.read_bits(8)?;
        }
    }

    Ok(Header { version, file_type, channels, blocksize, maxnlpc, nmean })
}

/// What the verbatim canonical header says about the original file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SourceFormat {
    pub sample_rate: u32,
    pub bits_per_sample: u16,
    /// FFmpeg byte-swaps the decoded 16-bit samples of AIFC files (`s->swap`).
    pub swap: bool,
    /// The size in bytes of the audio data the header announces, if it is a usable value.
    pub data_bytes: Option<u64>,
}

/// Reads the `FN_VERBATIM` command that must open every stream (after the parameters) and the
/// canonical header it carries, and interprets the header.
pub(crate) fn read_source_format(br: &mut BitReader<'_>) -> Result<Option<SourceFormat>> {
    // Streams of the sample types the encoder does not describe with a header (`-t s16hl`, ...)
    // start directly with audio commands; the caller must then know the format otherwise.
    let mut peek = br.clone();
    if peek.golomb_u(FNSIZE)? != FN_VERBATIM {
        return Ok(None);
    }
    br.golomb_u(FNSIZE)?;
    let size = br.golomb_u(VERBATIM_CKSIZE_SIZE)? as usize;
    if !(CANONICAL_HEADER_SIZE..MAX_HEADER_SIZE).contains(&size) {
        return decode_error("shorten: invalid verbatim header size");
    }
    let mut header = Vec::with_capacity(size);
    for _ in 0..size {
        header.push(br.golomb_u(VERBATIM_BYTE_SIZE)? as u8);
    }
    parse_source_header(&header).map(Some)
}

/// Interprets a verbatim canonical header: a RIFF/WAVE or FORM/AIFF(C) header, up to the audio.
pub(crate) fn parse_source_header(header: &[u8]) -> Result<SourceFormat> {
    match header.get(..4) {
        Some(b"RIFF") => parse_wave_header(header),
        Some(b"FORM") => parse_aiff_header(header),
        _ => unsupported_error("shorten: unsupported bit packing (not a WAVE or AIFF header)"),
    }
}

/// A cursor over a byte slice with the semantics of FFmpeg's `GetByteContext`: reads past the end
/// return zero and skips stop at the end.
struct Bytes<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Bytes<'a> {
    fn new(data: &'a [u8]) -> Self {
        Bytes { data, pos: 0 }
    }

    fn left(&self) -> usize {
        self.data.len() - self.pos
    }

    fn skip(&mut self, n: usize) {
        self.pos += n.min(self.left());
    }

    fn array<const N: usize>(&mut self) -> [u8; N] {
        let mut out = [0u8; N];
        if self.left() >= N {
            out.copy_from_slice(&self.data[self.pos..self.pos + N]);
            self.pos += N;
        }
        else {
            self.pos = self.data.len();
        }
        out
    }

    fn tag(&mut self) -> [u8; 4] {
        self.array()
    }

    fn u16_le(&mut self) -> u16 {
        u16::from_le_bytes(self.array())
    }

    fn u32_le(&mut self) -> u32 {
        u32::from_le_bytes(self.array())
    }

    fn u16_be(&mut self) -> u16 {
        u16::from_be_bytes(self.array())
    }

    fn u32_be(&mut self) -> u32 {
        u32::from_be_bytes(self.array())
    }

    fn u64_be(&mut self) -> u64 {
        u64::from_be_bytes(self.array())
    }
}

/// A usable "audio data size" field: zero and all-ones are what streaming writers leave behind.
fn plausible_size(size: u64) -> Option<u64> {
    (size != 0 && size != u64::from(u32::MAX)).then_some(size)
}

fn parse_wave_header(header: &[u8]) -> Result<SourceFormat> {
    let mut gb = Bytes::new(header);

    // Already checked: 'RIFF'.
    gb.skip(4);
    gb.skip(4); // Chunk size.
    if &gb.tag() != b"WAVE" {
        return decode_error("shorten: missing WAVE tag");
    }

    while &gb.tag() != b"fmt " {
        let len = gb.u32_le() as i32;
        if len < 0 {
            return decode_error("shorten: no fmt chunk found");
        }
        gb.skip(len as usize);
        if gb.left() < 16 {
            return decode_error("shorten: no fmt chunk found");
        }
    }
    let len = gb.u32_le() as usize;
    if len < 16 {
        return decode_error("shorten: fmt chunk was too short");
    }
    let fmt_end = gb.pos + len;

    // WAVE_FORMAT_PCM only.
    if gb.u16_le() != 1 {
        return unsupported_error("shorten: unsupported wave format");
    }
    gb.skip(2); // Channels: already known from the Shorten parameters.
    let sample_rate = gb.u32_le();
    gb.skip(4); // Byte rate of the original data.
    gb.skip(2); // Block align.
    let bits = gb.u16_le();
    if bits != 8 && bits != 16 {
        return unsupported_error("shorten: unsupported number of bits per sample");
    }
    if sample_rate == 0 {
        return decode_error("shorten: invalid sample rate");
    }

    // Not needed to decode: the announced size of the audio, found in the chunk after `fmt `.
    let mut gb = Bytes::new(header);
    gb.skip(fmt_end + (len & 1));
    let mut data_bytes = None;
    while gb.left() >= 8 {
        let id = gb.tag();
        let len = gb.u32_le() as usize;
        if &id == b"data" {
            data_bytes = plausible_size(len as u64);
            break;
        }
        gb.skip(len + (len & 1));
    }

    Ok(SourceFormat { sample_rate, bits_per_sample: bits, swap: false, data_bytes })
}

fn parse_aiff_header(header: &[u8]) -> Result<SourceFormat> {
    let mut gb = Bytes::new(header);

    // Already checked: 'FORM'.
    gb.skip(4);
    gb.skip(4); // Chunk size.
    let form = gb.tag();
    if &form != b"AIFF" && &form != b"AIFC" {
        return decode_error("shorten: missing AIFF tag");
    }

    while &gb.tag() != b"COMM" {
        let len = gb.u32_be() as i32;
        if len < 0 || (gb.left() as u64) < 18 + len as u64 + (len & 1) as u64 {
            return decode_error("shorten: no COMM chunk found");
        }
        gb.skip(len as usize + (len & 1) as usize);
    }
    let len = gb.u32_be() as i32;
    if len < 18 {
        return decode_error("shorten: COMM chunk was too short");
    }
    let comm_end = gb.pos + len as usize;

    gb.skip(6);
    let bits = gb.u16_be();
    if bits != 8 && bits != 16 {
        return unsupported_error("shorten: unsupported number of bits per sample");
    }

    // The sample rate is an 80-bit IEEE 754 extended precision number.
    let exp = i32::from(gb.u16_be()) - 16383 - 63;
    let val = gb.u64_be();
    if !(-63..=63).contains(&exp) {
        return decode_error("shorten: invalid sample rate exponent");
    }
    let rate = if exp >= 0 { val << exp } else { (val + (1u64 << (-exp - 1))) >> -exp };
    let sample_rate = match u32::try_from(rate) {
        Ok(rate) if rate != 0 && rate <= i32::MAX as u32 => rate,
        _ => return decode_error("shorten: invalid sample rate"),
    };

    // Not needed to decode: the announced size of the audio (the SSND chunk starts with an
    // 8 byte offset/block size header).
    let mut gb = Bytes::new(header);
    gb.skip(comm_end + (len & 1) as usize);
    let mut data_bytes = None;
    while gb.left() >= 8 {
        let id = gb.tag();
        let len = u64::from(gb.u32_be());
        if &id == b"SSND" {
            data_bytes = len.checked_sub(8).and_then(plausible_size);
            break;
        }
        gb.skip((len + (len & 1)).min(usize::MAX as u64) as usize);
    }

    Ok(SourceFormat { sample_rate, bits_per_sample: bits, swap: &form == b"AIFC", data_bytes })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn wave_header(rate: u32, bits: u16, data_len: u32) -> Vec<u8> {
        let mut h = Vec::new();
        h.extend_from_slice(b"RIFF");
        h.extend_from_slice(&0u32.to_le_bytes());
        h.extend_from_slice(b"WAVEfmt ");
        h.extend_from_slice(&16u32.to_le_bytes());
        h.extend_from_slice(&1u16.to_le_bytes());
        h.extend_from_slice(&2u16.to_le_bytes());
        h.extend_from_slice(&rate.to_le_bytes());
        h.extend_from_slice(&(rate * 4).to_le_bytes());
        h.extend_from_slice(&4u16.to_le_bytes());
        h.extend_from_slice(&bits.to_le_bytes());
        h.extend_from_slice(b"data");
        h.extend_from_slice(&data_len.to_le_bytes());
        h
    }

    #[test]
    fn wave_header_fields() {
        let h = wave_header(44100, 16, 1000);
        assert_eq!(h.len(), CANONICAL_HEADER_SIZE);
        let f = parse_source_header(&h).unwrap();
        assert_eq!(
            f,
            SourceFormat {
                sample_rate: 44100,
                bits_per_sample: 16,
                swap: false,
                data_bytes: Some(1000)
            }
        );
        assert_eq!(parse_source_header(&wave_header(8000, 8, u32::MAX)).unwrap().data_bytes, None);
        assert!(parse_source_header(&wave_header(8000, 24, 1)).is_err());
        assert!(parse_source_header(&h[..30]).is_err());
    }

    #[test]
    fn aiff_header_fields() {
        let mut h = Vec::new();
        h.extend_from_slice(b"FORM");
        h.extend_from_slice(&0u32.to_be_bytes());
        h.extend_from_slice(b"AIFFCOMM");
        h.extend_from_slice(&18u32.to_be_bytes());
        h.extend_from_slice(&2u16.to_be_bytes());
        h.extend_from_slice(&0u32.to_be_bytes());
        h.extend_from_slice(&16u16.to_be_bytes());
        // 44100 Hz as an 80-bit extended float: exponent 0x400e, mantissa 44100 << 48.
        h.extend_from_slice(&0x400eu16.to_be_bytes());
        h.extend_from_slice(&(44100u64 << 48).to_be_bytes());
        h.extend_from_slice(b"SSND");
        h.extend_from_slice(&(8 + 400u32).to_be_bytes());
        h.extend_from_slice(&[0; 8]);
        let f = parse_source_header(&h).unwrap();
        assert_eq!(f.sample_rate, 44100);
        assert_eq!(f.bits_per_sample, 16);
        assert_eq!(f.data_bytes, Some(400));
        assert!(!f.swap);
    }

    #[test]
    fn extra_data_round_trip() {
        let h = Header {
            version: 3,
            file_type: FileType::U8,
            channels: 2,
            blocksize: 256,
            maxnlpc: 0,
            nmean: 4,
        };
        assert_eq!(Header::from_extra_data(&h.to_extra_data(true)).unwrap(), (h, true));
        assert!(Header::from_extra_data(&[0; 3]).is_err());
    }
}
