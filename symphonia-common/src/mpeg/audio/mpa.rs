// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! MPEG-1/2/2.5 audio (layers 1, 2, and 3) frame header parsing, and a streaming frame
//! synchroniser for containers that do not preserve frame boundaries (e.g., MPEG-PS and MPEG-TS).

use symphonia_core::audio::{Channels, Position};
use symphonia_core::codecs::audio::AudioCodecId;
use symphonia_core::codecs::audio::well_known::{CODEC_ID_MP1, CODEC_ID_MP2, CODEC_ID_MP3};

/// The length of an MPEG audio frame header.
pub const MPA_HEADER_LEN: usize = 4;

/// The number of frames before a seek target that must be decoded for the layer 1 and 2 decoders
/// (and the layer 3 synthesis filterbank and overlap) to reproduce a continuous decode.
pub const MPA_SEEK_PREROLL_FRAMES: u64 = 3;

/// The number of frames before a seek target that must be decoded, in addition to
/// [`MPA_SEEK_PREROLL_FRAMES`], for the bit reservoir of a layer 3 decoder to be primed. The
/// reservoir holds at most 511 bytes, which spans at most 8 frames of the smallest practical
/// (32 kbps) streams.
pub const MPA_L3_RESERVOIR_PREROLL_FRAMES: u64 = 8;

/// The maximum length of an MPEG audio frame, including the header.
pub const MPA_MAX_FRAME_LEN: usize = 2881;

/// The information about an MPEG audio frame that can be determined from its header.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct MpaFrameInfo {
    /// The codec ID: MP1, MP2, or MP3.
    pub codec: AudioCodecId,
    /// The MPEG audio layer: 1, 2, or 3.
    pub layer: u8,
    /// True if the frame is MPEG-1 audio, false if MPEG-2 or MPEG-2.5.
    pub is_mpeg1: bool,
    /// The sample rate in Hz.
    pub sample_rate: u32,
    /// The number of channels: 1 or 2.
    pub num_channels: u8,
    /// The bit-rate in bits per second.
    pub bitrate: u32,
    /// The number of samples per channel in the frame.
    pub samples_per_frame: u32,
    /// The length of the frame in bytes, including the header.
    pub frame_len: usize,
    /// True if the header is followed by a CRC.
    pub has_crc: bool,
}

impl MpaFrameInfo {
    /// The channels of the frame.
    pub fn channels(&self) -> Channels {
        if self.num_channels == 1 {
            Channels::Positioned(Position::FRONT_LEFT)
        }
        else {
            Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT)
        }
    }

    /// Returns true if `other` may be the next frame of the same stream.
    pub fn is_compatible(&self, other: &MpaFrameInfo) -> bool {
        self.layer == other.layer
            && self.is_mpeg1 == other.is_mpeg1
            && self.sample_rate == other.sample_rate
    }

    /// The number of frames before a seek target to start decoding from.
    pub fn seek_preroll_frames(&self) -> u64 {
        if self.layer == 3 {
            MPA_SEEK_PREROLL_FRAMES + MPA_L3_RESERVOIR_PREROLL_FRAMES
        }
        else {
            MPA_SEEK_PREROLL_FRAMES
        }
    }

    /// The length of the side information of a layer 3 frame.
    fn l3_side_info_len(&self) -> usize {
        match (self.is_mpeg1, self.num_channels) {
            (true, 1) => 17,
            (true, _) => 32,
            (false, 1) => 9,
            (false, _) => 17,
        }
    }

    /// Returns true if `frame`, a complete frame with this header, is a Xing/Info (or VBRI)
    /// metadata frame that does not contain audio.
    pub fn is_info_frame(&self, frame: &[u8]) -> bool {
        if self.layer != 3 {
            return false;
        }

        let crc = if self.has_crc { 2 } else { 0 };
        let xing = MPA_HEADER_LEN + crc + self.l3_side_info_len();

        if let Some(tag) = frame.get(xing..xing + 4) {
            if tag == b"Xing" || tag == b"Info" {
                return true;
            }
        }

        // VBRI is always 32 bytes after the header.
        matches!(frame.get(MPA_HEADER_LEN + 32..MPA_HEADER_LEN + 36), Some(b"VBRI"))
    }
}

