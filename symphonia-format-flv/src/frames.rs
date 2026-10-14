// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Elementary stream parsers for the codecs of the enhanced audio tags (Enhanced RTMP, "ExHeader")
//! that are not framed by their own sync words: Opus, FLAC, AC-3, and E-AC-3. Each tag has whole
//! frames, and the duration of a tag is found from the frame headers.

use std::collections::VecDeque;

use symphonia_common::mpeg::es::{
    EsFrame, EsInfo, EsParser, build_opus_head, opus_channels, opus_packet_samples,
};
use symphonia_core::audio::{Channels, Position};
use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::codecs::audio::well_known::{
    CODEC_ID_AC3, CODEC_ID_ADPCM_SWF, CODEC_ID_EAC3, CODEC_ID_FLAC, CODEC_ID_NELLYMOSER,
    CODEC_ID_OPUS, CODEC_ID_SPEEX,
};
use symphonia_core::errors::{Result, decode_error, unsupported_error};

/// The pre-roll of an Opus stream, in samples at 48 kHz (RFC 7845 §4.6: at least 80 ms).
const OPUS_SEEK_PREROLL: i64 = 80 * 48;

/// The number of samples of an AC-3 syncframe (6 audio blocks of 256 samples), and the maximum of
/// an E-AC-3 frame.
const AC3_FRAME_SAMPLES: u64 = 1536;

/// The number of frames of a block of SWF ADPCM, which is its initial sample and 4095 coded samples.
const SWF_ADPCM_BLOCK_FRAMES: u64 = 4096;

/// The length in bytes of a block of Nellymoser, and the number of frames that it decodes to.
const NELLYMOSER_BLOCK_BYTES: usize = 64;
const NELLYMOSER_BLOCK_FRAMES: u64 = 256;

/// The number of frames of a Speex frame of wideband (FLV Speex is always 16 kHz, 20 ms per tag).
const SPEEX_FRAME_FRAMES: u64 = 320;

/// The length of the STREAMINFO block of FLAC.
const FLAC_STREAMINFO_LEN: usize = 34;

#[derive(Copy, Clone, Debug)]
enum Codec {
    Opus,
    Flac { max_block: u64 },
    SwfAdpcm { channels: usize },
    Ac3,
    Eac3,
    Nellymoser,
    Speex,
}

/// A parser for tags that contain frames of Opus, FLAC, AC-3, or E-AC-3.
pub struct FramedEs {
    codec: Codec,
    info: Option<EsInfo>,
    grid: u64,
    pending: VecDeque<EsFrame>,
    pushed: u64,
}

/// The stream info of an Opus stream, and the number of samples (at 48 kHz) to skip at the start
/// of the stream.
pub struct OpusConfig {
    pub es: FramedEs,
    pub pre_skip: u16,
}

impl FramedEs {
    /// Create a parser for an Opus stream from the identification header of the sequence start
    /// (RFC 7845 §5.1). If the header is empty, the number of channels is taken from the first
    /// packet (mono or stereo, as Enhanced RTMP specifies).
    pub fn opus(head: &[u8]) -> Result<OpusConfig> {
        if head.is_empty() {
            return Ok(OpusConfig {
                es: FramedEs {
                    codec: Codec::Opus,
                    info: None,
                    grid: 0,
                    pending: VecDeque::new(),
                    pushed: 0,
                },
                pre_skip: 0,
            });
        }

        if head.len() < 19 || &head[..8] != b"OpusHead" || head[8] & 0xf0 != 0 {
            return decode_error("flv: invalid opus identification header");
        }

        let channels = head[9];
        let pre_skip = u16::from_le_bytes([head[10], head[11]]);

        let Some(positions) = opus_channels(channels)
        else {
            return unsupported_error("flv: unsupported opus channel count");
        };

        // The channel mapping family 0 is for mono and stereo only, and the mapping table must be
        // present for the other families.
        let family = head[18];
        let valid = match family {
            0 => channels <= 2 && head.len() == 19,
            1 => head.len() == 21 + usize::from(channels),
            _ => false,
        };

        if !valid {
            return unsupported_error("flv: unsupported opus channel mapping");
        }

        let mut params = AudioCodecParameters::new();
        params
            .for_codec(CODEC_ID_OPUS)
            .with_sample_rate(48_000)
            .with_channels(positions)
            .with_extra_data(head.into());

        Ok(OpusConfig {
            es: FramedEs {
                codec: Codec::Opus,
                info: Some(EsInfo { params, rate: 48_000 }),
                grid: 0,
                pending: VecDeque::new(),
                pushed: 0,
            },
            pre_skip,
        })
    }

