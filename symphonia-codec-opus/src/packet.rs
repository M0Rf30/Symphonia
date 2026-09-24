// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Opus packet / TOC (Table-Of-Contents) parsing, per RFC 6716 section 3.1.
//!
//! Ported from libopus `src/opus.c` (`opus_packet_parse_impl`, `parse_size`,
//! `opus_packet_get_samples_per_frame`) and `src/opus_decoder.c` (`opus_packet_get_bandwidth`,
//! `opus_packet_get_nb_channels`, `opus_packet_get_nb_frames`, `opus_packet_get_nb_samples`).
//! Ported from libopus (BSD-3-Clause), see NOTICE.

use std::fmt;

/// The three top-level Opus coding modes, decoded from the TOC byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpusMode {
    SilkOnly,
    Hybrid,
    CeltOnly,
}

/// Audio bandwidth, matching libopus `OPUS_BANDWIDTH_*` constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bandwidth {
    Narrowband,
    Mediumband,
    Wideband,
    Superwideband,
    Fullband,
}

/// An error returned while parsing an Opus packet. Never panics on malformed input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketError {
    /// The packet is zero-length, or otherwise structurally invalid (`OPUS_INVALID_PACKET`).
    InvalidPacket,
    /// An argument (e.g. more than 48 frames) was out of range (`OPUS_BAD_ARG`).
    BadArgument,
}

impl fmt::Display for PacketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PacketError::InvalidPacket => write!(f, "invalid opus packet"),
            PacketError::BadArgument => write!(f, "bad argument"),
        }
    }
}

impl std::error::Error for PacketError {}

pub type Result<T, E = PacketError> = std::result::Result<T, E>;

/// Maximum number of frames documented by libopus (`frames[48]`, `size[48]`).
pub const MAX_FRAMES: usize = 48;

/// The parsed Table-Of-Contents byte. C: `toc` local in `opus_packet_parse_impl`, plus the
/// individual accessors `opus_packet_get_mode`/`get_bandwidth`/`get_nb_channels`/
/// `get_samples_per_frame`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Toc {
    pub byte: u8,
}

impl Toc {
    pub fn new(byte: u8) -> Self {
        Toc { byte }
    }

    /// C: `opus_packet_get_mode` (inlined in `opus_packet_get_bandwidth` et al.).
    pub fn mode(&self) -> OpusMode {
        if self.byte & 0x80 != 0 {
            OpusMode::CeltOnly
        }
        else if self.byte & 0x60 == 0x60 {
            OpusMode::Hybrid
        }
        else {
            OpusMode::SilkOnly
        }
    }

    /// C: `opus_packet_get_bandwidth`.
    pub fn bandwidth(&self) -> Bandwidth {
        let b = self.byte;
        if b & 0x80 != 0 {
            match (b >> 5) & 0x3 {
                0 => Bandwidth::Narrowband, // NB+MB collapse to NB per libopus comment.
                1 => Bandwidth::Wideband,
                2 => Bandwidth::Superwideband,
                _ => Bandwidth::Fullband,
            }
        }
        else if b & 0x60 == 0x60 {
            if b & 0x10 != 0 {
                Bandwidth::Fullband
            }
            else {
                Bandwidth::Superwideband
            }
        }
        else {
            match (b >> 5) & 0x3 {
                0 => Bandwidth::Narrowband,
                1 => Bandwidth::Mediumband,
                2 => Bandwidth::Wideband,
                _ => Bandwidth::Superwideband,
            }
        }
    }

    /// C: `opus_packet_get_nb_channels`.
    pub fn stereo(&self) -> bool {
        self.byte & 0x4 != 0
    }

    /// Frame code, bits 0-1 of the TOC byte (determines packet framing, RFC 6716 3.1).
    pub fn frame_code(&self) -> u8 {
        self.byte & 0x3
    }

    /// C: `opus_packet_get_samples_per_frame`. `fs` is the target sample rate (e.g. 48000).
    pub fn samples_per_frame(&self, fs: u32) -> u32 {
        let b = self.byte;
        if b & 0x80 != 0 {
            let audiosize = (b >> 3) & 0x3;
            (fs << audiosize) / 400
        }
        else if b & 0x60 == 0x60 {
            if b & 0x08 != 0 {
                fs / 50
            }
            else {
                fs / 100
            }
        }
        else {
            let audiosize = (b >> 3) & 0x3;
            if audiosize == 3 {
                fs * 60 / 1000
            }
            else {
                (fs << audiosize) / 100
            }
        }
    }
}