/// Parse an MPEG audio frame header word. Returns `None` if the word is not a valid header, or the
/// frame uses the "free" bit-rate (which cannot be framed without a lookahead).
pub fn parse_header(word: u32) -> Option<MpaFrameInfo> {
    const BIT_RATES_MPEG1_L1: [u32; 15] =
        [0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448];
    const BIT_RATES_MPEG1_L2: [u32; 15] =
        [0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384];
    const BIT_RATES_MPEG1_L3: [u32; 15] =
        [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320];
    const BIT_RATES_MPEG2_L1: [u32; 15] =
        [0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256];
    const BIT_RATES_MPEG2_L23: [u32; 15] =
        [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];

    if word & 0xffe0_0000 != 0xffe0_0000 {
        return None;
    }

    // Version: 0b00 = MPEG-2.5, 0b10 = MPEG-2, 0b11 = MPEG-1.
    let version = (word >> 19) & 0x3;
    let is_mpeg1 = match version {
        0b11 => true,
        0b10 | 0b00 => false,
        _ => return None,
    };

    let layer = match (word >> 17) & 0x3 {
        0b11 => 1u8,
        0b10 => 2,
        0b01 => 3,
        _ => return None,
    };

    let bitrate_idx = ((word >> 12) & 0xf) as usize;

    // The free bit-rate and the invalid bit-rate.
    if bitrate_idx == 0 || bitrate_idx == 0xf {
        return None;
    }

    let bitrate = 1000
        * match (is_mpeg1, layer) {
            (true, 1) => BIT_RATES_MPEG1_L1[bitrate_idx],
            (true, 2) => BIT_RATES_MPEG1_L2[bitrate_idx],
            (true, _) => BIT_RATES_MPEG1_L3[bitrate_idx],
            (false, 1) => BIT_RATES_MPEG2_L1[bitrate_idx],
            (false, _) => BIT_RATES_MPEG2_L23[bitrate_idx],
        };

    let sample_rate = match ((word >> 10) & 0x3, version) {
        (0b00, 0b11) => 44_100,
        (0b01, 0b11) => 48_000,
        (0b10, 0b11) => 32_000,
        (0b00, 0b10) => 22_050,
        (0b01, 0b10) => 24_000,
        (0b10, 0b10) => 16_000,
        (0b00, 0b00) => 11_025,
        (0b01, 0b00) => 12_000,
        (0b10, 0b00) => 8_000,
        _ => return None,
    };

    let is_mono = (word >> 6) & 0x3 == 0b11;

    // Some layer 2 bit-rate and channel mode combinations are not allowed.
    if layer == 2 {
        if is_mono {
            if matches!(bitrate, 224_000 | 256_000 | 320_000 | 384_000) {
                return None;
            }
        }
        else if matches!(bitrate, 32_000 | 48_000 | 56_000 | 80_000) {
            return None;
        }
    }

    let has_padding = (word >> 9) & 1 == 1;

    let (samples_per_frame, frame_len) = match (layer, is_mpeg1) {
        (1, _) => (384, (12 * bitrate / sample_rate + u32::from(has_padding)) * 4),
        (2, _) => (1152, 144 * bitrate / sample_rate + u32::from(has_padding)),
        (_, true) => (1152, 144 * bitrate / sample_rate + u32::from(has_padding)),
        (_, false) => (576, 72 * bitrate / sample_rate + u32::from(has_padding)),
    };

    let codec = match layer {
        1 => CODEC_ID_MP1,
        2 => CODEC_ID_MP2,
        _ => CODEC_ID_MP3,
    };

    Some(MpaFrameInfo {
        codec,
        layer,
        is_mpeg1,
        sample_rate,
        num_channels: if is_mono { 1 } else { 2 },
        bitrate,
        samples_per_frame,
        frame_len: frame_len as usize,
        has_crc: (word >> 16) & 1 == 0,
    })
}

/// Parse the MPEG audio frame header at the start of `buf`.
pub fn parse_header_bytes(buf: &[u8]) -> Option<MpaFrameInfo> {
    let word = u32::from_be_bytes(buf.get(..MPA_HEADER_LEN)?.try_into().ok()?);
    parse_header(word)
}

/// An MPEG audio frame found by an [`MpaFramer`].
#[derive(Clone, Debug)]
pub struct MpaFrame {
    /// The frame header information.
    pub info: MpaFrameInfo,
    /// The absolute offset, relative to the first byte pushed into the framer since it was created
    /// (or last cleared), of the first byte of the frame.
    pub start: u64,
    /// The frame, including the header.
    pub data: Vec<u8>,
}

/// An `MpaFramer` finds MPEG audio frames in a stream of bytes of arbitrary chunking.
#[derive(Default)]
pub struct MpaFramer {
    buf: Vec<u8>,
    /// The absolute offset of the first byte in `buf`.
    base: u64,
    /// The header information of the last frame returned.
    locked: Option<MpaFrameInfo>,
}

