// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! DVD-Video linear PCM (LPCM) in private stream 1.

use std::collections::VecDeque;

use symphonia_common::mpeg::es::{EsFrame, EsInfo, EsParser};
use symphonia_core::audio::{Channels, Position};
use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::codecs::audio::well_known::{CODEC_ID_PCM_S16BE, CODEC_ID_PCM_S24BE};

/// The length of the private stream header (the sub-stream ID, the number of frame headers, and
/// the first access unit pointer), and the LPCM audio header.
pub const LPCM_HEADER_LEN: usize = 4 + 3;

/// The format of an LPCM stream, from the audio header.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct LpcmFormat {
    bits: u8,
    rate: u32,
    channels: u8,
}

impl LpcmFormat {
    fn parse(header: &[u8]) -> Option<LpcmFormat> {
        let b = header.get(5)?;

        // The quantisation word length: 16, 20, or 24 bits. 20-bit samples are not supported.
        let bits = match b >> 6 {
            0 => 16,
            2 => 24,
            _ => return None,
        };

        let rate = match (b >> 4) & 3 {
            0 => 48_000,
            1 => 96_000,
            _ => return None,
        };

        Some(LpcmFormat { bits, rate, channels: (b & 7) + 1 })
    }

    /// The number of bytes of the coded samples of all channels of one sample group (the
    /// smallest unit of samples): one sample for 16-bit, and two for 24-bit.
    fn group_len(&self) -> usize {
        match self.bits {
            16 => 2 * usize::from(self.channels),
            _ => 6 * usize::from(self.channels),
        }
    }

    fn group_samples(&self) -> usize {
        if self.bits == 16 { 1 } else { 2 }
    }

    fn channels(&self) -> Channels {
        match self.channels {
            1 => Channels::Positioned(Position::FRONT_LEFT),
            2 => Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT),
            n => Channels::Discrete(u16::from(n)),
        }
    }
}

/// A DVD-Video LPCM stream parser. It is given the payload of each PES packet of private stream 1
/// (beginning with the sub-stream ID) and produces one frame for it.
///
/// The audio of the packets is a continuous stream of samples, split in the packets at arbitrary
/// byte positions. The first access unit pointer of the first packet gives the position of the first
/// complete sample group.
///
/// 16-bit samples are big-endian, and are output unchanged. The 24-bit samples are stored in
/// groups of two samples as the 16 most-significant bits of each sample followed by the least-
/// significant 8 bits of each sample, and are re-ordered into big-endian 24-bit samples.
#[derive(Default)]
pub struct LpcmEs {
    pending: VecDeque<EsFrame>,
    pushed: u64,
    format: Option<LpcmFormat>,
    info: Option<EsInfo>,
    /// True once the position of a sample group is known.
    aligned: bool,
    /// The bytes of an incomplete sample group.
    leftover: Vec<u8>,
}

impl LpcmEs {
    pub fn new() -> Self {
        Default::default()
    }
}