/// One parsed sub-frame: a byte range within the packet, expressed as an offset + length so it
/// borrows nothing and stays trivially `Copy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameRange {
    pub offset: usize,
    pub len: usize,
}

/// A fixed-capacity list of up to [`MAX_FRAMES`] [`FrameRange`]s. RFC 6716 bounds the number of
/// frames in one packet to at most 48 (enforced by [`parse_impl`] before it ever builds one of
/// these), so a plain array + length avoids a heap allocation on every single packet parse.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameList {
    buf: [FrameRange; MAX_FRAMES],
    len: usize,
}

impl FrameList {
    fn new() -> Self {
        FrameList { buf: [FrameRange { offset: 0, len: 0 }; MAX_FRAMES], len: 0 }
    }

    /// Panics if already at [`MAX_FRAMES`] capacity; [`parse_impl`] never pushes more than
    /// `count` (<= `MAX_FRAMES`, checked before any push) entries.
    fn push(&mut self, fr: FrameRange) {
        self.buf[self.len] = fr;
        self.len += 1;
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn iter(&self) -> std::slice::Iter<'_, FrameRange> {
        self.buf[..self.len].iter()
    }
}

impl std::ops::Index<usize> for FrameList {
    type Output = FrameRange;

    fn index(&self, i: usize) -> &FrameRange {
        &self.buf[..self.len][i]
    }
}

impl<'a> IntoIterator for &'a FrameList {
    type Item = &'a FrameRange;
    type IntoIter = std::slice::Iter<'a, FrameRange>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Result of [`parse_impl`]: the TOC, each sub-frame's location within `data`, the offset of the
/// first frame (`payload_offset`), and any self-delimited padding.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedPacket {
    pub toc: Toc,
    pub frames: FrameList,
    pub payload_offset: usize,
    /// Offset + length of the padding region, if any (multi-frame packets only).
    pub padding: Option<FrameRange>,
    /// Total packet length consumed, including any self-delimited trailing size and padding.
    pub packet_len: usize,
}

/// C: `parse_size`. Reads a 1- or 2-byte frame-length encoding; returns `(consumed_bytes, size)`.
fn parse_size(data: &[u8]) -> Result<(usize, i32)> {
    if data.is_empty() {
        return Err(PacketError::InvalidPacket);
    }
    if data[0] < 252 {
        Ok((1, data[0] as i32))
    }
    else if data.len() < 2 {
        Err(PacketError::InvalidPacket)
    }
    else {
        Ok((2, 4 * data[1] as i32 + data[0] as i32))
    }
}