impl MpaFramer {
    /// Create a new framer.
    pub fn new() -> Self {
        Default::default()
    }

    /// Append data to the framer.
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// The absolute offset one past the last byte pushed.
    pub fn end_offset(&self) -> u64 {
        self.base + self.buf.len() as u64
    }

    /// Discard all buffered data, and forget the synchronisation state. The absolute offset
    /// continues.
    pub fn clear(&mut self) {
        self.base += self.buf.len() as u64;
        self.buf.clear();
        self.locked = None;
    }

    /// Get the next complete frame, if any. If `flush` is true, no more data will be pushed, so a
    /// frame at the very end of the buffered data is returned even though the header of a
    /// following frame cannot be checked.
    pub fn next_frame(&mut self, flush: bool) -> Option<MpaFrame> {
        let mut i = 0;

        let found = loop {
            // Find the next sync word.
            while i + 1 < self.buf.len() && !(self.buf[i] == 0xff && self.buf[i + 1] & 0xe0 == 0xe0)
            {
                i += 1;
            }

            if i + MPA_HEADER_LEN > self.buf.len() {
                break None;
            }

            let Some(info) = parse_header_bytes(&self.buf[i..])
            else {
                i += 1;
                continue;
            };

            if self.locked.is_some_and(|l| !l.is_compatible(&info)) {
                i += 1;
                continue;
            }

            let end = i + info.frame_len;

            // Wait for the entire frame. If no more data will arrive, it is not a frame.
            if end > self.buf.len() {
                if flush {
                    i += 1;
                    continue;
                }
                break None;
            }

            // Check the frame is followed by another, or the end of the stream.
            if end + MPA_HEADER_LEN <= self.buf.len() {
                match parse_header_bytes(&self.buf[end..]) {
                    Some(next) if next.is_compatible(&info) => (),
                    _ => {
                        i += 1;
                        continue;
                    }
                }
            }
            else if !flush {
                // Not enough data to check the next header.
                break None;
            }

            break Some((i, info, end));
        };

        match found {
            Some((i, info, end)) => {
                let data = self.buf[i..end].to_vec();
                let start = self.base + i as u64;
                self.buf.drain(..end);
                self.base += end as u64;
                self.locked = Some(info);
                Some(MpaFrame { info, start, data })
            }
            None => {
                // Discard the bytes before the candidate frame.
                self.buf.drain(..i);
                self.base += i as u64;
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(len_pad: bool) -> Vec<u8> {
        // MPEG-1 layer 2, 128 kbps, 44.1 kHz, stereo: 417 or 418 bytes.
        let word: u32 = 0xffe0_0000
            | (0b11 << 19)
            | (0b10 << 17)
            | (1 << 16)
            | (8 << 12)
            | (u32::from(len_pad) << 9);
        let info = parse_header(word).unwrap();
        let mut f = vec![0u8; info.frame_len];
        f[..4].copy_from_slice(&word.to_be_bytes());
        f
    }

    #[test]
    fn verify_header() {
        let info = parse_header_bytes(&frame(false)).unwrap();
        assert_eq!(info.codec, CODEC_ID_MP2);
        assert_eq!(info.sample_rate, 44_100);
        assert_eq!(info.bitrate, 128_000);
        assert_eq!(info.samples_per_frame, 1152);
        assert_eq!(info.frame_len, 417);
        assert_eq!(info.num_channels, 2);
        assert_eq!(parse_header_bytes(&frame(true)).unwrap().frame_len, 418);
        assert!(parse_header(0xfffe_0000).is_none());
        assert!(parse_header(0x0000_0000).is_none());
    }

    #[test]
    fn verify_framer_split_arbitrarily() {
        let mut stream = vec![0x12, 0x34, 0xff];
        let mut starts = vec![];
        for i in 0..10 {
            starts.push(stream.len() as u64);
            stream.extend(frame(i % 3 == 0));
        }

        for chunk in [1usize, 7, 100, 417, 1000] {
            let mut framer = MpaFramer::new();
            let mut got = vec![];

            for c in stream.chunks(chunk) {
                framer.push(c);
                while let Some(f) = framer.next_frame(false) {
                    got.push(f.start);
                }
            }

            while let Some(f) = framer.next_frame(true) {
                got.push(f.start);
            }

            assert_eq!(got, starts, "chunk={chunk}");
        }
    }
}
