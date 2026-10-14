// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Blu-ray linear PCM (LPCM), stream type 0x80 (BD-ROM Part 3, Audio Visual Basic
//! Specifications).

use std::collections::VecDeque;

use symphonia_common::mpeg::es::{EsFrame, EsInfo, EsParser};
use symphonia_core::audio::{Channels, Position};
use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::codecs::audio::well_known::{CODEC_ID_PCM_S16BE, CODEC_ID_PCM_S24BE};

/// The length of the header of an access unit: the payload size, and the audio parameters.
const HEADER_LEN: usize = 4;

/// The channel assignments, by the channel assignment code: the number of channels coded in the
/// stream (an odd number of channels is padded with one empty channel), and the position of each
/// coded channel in the order they are coded, if it is not padding.
fn channel_assignment(code: u8) -> Option<(usize, &'static [Position])> {
    use Position as P;

    Some(match code {
        // Mono: M1, X.
        1 => (2, &[P::FRONT_CENTER]),
        // L, R.
        3 => (2, &[P::FRONT_LEFT, P::FRONT_RIGHT]),
        // 3/0: L, R, C, X.
        4 => (4, &[P::FRONT_LEFT, P::FRONT_RIGHT, P::FRONT_CENTER]),
        // 2/1: L, R, S, X.
        5 => (4, &[P::FRONT_LEFT, P::FRONT_RIGHT, P::REAR_CENTER]),
        // 3/1: L, R, C, S.
        6 => (4, &[P::FRONT_LEFT, P::FRONT_RIGHT, P::FRONT_CENTER, P::REAR_CENTER]),
        // 2/2: L, R, LS, RS.
        7 => (4, &[P::FRONT_LEFT, P::FRONT_RIGHT, P::SIDE_LEFT, P::SIDE_RIGHT]),
        // 3/2: L, R, C, LS, RS, X.
        8 => (6, &[P::FRONT_LEFT, P::FRONT_RIGHT, P::FRONT_CENTER, P::SIDE_LEFT, P::SIDE_RIGHT]),
        // 3/2+LFE: L, R, C, LS, RS, LFE.
        9 => (
            6,
            &[P::FRONT_LEFT, P::FRONT_RIGHT, P::FRONT_CENTER, P::SIDE_LEFT, P::SIDE_RIGHT, P::LFE1],
        ),
        // 3/4: L, R, C, LS, Rls, Rrs, RS, X.
        10 => (
            8,
            &[
                P::FRONT_LEFT,
                P::FRONT_RIGHT,
                P::FRONT_CENTER,
                P::SIDE_LEFT,
                P::REAR_LEFT,
                P::REAR_RIGHT,
                P::SIDE_RIGHT,
            ],
        ),
        // 3/4+LFE: L, R, C, LS, Rls, Rrs, RS, LFE.
        11 => (
            8,
            &[
                P::FRONT_LEFT,
                P::FRONT_RIGHT,
                P::FRONT_CENTER,
                P::SIDE_LEFT,
                P::REAR_LEFT,
                P::REAR_RIGHT,
                P::SIDE_RIGHT,
                P::LFE1,
            ],
        ),
        _ => return None,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BdFormat {
    bits: u8,
    rate: u32,
    /// The number of channels coded.
    coded: usize,
    /// The index of the coded channel of each output channel, in order of position.
    order: Vec<usize>,
    channels: Channels,
}

impl BdFormat {
    fn parse(header: &[u8]) -> Option<BdFormat> {
        let (code, rate_code) = (header[2] >> 4, header[2] & 0xf);

        let bits = match header[3] >> 6 {
            1 => 16,
            3 => 24,
            // 20-bit samples are not used by any known stream.
            _ => return None,
        };

        let rate = match rate_code {
            1 => 48_000,
            4 => 96_000,
            5 => 192_000,
            _ => return None,
        };

        let (coded, positions) = channel_assignment(code)?;

        // Output the channels in order of their position.
        let mut order: Vec<usize> = (0..positions.len()).collect();
        order.sort_by_key(|&i| positions[i].bits());

        let channels = Channels::Positioned(positions.iter().copied().collect::<Position>());

        Some(BdFormat { bits, rate, coded, order, channels })
    }

    fn sample_bytes(&self) -> usize {
        usize::from(self.bits) / 8
    }
}

/// A Blu-ray LPCM stream parser. It is given the payload of each PES packet, which is a number of
/// access units, each with a 4 byte header, and produces one frame for each.
#[derive(Default)]
pub struct BdLpcmEs {
    pending: VecDeque<EsFrame>,
    pushed: u64,
    format: Option<BdFormat>,
    info: Option<EsInfo>,
}

impl BdLpcmEs {
    pub fn new() -> Self {
        Default::default()
    }
}

impl EsParser for BdLpcmEs {
    fn push(&mut self, data: &[u8]) {
        let base = self.pushed;
        self.pushed += data.len() as u64;

        let mut pos = 0;

        while data.len() - pos >= HEADER_LEN {
            let size = usize::from(u16::from_be_bytes([data[pos], data[pos + 1]]));
            let Some(format) = BdFormat::parse(&data[pos..pos + HEADER_LEN])
            else {
                return;
            };

            let start = base + pos as u64;
            let end = (pos + HEADER_LEN + size).min(data.len());
            let payload = &data[pos + HEADER_LEN..end];
            pos = end;

            match &self.format {
                // The format may not change.
                Some(f) if *f != format => continue,
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
                        .with_channels(format.channels.clone())
                        .with_bits_per_sample(u32::from(format.bits));

                    self.info = Some(EsInfo { params, rate: format.rate });
                    self.format = Some(format.clone());
                }
            }

            let width = format.sample_bytes();
            let frame_len = format.coded * width;
            let frames = payload.len() / frame_len;

            if frames == 0 {
                continue;
            }

            let mut out = Vec::with_capacity(frames * format.order.len() * width);

            for frame in payload.chunks_exact(frame_len).take(frames) {
                for &ch in &format.order {
                    out.extend_from_slice(&frame[ch * width..(ch + 1) * width]);
                }
            }

            self.pending.push_back(EsFrame {
                start,
                data: out.into(),
                dur: frames as u64,
                trim_start: 0,
                trim_end: 0,
            });
        }
    }

    fn next_frame(&mut self, _flush: bool) -> Option<EsFrame> {
        self.pending.pop_front()
    }

    fn clear(&mut self) {
        self.pending.clear();
    }

    fn info(&self) -> Option<&EsInfo> {
        self.info.as_ref()
    }

    fn seek_start_ts(&self, target: i64) -> i64 {
        target
    }

    fn fresh(&self) -> Box<dyn EsParser> {
        Box::new(BdLpcmEs::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn au(code: u8, rate: u8, quant: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = (payload.len() as u16).to_be_bytes().to_vec();
        v.extend_from_slice(&[(code << 4) | rate, quant << 6]);
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn verify_5_1_reordering_16_bit() {
        // Coded order: L R C LS RS LFE. Output order: L R C LFE LS RS.
        let mut es = BdLpcmEs::new();
        es.push(&au(9, 1, 1, &[0, 1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6]));
        let f = es.next_frame(false).unwrap();
        assert_eq!(f.dur, 1);
        assert_eq!(&f.data[..], &[0, 1, 0, 2, 0, 3, 0, 6, 0, 4, 0, 5]);
        assert_eq!(es.info().unwrap().params.codec, CODEC_ID_PCM_S16BE);
    }

    #[test]
    fn verify_odd_channels_drop_padding() {
        // Mono, 24-bit: M1 and the empty channel.
        let mut es = BdLpcmEs::new();
        es.push(&au(1, 4, 3, &[1, 2, 3, 0, 0, 0, 4, 5, 6, 0, 0, 0]));
        let f = es.next_frame(false).unwrap();
        assert_eq!(f.dur, 2);
        assert_eq!(&f.data[..], &[1, 2, 3, 4, 5, 6]);
        assert_eq!(es.info().unwrap().rate, 96_000);
        assert_eq!(es.info().unwrap().params.codec, CODEC_ID_PCM_S24BE);
    }

    #[test]
    fn verify_reserved_values_are_rejected() {
        let mut es = BdLpcmEs::new();
        es.push(&au(9, 2, 1, &[0; 12]));
        es.push(&au(2, 1, 1, &[0; 12]));
        es.push(&au(9, 1, 2, &[0; 12]));
        assert!(es.info().is_none());
        assert!(es.next_frame(true).is_none());
    }
}