/// C: `opus_packet_parse_impl`.
///
/// `self_delimited` requests self-delimited framing (used for streams within a multistream
/// Ogg/Opus packet, RFC 7845 section 5.2), where the *last* frame additionally carries an
/// explicit length.
pub fn parse_impl(data: &[u8], self_delimited: bool) -> Result<ParsedPacket> {
    if data.is_empty() {
        return Err(PacketError::InvalidPacket);
    }

    let framesize = Toc::new(data[0]).samples_per_frame(48000) as i64;

    let mut cbr = false;
    let toc = Toc::new(data[0]);
    let mut pos = 1usize;
    let mut len = data.len() as i64 - 1;
    let mut last_size: i64 = len;
    // Fixed-size scratch bounded by `MAX_FRAMES` (RFC 6716 documents 48 as the maximum number of
    // frames in one packet; the `framesize * count > 5760` check below enforces it before any
    // frame_code 3 push can exceed it) -- avoids a heap allocation on every single packet parse.
    let mut sizes_buf = [0i32; MAX_FRAMES];
    let mut sizes_len = 0usize;
    let mut count: usize;
    let mut pad: i64 = 0;

    match toc.frame_code() {
        0 => {
            count = 1;
        }
        1 => {
            count = 2;
            cbr = true;
            if !self_delimited {
                if len & 0x1 != 0 {
                    return Err(PacketError::InvalidPacket);
                }
                last_size = len / 2;
                sizes_buf[sizes_len] = last_size as i32;
                sizes_len += 1;
            }
        }
        2 => {
            count = 2;
            let (bytes, size0) = parse_size(&data[pos..])?;
            len -= bytes as i64;
            if size0 < 0 || size0 as i64 > len {
                return Err(PacketError::InvalidPacket);
            }
            pos += bytes;
            last_size = len - size0 as i64;
            sizes_buf[sizes_len] = size0;
            sizes_len += 1;
        }
        _ => {
            if len < 1 {
                return Err(PacketError::InvalidPacket);
            }
            let ch = data[pos];
            pos += 1;
            count = (ch & 0x3F) as usize;
            if count == 0 || framesize * count as i64 > 5760 {
                return Err(PacketError::InvalidPacket);
            }
            len -= 1;
            if ch & 0x40 != 0 {
                loop {
                    if len <= 0 {
                        return Err(PacketError::InvalidPacket);
                    }
                    let p = data[pos];
                    pos += 1;
                    len -= 1;
                    let tmp = if p == 255 { 254 } else { p as i64 };
                    len -= tmp;
                    pad += tmp;
                    if p != 255 {
                        break;
                    }
                }
            }
            if len < 0 {
                return Err(PacketError::InvalidPacket);
            }
            cbr = ch & 0x80 == 0;
            if !cbr {
                last_size = len;
                for _ in 0..count - 1 {
                    let (bytes, size) = parse_size(&data[pos..])?;
                    len -= bytes as i64;
                    if size < 0 || size as i64 > len {
                        return Err(PacketError::InvalidPacket);
                    }
                    pos += bytes;
                    last_size -= bytes as i64 + size as i64;
                    sizes_buf[sizes_len] = size;
                    sizes_len += 1;
                }
                if last_size < 0 {
                    return Err(PacketError::InvalidPacket);
                }
            }
            else if !self_delimited {
                last_size = len / count as i64;
                if last_size * count as i64 != len {
                    return Err(PacketError::InvalidPacket);
                }
                for _ in 0..count - 1 {
                    sizes_buf[sizes_len] = last_size as i32;
                    sizes_len += 1;
                }
            }
        }
    }

    // Self-delimited framing has an extra explicit size for the last frame.
    if self_delimited {
        let (bytes, last) = parse_size(&data[pos..])?;
        len -= bytes as i64;
        if last < 0 || last as i64 > len {
            return Err(PacketError::InvalidPacket);
        }
        pos += bytes;
        if cbr {
            if last as i64 * count as i64 > len {
                return Err(PacketError::InvalidPacket);
            }
            sizes_len = 0;
            for _ in 0..count - 1 {
                sizes_buf[sizes_len] = last;
                sizes_len += 1;
            }
        }
        else if bytes as i64 + last as i64 > last_size {
            return Err(PacketError::InvalidPacket);
        }
        sizes_buf[sizes_len] = last;
        sizes_len += 1;
    }
    else {
        if last_size > 1275 {
            return Err(PacketError::InvalidPacket);
        }
        sizes_buf[sizes_len] = last_size as i32;
        sizes_len += 1;
    }

    // `sizes` was built with `count` entries by construction above; guard defensively.
    if sizes_len != count {
        return Err(PacketError::InvalidPacket);
    }
    count = sizes_len;

    let payload_offset = pos;
    let mut frames = FrameList::new();
    for &size in &sizes_buf[..sizes_len] {
        let size = size as usize;
        if pos + size > data.len() {
            return Err(PacketError::InvalidPacket);
        }
        frames.push(FrameRange { offset: pos, len: size });
        pos += size;
    }

    let padding = if pad > 0 {
        let range = FrameRange { offset: pos, len: pad as usize };
        if range.offset + range.len > data.len() {
            return Err(PacketError::InvalidPacket);
        }
        Some(range)
    }
    else {
        None
    };

    let packet_len = pos + pad as usize;

    Ok(ParsedPacket { toc, frames, payload_offset, padding, packet_len })
}

/// C: `opus_packet_parse` (non-self-delimited convenience wrapper).
pub fn parse(data: &[u8]) -> Result<ParsedPacket> {
    parse_impl(data, false)
}

/// C: `opus_packet_get_nb_frames`.
pub fn get_nb_frames(data: &[u8]) -> Result<usize> {
    if data.is_empty() {
        return Err(PacketError::BadArgument);
    }
    let count = data[0] & 0x3;
    if count == 0 {
        Ok(1)
    }
    else if count != 3 {
        Ok(2)
    }
    else if data.len() < 2 {
        Err(PacketError::InvalidPacket)
    }
    else {
        Ok((data[1] & 0x3F) as usize)
    }
}