    /// Create a parser for a FLAC stream from the sequence start: the STREAMINFO block, optionally
    /// preceded by the "fLaC" marker and the metadata block header.
    pub fn flac(header: &[u8]) -> Result<Self> {
        let mut b = header.strip_prefix(b"fLaC").unwrap_or(header);

        // The metadata block header of the STREAMINFO block.
        if b.len() >= 4 + FLAC_STREAMINFO_LEN && b[0] & 0x7f == 0 && b[1..4] == [0, 0, 34] {
            b = &b[4..];
        }

        let Some(info) = b.get(..FLAC_STREAMINFO_LEN)
        else {
            return decode_error("flv: invalid flac stream info");
        };

        let min_block = u64::from(u16::from_be_bytes([info[0], info[1]]));
        let max_block = u64::from(u16::from_be_bytes([info[2], info[3]]));

        let rate = u32::from(info[10]) << 12 | u32::from(info[11]) << 4 | u32::from(info[12] >> 4);
        let channels = ((info[12] >> 1) & 7) + 1;
        let bits = (u32::from(info[12] & 1) << 4 | u32::from(info[13] >> 4)) + 1;

        if rate == 0 || max_block < 16 || min_block > max_block {
            return decode_error("flv: invalid flac stream info");
        }

        let mut params = AudioCodecParameters::new();
        params
            .for_codec(CODEC_ID_FLAC)
            .with_sample_rate(rate)
            .with_channels(flac_channels(channels))
            .with_bits_per_sample(bits)
            .with_extra_data(info.into());

        Ok(FramedEs {
            codec: Codec::Flac { max_block },
            info: Some(EsInfo { params, rate }),
            // The frames of a stream of a fixed block size all have it, except the last.
            grid: if min_block == max_block { max_block } else { 0 },
            pending: VecDeque::new(),
            pushed: 0,
        })
    }

    /// Create a parser for SWF ADPCM audio of the sample rate `rate` and `channels` channels. A tag
    /// is a packet of one or more blocks of 4096 frames.
    pub fn swf_adpcm(rate: u32, channels: Channels) -> Self {
        let count = channels.count();

        let mut params = AudioCodecParameters::new();
        params
            .for_codec(CODEC_ID_ADPCM_SWF)
            .with_sample_rate(rate)
            .with_channels(channels)
            .with_max_frames_per_packet(SWF_ADPCM_BLOCK_FRAMES * 16);

        FramedEs {
            codec: Codec::SwfAdpcm { channels: count },
            info: Some(EsInfo { params, rate }),
            grid: 0,
            pending: VecDeque::new(),
            pushed: 0,
        }
    }

    /// Create a parser for mono Nellymoser audio of the sample rate `rate`. A tag is one or more
    /// blocks of 64 bytes, each of 256 frames.
    pub fn nellymoser(rate: u32) -> Self {
        let mut params = AudioCodecParameters::new();
        params
            .for_codec(CODEC_ID_NELLYMOSER)
            .with_sample_rate(rate)
            .with_channels(Channels::Positioned(Position::FRONT_LEFT));

        FramedEs {
            codec: Codec::Nellymoser,
            info: Some(EsInfo { params, rate }),
            grid: 0,
            pending: VecDeque::new(),
            pushed: 0,
        }
    }

    /// Create a parser for Speex audio, which in FLV is always 16 kHz mono, a tag is one 20 ms
    /// frame, and there is no header.
    pub fn speex() -> Self {
        let mut params = AudioCodecParameters::new();
        params
            .for_codec(CODEC_ID_SPEEX)
            .with_sample_rate(16_000)
            .with_channels(Channels::Positioned(Position::FRONT_LEFT));

        FramedEs {
            codec: Codec::Speex,
            info: Some(EsInfo { params, rate: 16_000 }),
            grid: SPEEX_FRAME_FRAMES,
            pending: VecDeque::new(),
            pushed: 0,
        }
    }

