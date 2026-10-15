// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! SMPTE ST 337 ("S337M") data bursts carrying Dolby E, and an elementary stream parser of them.
//!
//! A Dolby E frame is carried in the PCM of a pair of channels as a burst. The burst starts with
//! a preamble of four words, `Pa`, `Pb` (the sync words), `Pc` (the data type), and `Pd` (the
//! length of the payload in bits), followed by the payload words. Each word is the size of a PCM
//! sample (16, 20, or 24 bits), stored little-endian (in 2 bytes for 16 bits, and in 3 bytes
//! otherwise, with the 20-bit words in the upper bits of their 3 bytes). The words of the pair
//! of channels are interleaved, and the payload is the words in order of time and channel.
//!
//! The framing follows FFmpeg's `libavformat/s337m.c` (LGPL-2.1-or-later, Copyright (c) 2017
//! foo86): only the Dolby E data type (28) is carried; the payload size selects the burst
//! spacing, i.e., the number of stereo samples of a video frame, and a packet is the words that
//! follow the preamble up to 4 stereo samples before the next burst.

use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::codecs::audio::well_known::CODEC_ID_DOLBY_E;

use super::dolby_e::{DolbyEHeader, FRAME_SAMPLES, WordStream, channels};
use super::es::{EsFrame, EsInfo, EsParser};
use super::sync_buf::SyncBuffer;

/// The data type of Dolby E.
const DATA_TYPE_DOLBY_E: u32 = 0x1c;

/// `Pa` and `Pb` in the 16-bit, 20-bit, and 24-bit forms (little-endian).
const MARKER_16: [u8; 4] = [0x72, 0xf8, 0x1f, 0x4e];
const MARKER_20: [u8; 6] = [0x20, 0x87, 0x6f, 0xf0, 0xe1, 0x54];
const MARKER_24: [u8; 6] = [0x72, 0xf8, 0x96, 0x1f, 0x4e, 0xa5];

/// The preamble of a burst.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Preamble {
    /// The size of a word in bits: 16, 20, or 24.
    pub word_bits: u32,
    /// The length in bytes of the preamble.
    pub len: usize,
    /// The length in bytes of the payload that follows (a packet).
    pub payload_len: usize,
}

/// Returns the position and the size in bits of the words of the first marker (`Pa` and `Pb`) in
/// `data`.
pub fn find_marker(data: &[u8]) -> Option<(usize, u32)> {
    // The marker that ends first is the one found, as FFmpeg's byte-wise search does.
    for end in 3..data.len() {
        if data[end - 3..=end] == MARKER_16 {
            return Some((end - 3, 16));
        }

        if end >= 5 {
            let w = &data[end - 5..=end];

            // The low nibbles of 20-bit words are padding.
            if w[0] & 0xf0 == MARKER_20[0]
                && w[1] == MARKER_20[1]
                && w[2] == MARKER_20[2]
                && w[3] & 0xf0 == MARKER_20[3]
                && w[4] == MARKER_20[4]
                && w[5] == MARKER_20[5]
            {
                return Some((end - 5, 20));
            }

            if *w == MARKER_24 {
                return Some((end - 5, 24));
            }
        }
    }

    None
}

/// The data type and size in bits of the payload of the burst with the marker (of words of
/// `word_bits`) at the start of `data`, or `None` if `data` is too short.
fn read_pc_pd(data: &[u8], word_bits: u32) -> Option<(u32, u32, usize)> {
    if word_bits == 16 {
        let d = data.get(4..8)?;
        let pc = u32::from(u16::from_le_bytes([d[0], d[1]]));
        let pd = u32::from(u16::from_le_bytes([d[2], d[3]]));
        Some((pc, pd, 8))
    }
    else {
        let d = data.get(6..12)?;
        let pc = u32::from_le_bytes([d[0], d[1], d[2], 0]) >> 8;
        let pd = u32::from_le_bytes([d[3], d[4], d[5], 0]);
        Some((pc, if word_bits == 20 { pd >> 4 } else { pd }, 12))
    }
}

/// The length in bytes of the packet of a Dolby E burst with a payload of `data_size` bits.
fn packet_len(word_bits: u32, data_size: u32) -> Option<usize> {
    // The number of stereo samples of the burst spacing.
    let spacing = match data_size / word_bits {
        3648 => 1920,
        3644 => 2002,
        3640 => 2000,
        3040 => 1601,
        _ => return None,
    };

    let word_bytes = (word_bits as usize + 7) >> 3;
    Some((spacing - 4) * word_bytes * 2)
}

/// The result of parsing the preamble of a burst.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BurstPreamble {
    /// More data is required.
    Incomplete,
    /// The burst does not carry a Dolby E frame of a known size.
    Unsupported,
    /// The preamble of a Dolby E burst.
    DolbyE(Preamble),
}