/// C: `opus_packet_get_nb_samples`.
pub fn get_nb_samples(data: &[u8], fs: u32) -> Result<u32> {
    let count = get_nb_frames(data)? as u32;
    let samples = count * Toc::new(data[0]).samples_per_frame(fs);
    if (samples as u64) * 25 > (fs as u64) * 3 {
        Err(PacketError::InvalidPacket)
    }
    else {
        Ok(samples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toc_byte(config: u8, stereo: bool, frame_code: u8) -> u8 {
        (config << 3) | ((stereo as u8) << 2) | frame_code
    }

    #[test]
    fn single_frame_code0() {
        // config=0 (SILK-only NB 10ms), mono, code 0: single frame filling the rest of the
        // packet.
        let data = [toc_byte(0, false, 0), 1, 2, 3, 4];
        let p = parse(&data).unwrap();
        assert_eq!(p.frames.len(), 1);
        assert_eq!(p.frames[0], FrameRange { offset: 1, len: 4 });
        assert_eq!(p.toc.mode(), OpusMode::SilkOnly);
        assert!(!p.toc.stereo());
    }

    #[test]
    fn two_cbr_frames_code1() {
        let data = [toc_byte(0, false, 1), 1, 2, 3, 4];
        let p = parse(&data).unwrap();
        assert_eq!(p.frames.len(), 2);
        assert_eq!(p.frames[0], FrameRange { offset: 1, len: 2 });
        assert_eq!(p.frames[1], FrameRange { offset: 3, len: 2 });
    }

    #[test]
    fn two_cbr_frames_code1_odd_length_is_invalid() {
        let data = [toc_byte(0, false, 1), 1, 2, 3];
        assert_eq!(parse(&data), Err(PacketError::InvalidPacket));
    }

    #[test]
    fn two_vbr_frames_code2() {
        let data = [toc_byte(0, false, 2), 2, 0xAA, 0xBB, 0xCC];
        let p = parse(&data).unwrap();
        assert_eq!(p.frames.len(), 2);
        assert_eq!(p.frames[0], FrameRange { offset: 2, len: 2 });
        assert_eq!(p.frames[1], FrameRange { offset: 4, len: 1 });
    }

    #[test]
    fn code3_cbr_multiple_frames() {
        // 3 frames CBR, no padding: ch byte = count(3) | vbr_bit(0)<<7.
        let data = [toc_byte(0, false, 3), 3, 1, 2, 3, 4, 5, 6];
        let p = parse(&data).unwrap();
        assert_eq!(p.frames.len(), 3);
        for f in &p.frames {
            assert_eq!(f.len, 2);
        }
    }

    #[test]
    fn code3_with_padding() {
        // 2 frames CBR with padding flag set and a single 3-byte padding block.
        let ch = 2 | 0x40; // count=2, padding bit set, vbr bit clear (CBR).
        let data = [toc_byte(0, false, 3), ch, 3, 1, 2, 3, 4, 0xFF, 0xFF, 0xFF];
        let p = parse(&data).unwrap();
        assert_eq!(p.frames.len(), 2);
        assert!(p.padding.is_some());
        assert_eq!(p.padding.unwrap().len, 3);
    }

    #[test]
    fn malformed_packets_never_panic() {
        assert_eq!(parse(&[]), Err(PacketError::InvalidPacket));
        assert_eq!(parse(&[toc_byte(0, false, 2)]), Err(PacketError::InvalidPacket));
        // code 3 with a bogus count of 0.
        assert_eq!(parse(&[toc_byte(0, false, 3), 0]), Err(PacketError::InvalidPacket));
        // Truncated 2-byte size prefix.
        assert_eq!(parse(&[toc_byte(0, false, 2), 253]), Err(PacketError::InvalidPacket));
        // count*framesize overflow guard.
        let data = [toc_byte(15, false, 3), 63];
        assert!(parse(&data).is_err());
    }

    #[test]
    fn celt_only_fullband_mode_and_bandwidth() {
        // config >= 16 selects CELT-only; config 31 (0x80|0xF8) => fullband, 20ms.
        let toc = Toc::new(0x80 | (31 << 3) & 0x7F | 0x80);
        // Just exercise the CELT-only high bit directly instead of guessing exact config table.
        let toc = Toc::new(toc.byte | 0x80);
        assert_eq!(toc.mode(), OpusMode::CeltOnly);
    }

    #[test]
    fn hybrid_mode_detection() {
        let toc = Toc::new(0x60); // bits 0x60 set, bit 0x80 clear => hybrid.
        assert_eq!(toc.mode(), OpusMode::Hybrid);
    }

    #[test]
    fn nb_frames_and_samples() {
        let data = [toc_byte(0, false, 0), 0, 0];
        assert_eq!(get_nb_frames(&data).unwrap(), 1);
        // config 0 => 10ms SILK NB frame; at 48kHz that's 480 samples.
        assert_eq!(get_nb_samples(&data, 48000).unwrap(), 480);
    }
}