    /// Create a parser for an AC-3 (or E-AC-3) stream. The stream info is found from the first
    /// frame.
    pub fn ac3(eac3: bool) -> Self {
        FramedEs {
            codec: if eac3 { Codec::Eac3 } else { Codec::Ac3 },
            info: None,
            grid: 0,
            pending: VecDeque::new(),
            pushed: 0,
        }
    }

    /// Find the info and the duration in ticks of the frames in `data`. Returns `None` if the data
    /// does not have a frame.
    fn describe(&mut self, data: &[u8]) -> Option<u64> {
        match self.codec {
            Codec::Opus => {
                let dur = opus_packet_samples(data)?;

                if self.info.is_none() {
                    // Mono or stereo, per the stereo flag of the table of contents.
                    let channels = if data[0] & 4 != 0 { 2 } else { 1 };
                    let head = build_opus_head(channels, 0)?;

                    let mut params = AudioCodecParameters::new();
                    params
                        .for_codec(CODEC_ID_OPUS)
                        .with_sample_rate(48_000)
                        .with_channels(opus_channels(channels)?)
                        .with_extra_data(head);

                    self.info = Some(EsInfo { params, rate: 48_000 });
                }

                Some(dur)
            }
            Codec::SwfAdpcm { channels } => {
                let dur = swf_adpcm_frames(data, channels);
                (dur > 0).then_some(dur)
            }
            Codec::Nellymoser => {
                let blocks = (data.len() / NELLYMOSER_BLOCK_BYTES) as u64;
                (blocks > 0).then_some(blocks * NELLYMOSER_BLOCK_FRAMES)
            }
            Codec::Speex => (!data.is_empty()).then_some(SPEEX_FRAME_FRAMES),
            Codec::Flac { max_block } => {
                if data.is_empty() {
                    return None;
                }

                Some(flac_block_size(data).unwrap_or(max_block))
            }
            Codec::Ac3 | Codec::Eac3 => {
                let eac3 = matches!(self.codec, Codec::Eac3);
                let mut pos = 0;
                let mut dur = 0;

                while let Some(frame) = data.get(pos..).and_then(|d| Ac3Frame::parse(d, eac3)) {
                    if self.info.is_none() && frame.counts {
                        let mut params = AudioCodecParameters::new();
                        params
                            .for_codec(if eac3 { CODEC_ID_EAC3 } else { CODEC_ID_AC3 })
                            .with_sample_rate(frame.rate)
                            .with_channels(frame.channels);

                        self.info = Some(EsInfo { params, rate: frame.rate });

                        // The frames of a stream have the same number of blocks.
                        self.grid = frame.samples;
                    }

                    if frame.counts {
                        dur += frame.samples;
                    }

                    pos += frame.size;
                }

                (dur > 0).then_some(dur)
            }
        }
    }
}