impl Preamble {
    /// Parse the preamble of the burst at the start of `data`, which starts with the marker of
    /// words of `word_bits`.
    pub fn parse(data: &[u8], word_bits: u32) -> BurstPreamble {
        let Some((pc, pd, len)) = read_pc_pd(data, word_bits)
        else {
            return BurstPreamble::Incomplete;
        };

        if pc & 0x1f != DATA_TYPE_DOLBY_E {
            return BurstPreamble::Unsupported;
        }

        match packet_len(word_bits, pd) {
            Some(payload_len) => BurstPreamble::DolbyE(Preamble { word_bits, len, payload_len }),
            None => BurstPreamble::Unsupported,
        }
    }
}

/// Convert the little-endian words of a payload to the big-endian words of a Dolby E frame.
pub fn swap_words(data: &mut [u8], word_bits: u32) {
    if word_bits == 16 {
        for pair in data.chunks_exact_mut(2) {
            pair.swap(0, 1);
        }
    }
    else {
        for triplet in data.chunks_exact_mut(3) {
            triplet.swap(0, 2);
        }
    }
}

/// A parser of the Dolby E bursts of a SMPTE ST 337 stream. A frame is the payload of a burst,
/// as the words of a Dolby E frame (big-endian), and starts at the burst preamble.
#[derive(Default)]
pub struct S337mEs {
    buf: SyncBuffer,
    info: Option<EsInfo>,
}

impl S337mEs {
    pub fn new() -> Self {
        Default::default()
    }
}

impl EsParser for S337mEs {
    fn push(&mut self, data: &[u8]) {
        self.buf.push(data);
    }

    fn next_frame(&mut self, flush: bool) -> Option<EsFrame> {
        loop {
            let Some((pos, word_bits)) = find_marker(&self.buf[..])
            else {
                // Keep the end of the data, which may be the start of a marker.
                let keep = self.buf.len().min(5);
                self.buf.consume(self.buf.len() - keep);

                if flush {
                    self.buf.clear();
                }
                return None;
            };

            self.buf.consume(pos);

            let pre = match Preamble::parse(&self.buf[..], word_bits) {
                BurstPreamble::DolbyE(pre) => pre,
                BurstPreamble::Incomplete => {
                    if flush {
                        self.buf.clear();
                    }
                    return None;
                }
                BurstPreamble::Unsupported => {
                    // Not a Dolby E burst of a known size. Search for the next marker.
                    self.buf.consume(1);
                    continue;
                }
            };

            let end = pre.len + pre.payload_len;

            if self.buf.len() < end {
                if flush {
                    self.buf.clear();
                }
                return None;
            }

            let start = self.buf.base();
            let mut data: Box<[u8]> = self.buf[pre.len..end].into();
            self.buf.consume(end);

            swap_words(&mut data, word_bits);

            if self.info.is_none() {
                let mut words = WordStream::new();

                if let Ok(header) = DolbyEHeader::parse(&mut words, &data) {
                    let mut params = AudioCodecParameters::new();
                    params
                        .for_codec(CODEC_ID_DOLBY_E)
                        .with_sample_rate(header.sample_rate)
                        .with_channels(channels(header.nb_channels));

                    self.info = Some(EsInfo { params, rate: header.sample_rate });
                }
            }

            return Some(EsFrame {
                start,
                data,
                dur: FRAME_SAMPLES as u64,
                trim_start: 0,
                trim_end: 0,
            });
        }
    }

    fn clear(&mut self) {
        self.buf.clear();
    }

    fn info(&self) -> Option<&EsInfo> {
        self.info.as_ref()
    }

    fn seek_start_ts(&self, target: i64) -> i64 {
        // The overlap of the transform (256 samples) is the only state carried over to the next
        // frame, but frames are not split.
        target - FRAME_SAMPLES as i64
    }

    fn fresh(&self) -> Box<dyn EsParser> {
        Box::new(S337mEs::new())
    }

    fn frame_grid(&self) -> u64 {
        FRAME_SAMPLES as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_marker_search() {
        let mut data = vec![0u8; 5];
        data.extend_from_slice(&MARKER_16);
        assert_eq!(find_marker(&data), Some((5, 16)));

        let mut data = vec![1u8; 3];
        data.extend_from_slice(&[0x2f, 0x87, 0x6f, 0xf5, 0xe1, 0x54]);
        assert_eq!(find_marker(&data), Some((3, 20)));

        let mut data = vec![9u8; 7];
        data.extend_from_slice(&MARKER_24);
        assert_eq!(find_marker(&data), Some((7, 24)));

        assert_eq!(find_marker(&[0x72, 0xf8, 0x1f]), None);
    }

    #[test]
    fn verify_preamble() {
        // 16-bit, Pc = 0x1c, Pd = 3648 words of 16 bits.
        let mut data = MARKER_16.to_vec();
        data.extend_from_slice(&0x1cu16.to_le_bytes());
        data.extend_from_slice(&((3648 * 16) as u16).to_le_bytes());
        let pre = Preamble { word_bits: 16, len: 8, payload_len: (1920 - 4) * 4 };
        assert_eq!(Preamble::parse(&data, 16), BurstPreamble::DolbyE(pre));

        // Too short.
        assert_eq!(Preamble::parse(&data[..7], 16), BurstPreamble::Incomplete);

        // Another data type.
        data[4] = 0x01;
        assert_eq!(Preamble::parse(&data, 16), BurstPreamble::Unsupported);
    }
}