impl EsParser for LpcmEs {
    fn push(&mut self, data: &[u8]) {
        let start = self.pushed;
        self.pushed += data.len() as u64;

        let Some(format) = LpcmFormat::parse(data)
        else {
            return;
        };

        if data.len() < LPCM_HEADER_LEN {
            return;
        }

        match self.format {
            // The format may not change.
            Some(f) if f != format => return,
            Some(_) => (),
            None => {
                let mut params = AudioCodecParameters::new();
                params
                    .for_codec(if format.bits == 16 {
                        CODEC_ID_PCM_S16BE
                    }
                    else {
                        CODEC_ID_PCM_S24BE
                    })
                    .with_sample_rate(format.rate)
                    .with_channels(format.channels())
                    .with_bits_per_sample(u32::from(format.bits));

                self.format = Some(format);
                self.info = Some(EsInfo { params, rate: format.rate });
            }
        }

        let mut pcm = &data[LPCM_HEADER_LEN..];

        if !self.aligned {
            // The first access unit pointer is the offset from the end of the pointer to the first
            // access unit. A pointer of 0 indicates that no access unit begins in the packet.
            let ptr = usize::from(u16::from_be_bytes([data[2], data[3]]));

            // The audio begins right after the header, which is 4 bytes after the pointer.
            let Some(skip) = ptr.checked_sub(4).filter(|&s| s <= pcm.len())
            else {
                return;
            };

            pcm = &pcm[skip..];
            self.aligned = true;
            self.leftover.clear();
        }

        let mut buf = std::mem::take(&mut self.leftover);
        buf.extend_from_slice(pcm);

        let groups = buf.len() / format.group_len();

        if groups == 0 {
            self.leftover = buf;
            return;
        }

        self.leftover = buf.split_off(groups * format.group_len());
        let pcm = &buf[..];
        let ch = usize::from(format.channels);

        let out: Box<[u8]> = if format.bits == 16 {
            pcm.into()
        }
        else {
            // The 24-bit samples.
            let mut out = Vec::with_capacity(groups * 2 * ch * 3);

            for group in pcm.chunks_exact(format.group_len()) {
                let (msb, lsb) = group.split_at(4 * ch);

                for i in 0..2 * ch {
                    out.extend_from_slice(&msb[2 * i..2 * i + 2]);
                    out.push(lsb[i]);
                }
            }

            out.into()
        };

        self.pending.push_back(EsFrame {
            start,
            data: out,
            dur: (groups * format.group_samples()) as u64,
            trim_start: 0,
            trim_end: 0,
        });
    }

    fn next_frame(&mut self, _flush: bool) -> Option<EsFrame> {
        self.pending.pop_front()
    }

    fn clear(&mut self) {
        self.pending.clear();
        self.leftover.clear();
        self.aligned = false;
    }

    fn info(&self) -> Option<&EsInfo> {
        self.info.as_ref()
    }

    fn seek_start_ts(&self, target: i64) -> i64 {
        target
    }

    fn fresh(&self) -> Box<dyn EsParser> {
        Box::new(LpcmEs::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(quant: u8, freq: u8, channels: u8) -> Vec<u8> {
        vec![0xa0, 1, 0, 4, 0, (quant << 6) | (freq << 4) | (channels - 1), 0]
    }

    #[test]
    fn verify_16_bit() {
        let mut es = LpcmEs::new();
        let mut data = header(0, 0, 2);
        data.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9]);
        es.push(&data);

        let f = es.next_frame(false).unwrap();
        assert_eq!(f.dur, 2);
        assert_eq!(&f.data[..], &[1, 2, 3, 4, 5, 6, 7, 8]);

        let info = es.info().unwrap();
        assert_eq!(info.rate, 48_000);
        assert_eq!(info.params.codec, CODEC_ID_PCM_S16BE);
    }

    #[test]
    fn verify_24_bit_reordering() {
        // Stereo: two sample groups: the 16-bit words of s0c0 s0c1 s1c0 s1c1, then the low bytes.
        let mut es = LpcmEs::new();
        let mut data = header(2, 1, 2);
        data.extend_from_slice(&[
            0x11, 0x12, 0x21, 0x22, 0x31, 0x32, 0x41, 0x42, 0xa1, 0xa2, 0xa3, 0xa4,
        ]);
        es.push(&data);

        let f = es.next_frame(false).unwrap();
        assert_eq!(f.dur, 2);
        assert_eq!(
            &f.data[..],
            &[0x11, 0x12, 0xa1, 0x21, 0x22, 0xa2, 0x31, 0x32, 0xa3, 0x41, 0x42, 0xa4]
        );

        let info = es.info().unwrap();
        assert_eq!(info.rate, 96_000);
        assert_eq!(info.params.codec, CODEC_ID_PCM_S24BE);
    }

    #[test]
    fn verify_unsupported_formats_are_ignored() {
        let mut es = LpcmEs::new();
        let mut data = header(1, 0, 2);
        data.extend_from_slice(&[0u8; 100]);
        es.push(&data);
        assert!(es.next_frame(false).is_none());
        assert!(es.info().is_none());
    }
}