impl EsParser for FramedEs {
    fn push(&mut self, data: &[u8]) {
        let start = self.pushed;
        self.pushed += data.len() as u64;

        if let Some(dur) = self.describe(data) {
            self.pending.push_back(EsFrame {
                start,
                data: data.into(),
                dur,
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
        match self.codec {
            Codec::Opus => target - OPUS_SEEK_PREROLL,
            // The transform overlaps with the previous frame.
            Codec::Ac3 | Codec::Eac3 => target - AC3_FRAME_SAMPLES as i64,
            // The transform overlaps with the previous block.
            Codec::Nellymoser => target - NELLYMOSER_BLOCK_FRAMES as i64,
            // The synthesis filters and the pitch predictor have memory of the previous frame.
            Codec::Speex => target - SPEEX_FRAME_FRAMES as i64,
            Codec::Flac { .. } | Codec::SwfAdpcm { .. } => target,
        }
    }

    fn fresh(&self) -> Box<dyn EsParser> {
        Box::new(FramedEs {
            codec: self.codec,
            info: self.info.clone(),
            grid: self.grid,
            pending: VecDeque::new(),
            pushed: 0,
        })
    }

    fn frame_grid(&self) -> u64 {
        self.grid
    }
}

/// The number of frames of the blocks of SWF ADPCM in `data`: a 2-bit code size, then for each
/// block of each channel an initial 16-bit sample and 6-bit step index, and up to 4095 coded
/// samples of 2 to 5 bits. This is the count that the decoder finds.
fn swf_adpcm_frames(data: &[u8], channels: usize) -> u64 {
    let Some(&first) = data.first()
    else {
        return 0;
    };

    let size = data.len() * 8;
    let bits = usize::from(first >> 6) + 2;
    let mut pos = 2;
    let mut frames = 0;

    while pos + 22 * channels <= size {
        pos += 22 * channels;
        frames += 1;

        let coded = ((size - pos) / (bits * channels)).min(SWF_ADPCM_BLOCK_FRAMES as usize - 1);
        pos += coded * bits * channels;
        frames += coded as u64;
    }

    frames
}

/// The channels of a FLAC stream of `channels` channels, in the order of RFC 9639 §9.1.3.
fn flac_channels(channels: u8) -> Channels {
    use Position as P;

    Channels::Positioned(match channels {
        1 => P::FRONT_LEFT,
        2 => P::FRONT_LEFT | P::FRONT_RIGHT,
        3 => P::FRONT_LEFT | P::FRONT_RIGHT | P::FRONT_CENTER,
        4 => P::FRONT_LEFT | P::FRONT_RIGHT | P::REAR_LEFT | P::REAR_RIGHT,
        5 => P::FRONT_LEFT | P::FRONT_RIGHT | P::FRONT_CENTER | P::REAR_LEFT | P::REAR_RIGHT,
        6 => {
            P::FRONT_LEFT
                | P::FRONT_RIGHT
                | P::FRONT_CENTER
                | P::LFE1
                | P::REAR_LEFT
                | P::REAR_RIGHT
        }
        7 => {
            P::FRONT_LEFT
                | P::FRONT_RIGHT
                | P::FRONT_CENTER
                | P::LFE1
                | P::REAR_CENTER
                | P::SIDE_LEFT
                | P::SIDE_RIGHT
        }
        _ => {
            P::FRONT_LEFT
                | P::FRONT_RIGHT
                | P::FRONT_CENTER
                | P::LFE1
                | P::REAR_LEFT
                | P::REAR_RIGHT
                | P::SIDE_LEFT
                | P::SIDE_RIGHT
        }
    })
}

/// Get the block size (the number of samples of a channel) from the header of the FLAC frame at
/// the start of `b` (RFC 9639 §9.1).
fn flac_block_size(b: &[u8]) -> Option<u64> {
    if b.len() < 5 || b[0] != 0xff || b[1] & 0xfe != 0xf8 {
        return None;
    }

    let code = b[2] >> 4;

    // The coded frame or sample number is UTF-8 coded, with up to 7 bytes.
    let number_len = match b[4] {
        x if x & 0x80 == 0 => 1,
        x if x & 0xe0 == 0xc0 => 2,
        x if x & 0xf0 == 0xe0 => 3,
        x if x & 0xf8 == 0xf0 => 4,
        x if x & 0xfc == 0xf8 => 5,
        x if x & 0xfe == 0xfc => 6,
        0xfe => 7,
        _ => return None,
    };

    let ext = 4 + number_len;

    match code {
        0 => None,
        1 => Some(192),
        2..=5 => Some(576 << (code - 2)),
        6 => Some(u64::from(*b.get(ext)?) + 1),
        7 => Some(u64::from(u16::from_be_bytes([*b.get(ext)?, *b.get(ext + 1)?])) + 1),
        _ => Some(256 << (code - 8)),
    }
}

/// The header of an AC-3 syncframe, or an E-AC-3 frame.
struct Ac3Frame {
    /// The sample rate.
    rate: u32,
    channels: Channels,
    /// The size in bytes.
    size: usize,
    /// The number of samples of each channel.
    samples: u64,
    /// True if the frame is not part of a dependent or secondary substream.
    counts: bool,
}

/// The bit rates in kb/s of the frame size codes `2 * n` and `2 * n + 1` (ATSC A/52 Table 5.18).
const AC3_BITRATES_KBPS: [u32; 19] =
    [32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384, 448, 512, 576, 640];

/// The channels of an AC-3 coding mode `acmod` (ATSC A/52 Table 5.8), and the LFE channel.
fn ac3_channels(acmod: u8, lfe: bool) -> Channels {
    use Position as P;

    let mut ch = match acmod {
        // 1+1 (two independent mono channels).
        0 | 2 => P::FRONT_LEFT | P::FRONT_RIGHT,
        1 => P::FRONT_CENTER,
        3 => P::FRONT_LEFT | P::FRONT_CENTER | P::FRONT_RIGHT,
        4 => P::FRONT_LEFT | P::FRONT_RIGHT | P::REAR_CENTER,
        5 => P::FRONT_LEFT | P::FRONT_CENTER | P::FRONT_RIGHT | P::REAR_CENTER,
        6 => P::FRONT_LEFT | P::FRONT_RIGHT | P::REAR_LEFT | P::REAR_RIGHT,
        _ => P::FRONT_LEFT | P::FRONT_CENTER | P::FRONT_RIGHT | P::REAR_LEFT | P::REAR_RIGHT,
    };

    if lfe {
        ch |= P::LFE1;
    }

    Channels::Positioned(ch)
}

impl Ac3Frame {
    fn parse(b: &[u8], eac3: bool) -> Option<Ac3Frame> {
        if b.len() < 8 || b[0] != 0x0b || b[1] != 0x77 {
            return None;
        }

        if eac3 {
            // ETSI TS 102 366 §E.1.2: strmtyp, substreamid, frmsiz, fscod, numblkscod, acmod,
            // lfeon, bsid.
            let strmtyp = b[2] >> 6;
            let substream = (b[2] >> 3) & 7;
            let size = ((usize::from(b[2] & 7) << 8 | usize::from(b[3])) + 1) * 2;
            let fscod = b[4] >> 6;

            let (rate, blocks) = if fscod == 3 {
                let rate = *[24_000, 22_050, 16_000].get(usize::from((b[4] >> 4) & 3))?;
                (rate, 6)
            }
            else {
                (
                    *[48_000, 44_100, 32_000].get(usize::from(fscod))?,
                    [1, 2, 3, 6][usize::from((b[4] >> 4) & 3)],
                )
            };

            let bsid = b[5] >> 3;

            if !(11..=16).contains(&bsid) || size < 8 {
                return None;
            }

            return Some(Ac3Frame {
                rate,
                channels: ac3_channels((b[4] >> 1) & 7, b[4] & 1 != 0),
                size,
                samples: 256 * blocks,
                counts: strmtyp != 1 && substream == 0,
            });
        }

        // ATSC A/52 §5.3.1, §5.4.1: syncword, crc1, fscod, frmsizecod, bsid, bsmod, acmod.
        let fscod = b[4] >> 6;
        let frmsizecod = usize::from(b[4] & 0x3f);
        let bsid = b[5] >> 3;

        if fscod == 3 || frmsizecod >= 38 || bsid > 10 {
            return None;
        }

        let kbps = AC3_BITRATES_KBPS[frmsizecod >> 1];

        let (rate, words) = match fscod {
            0 => (48_000, kbps * 2),
            // At 44.1 kHz, the odd codes have one more word (padding).
            1 => (44_100, kbps * 96_000 / 44_100 + (frmsizecod & 1) as u32),
            _ => (32_000, kbps * 3),
        };

        let acmod = b[6] >> 5;

        // The optional fields following acmod, before lfeon.
        let mut bit = 3;

        if acmod & 1 != 0 && acmod != 1 {
            bit += 2;
        }

        if acmod & 4 != 0 {
            bit += 2;
        }

        if acmod == 2 {
            bit += 2;
        }

        let lfe = b[6] >> (7 - bit) & 1 != 0;

        Some(Ac3Frame {
            rate,
            channels: ac3_channels(acmod, lfe),
            size: words as usize * 2,
            samples: AC3_FRAME_SAMPLES,
            counts: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_flac_block_sizes() {
        // 4096 samples (code 12), 44.1 kHz (code 9), one byte frame number.
        assert_eq!(flac_block_size(&[0xff, 0xf8, 0xc9, 0x18, 0x00, 0x00]), Some(4096));
        // 192 samples (code 1).
        assert_eq!(flac_block_size(&[0xff, 0xf9, 0x19, 0x18, 0x00, 0x00]), Some(192));
        // 576 << 2 samples (code 4).
        assert_eq!(flac_block_size(&[0xff, 0xf8, 0x49, 0x18, 0x00, 0x00]), Some(2304));
        // Block size - 1 in 8 bits (code 6), and in 16 bits (code 7); two byte frame number.
        assert_eq!(flac_block_size(&[0xff, 0xf8, 0x69, 0x18, 0xc2, 0x80, 0x63, 0]), Some(100));
        assert_eq!(flac_block_size(&[0xff, 0xf8, 0x79, 0x18, 0x00, 0x03, 0xe7, 0]), Some(1000));
        // Not a frame.
        assert_eq!(flac_block_size(&[0xff, 0xf8, 0x09, 0x18, 0x00]), None);
        assert_eq!(flac_block_size(&[0x12, 0xf8, 0xc9, 0x18, 0x00]), None);
    }

    #[test]
    fn verify_nellymoser_and_speex_durations() {
        let mut es = FramedEs::nellymoser(22_050);
        assert_eq!(es.info().unwrap().rate, 22_050);
        es.push(&[0u8; 63]);
        es.push(&[0u8; 128]);
        assert_eq!(es.next_frame(false).unwrap().dur, 512);
        assert!(es.next_frame(true).is_none());

        let mut es = FramedEs::speex();
        let info = es.info().unwrap();
        assert_eq!(info.rate, 16_000);
        assert!(info.params.extra_data.is_none());
        es.push(&[1u8; 42]);
        es.push(&[]);
        assert_eq!(es.next_frame(false).unwrap().dur, 320);
        assert!(es.next_frame(true).is_none());
    }

    #[test]
    fn verify_ac3_headers() {
        // 48 kHz, 192 kbps (code 20 -> 384 words), bsid 8, acmod 7 (3/2), lfeon.
        let mut b = vec![0u8; 8];
        b[..2].copy_from_slice(&[0x0b, 0x77]);
        b[4] = 20 & 0x3f;
        b[5] = 8 << 3;
        // acmod 7, cmixlev 2 bits, surmixlev 2 bits, lfeon 1.
        b[6] = 7 << 5 | 0 << 3 | 0 << 1 | 1;
        let f = Ac3Frame::parse(&b, false).unwrap();
        assert_eq!((f.rate, f.size, f.samples), (48_000, 768, 1536));
        assert_eq!(f.channels.count(), 6);

        // 44.1 kHz, 128 kbps: 278 words for the even code, 279 for the odd one.
        b[4] = 1 << 6 | 16;
        assert_eq!(Ac3Frame::parse(&b, false).unwrap().size, 278 * 2);
        b[4] = 1 << 6 | 17;
        assert_eq!(Ac3Frame::parse(&b, false).unwrap().size, 279 * 2);

        // E-AC-3: independent substream 0, frmsiz 0x3ff, 48 kHz, 6 blocks, acmod 2.
        let e = [0x0b, 0x77, 0x03, 0xff, 3 << 4 | 2 << 1, 16 << 3, 0, 0];
        let f = Ac3Frame::parse(&e, true).unwrap();
        assert_eq!((f.rate, f.size, f.samples, f.counts), (48_000, 2048, 1536, true));
        assert_eq!(f.channels.count(), 2);

        // The same, with fscod 3 and fscod2 1 (22.05 kHz) is always 6 blocks.
        let e = [0x0b, 0x77, 0x43, 0xff, 3 << 6 | 1 << 4 | 2 << 1, 16 << 3, 0, 0];
        let f = Ac3Frame::parse(&e, true).unwrap();
        assert_eq!((f.rate, f.samples, f.counts), (22_050, 1536, false));
    }

    #[test]
    fn verify_opus_pre_skip_and_empty_header() {
        let head = build_opus_head(2, 312).unwrap();
        let cfg = FramedEs::opus(&head).unwrap();
        assert_eq!(cfg.pre_skip, 312);
        assert_eq!(cfg.es.info().unwrap().rate, 48_000);

        // Without a header, the channels are found from the first packet.
        let mut es = FramedEs::opus(&[]).unwrap().es;
        assert!(es.info().is_none());
        // 20 ms CELT-only frame, stereo, one frame.
        es.push(&[0xfc, 1, 2, 3]);
        let f = es.next_frame(false).unwrap();
        assert_eq!(f.dur, 960);
        assert_eq!(es.info().unwrap().params.channels.as_ref().unwrap().count(), 2);

        assert!(FramedEs::opus(b"OpusHead").is_err());
    }
}
