// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

#![warn(rust_2018_idioms)]
#![forbid(unsafe_code)]
// The decoder mirrors the structure and the index arithmetic of the reference decoder.
#![allow(clippy::needless_range_loop)]
// The following lints are allowed in all Symphonia crates. Please see clippy.toml for their
// justification.
#![allow(clippy::comparison_chain)]
#![allow(clippy::excessive_precision)]
#![allow(clippy::identity_op)]
#![allow(clippy::manual_range_contains)]

//! A pure Rust MPEG-4 Audio Lossless Coding (ALS) decoder.
//!
//! Decodes ISO/IEC 14496-3 subpart 11 streams: forward-adaptive linear prediction with PARCOR
//! coefficients, long-term prediction, block switching, Rice and block Gilbert-Moore (BGMC)
//! residual coding, joint-stereo and multi-channel coding, channel rearrangement, `shift_lsbs`
//! blocks, and IEEE 754 floating-point streams (including the MLZ compressed difference signal).
//!
//! The decoder is a port of the ALS decoder of FFmpeg (`libavcodec/alsdec.c`, LGPL-2.1+, see the
//! `NOTICE` file), and reproduces its output sample for sample.
//!
//! The adaptive RLS-LMS predictor of the ALS "RLS-LMS" extension (`RLSLMS` flag of the config) is
//! not implemented: FFmpeg does not implement it either, and such streams are rejected.
//!
//! # Codec parameters
//!
//! The `extra_data` must hold the `ALSSpecificConfig`, preceded by the `AudioSpecificConfig`
//! (object type 36) as in MP4 and by nothing as at the start of an `.als` file. Each packet is one
//! ALS frame, and must be a random access frame unless `ra_distance` is 0 or the decoder has been
//! decoding the stream from its start (the decoder numbers the frames it decodes, and the last
//! frame of the stream is shortened according to the number of samples of the config).
//!
//! # Output
//!
//! Integer streams of up to 16 bits are decoded to 16-bit samples (8-bit samples are scaled to 16
//! bits) and others to 32-bit samples (24-bit samples are scaled to 32 bits), as FFmpeg does.
//! Floating-point streams are decoded to `f32`.
//!
//! Frames where the config has a deviating behaviour from the specification, but that FFmpeg
//! decodes the same, are documented where they occur. In particular, if there is no joint stereo
//! and no multi-channel coding the reference decodes the channels `2n` and `2n + 1` (for `n > 0`)
//! as a channel pair with interleaved blocks; this decoder does the same.

mod bgmc;
mod bgmc_tables;
mod bits;
mod config;
mod mlz;
mod softfloat;
mod tables;

use symphonia_core::audio::{
    AsGenericAudioBufferRef, AudioBuffer, AudioMut, AudioSpec, Channels, GenericAudioBufferRef,
    Position,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::FinalizeResult;
use symphonia_core::codecs::audio::well_known::CODEC_ID_ALS;
use symphonia_core::codecs::audio::{AudioCodecParameters, AudioDecoder, AudioDecoderOptions};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Result, decode_error, limit_error, unsupported_error};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use log::warn;

use bgmc::Bgmc;
use bits::Bits;
use config::{Config, ceil_log2};
use mlz::Mlz;
use softfloat::{FLOAT_1, SoftFloat, reconstruct};
use tables::{LTP_GAIN_VALUES, MCC_WEIGHTINGS, PARCOR_RICE_TABLE, PARCOR_SCALED_VALUES, TAIL_CODE};

/// The most samples of the buffer of decoded samples (all channels, one frame plus history).
const MAX_RAW_SAMPLES: usize = 1 << 27;

/// The data of a channel in a block of the multi-channel correlation (`ALSChannelData`).
#[derive(Clone, Copy, Default)]
struct ChannelData {
    stop_flag: bool,
    master_channel: usize,
    time_diff_flag: bool,
    time_diff_sign: bool,
    time_diff_index: i32,
    weighting: [i32; 6],
}

/// A block of a channel (`ALSBlockData`). Positions index into the decoded sample buffer.
#[derive(Clone, Copy)]
struct Block {
    /// Number of samples in the block.
    len: usize,
    /// Is this a random access block?
    ra: bool,
    /// Index of the per-channel state (always 0 unless multi-channel coding is on).
    ci: usize,
    /// Does this block contain a difference signal?
    js: bool,
    /// Position of the first sample of the block.
    pos: usize,
    /// Position of the first sample of the other block of a channel pair.
    other: Option<usize>,
}

/// Convert the PARCOR coefficient `k` to a direct filter coefficient: `cof[0..=k]` from
/// `par[0..=k]` (FFmpeg `parcor_to_lpc`).
fn parcor_to_lpc(k: usize, par: &[i32], cof: &mut [i32]) {
    let mul = |a: i32, b: i32| (i64::from(a) * i64::from(b) + (1 << 19)) >> 20;

    let (mut i, mut j) = (0usize, k as isize - 1);

    while (i as isize) < j {
        let ju = j as usize;
        let tmp1 = mul(par[k], cof[ju]) as u32;
        cof[ju] = (i64::from(cof[ju]) + mul(par[k], cof[i])) as i32;
        cof[i] = cof[i].wrapping_add(tmp1 as i32);
        i += 1;
        j -= 1;
    }

    if i as isize == j {
        cof[i] = (i64::from(cof[i]) + mul(par[k], cof[i])) as i32;
    }

    cof[k] = par[k];
}

/// Parse the `bs_info` field to extract the block partitioning used in block switching mode
/// (ISO/IEC 14496-3 11.6.2). Pushes the division level of each block.
fn parse_bs_info(bs_info: u32, n: u32, div: u32, div_blocks: &mut Vec<u32>) {
    if n < 31 && ((bs_info << n) & 0x4000_0000) != 0 {
        // The investigated bit is set: recursively check both children.
        let n = n * 2;
        parse_bs_info(bs_info, n + 1, div + 1, div_blocks);
        parse_bs_info(bs_info, n + 2, div + 1, div_blocks);
    }
    else {
        div_blocks.push(div);
    }
}

enum Output {
    S16(AudioBuffer<i16>),
    S32(AudioBuffer<i32>),
    F32(AudioBuffer<f32>),
}

/// The CRC-32 (IEEE 802.3) of the decoded samples.
struct Crc {
    table: [u32; 256],
    value: u32,
    /// The number of samples per channel that went into the CRC.
    samples: u64,
}

impl Crc {
    fn new() -> Crc {
        let mut table = [0u32; 256];
        for (i, entry) in table.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xedb8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *entry = c;
        }
        Crc { table, value: 0xffff_ffff, samples: 0 }
    }

    fn update(&mut self, byte: u8) {
        self.value =
            self.table[((self.value ^ u32::from(byte)) & 0xff) as usize] ^ (self.value >> 8);
    }
}

/// MPEG-4 ALS decoder.
pub struct AlsDecoder {
    params: AudioCodecParameters,
    opts: AudioDecoderOptions,
    cfg: Config,
    /// The maximum Rice parameter allowed in the entropy coding of the first residuals.
    s_max: u32,
    /// The number of bits of the lag of long-term prediction.
    ltp_lag_length: u32,
    /// The number of samples of the history in front of each channel.
    channel_size: usize,
    /// The ID of the frame to decode.
    frame_id: u64,
    cur_frame_length: usize,
    js_switch: bool,
    num_blocks: usize,
    div_blocks: Vec<u32>,
    bgmc: Option<Bgmc>,

    /// The decoded samples of all channels, each preceded by `max_order` samples of history.
    raw: Vec<i32>,
    prev_raw: Vec<i32>,
    lpc_rev: Vec<i32>,

    // The state of the channels (a single one unless multi-channel coding is on).
    const_block: Vec<bool>,
    shift_lsbs: Vec<u32>,
    opt_order: Vec<usize>,
    store_prev_samples: Vec<bool>,
    use_ltp: Vec<bool>,
    ltp_lag: Vec<i32>,
    ltp_gain: Vec<[i32; 5]>,
    quant_cof: Vec<Vec<i32>>,
    lpc_cof: Vec<Vec<i32>>,

    // Multi-channel coding.
    chan_data: Vec<ChannelData>,
    reverted: Vec<bool>,

    // Floating-point.
    mlz: Option<Mlz>,
    acf: Vec<SoftFloat>,
    last_acf_mantissa: Vec<i32>,
    shift_value: Vec<i32>,
    last_shift_value: Vec<i32>,
    raw_mantissa: Vec<Vec<i32>>,
    larray: Vec<u8>,
    nbits: Vec<i32>,

    crc: Option<Crc>,
    verify_ok: Option<bool>,
    buf: Output,
    /// The planar samples of the frames of the packet being decoded.
    acc: Vec<Vec<i32>>,
}

impl AlsDecoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        if params.codec != CODEC_ID_ALS {
            return unsupported_error("als: invalid codec");
        }

        let Some(extra_data) = &params.extra_data
        else {
            return decode_error("als: missing config");
        };

        let cfg = Config::parse(extra_data)?;

        if cfg.rlslms {
            return unsupported_error("als: RLS-LMS prediction");
        }

        let channels = cfg.channels;
        let max_order = cfg.max_order;
        let channel_size = cfg.frame_length + max_order;

        if channels.saturating_mul(channel_size) > MAX_RAW_SAMPLES {
            return limit_error("als: frame too large");
        }

        let num_buffers = if cfg.mc_coding { channels } else { 1 };

        let layout = match channels {
            1 => Channels::Positioned(Position::FRONT_LEFT),
            2 => Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT),
            n => Channels::Discrete(n as u16),
        };

        let spec = AudioSpec::new(cfg.sample_rate, layout.clone());
        let buf = if cfg.floating {
            Output::F32(AudioBuffer::new(spec, cfg.frame_length))
        }
        else if cfg.resolution > 1 {
            Output::S32(AudioBuffer::new(spec, cfg.frame_length))
        }
        else {
            Output::S16(AudioBuffer::new(spec, cfg.frame_length))
        };

        let (mlz, acf, raw_mantissa, larray, nbits) = if cfg.floating {
            (
                Some(Mlz::new()),
                vec![FLOAT_1; channels],
                vec![vec![0; cfg.frame_length]; channels],
                vec![0; cfg.frame_length * 4],
                vec![0; cfg.frame_length],
            )
        }
        else {
            (None, Vec::new(), Vec::new(), Vec::new(), Vec::new())
        };

        let mut params = params.clone();
        params.with_sample_rate(cfg.sample_rate);
        params.with_channels(layout);

        let verify_crc = opts.verify && cfg.crc.is_some() && cfg.resolution > 0;

        Ok(AlsDecoder {
            params,
            opts: *opts,
            s_max: if cfg.resolution > 1 { 31 } else { 15 },
            ltp_lag_length: 8
                + u32::from(cfg.sample_rate >= 96000)
                + u32::from(cfg.sample_rate >= 192000),
            channel_size,
            frame_id: 0,
            cur_frame_length: cfg.frame_length,
            js_switch: false,
            num_blocks: 0,
            div_blocks: Vec::with_capacity(32),
            bgmc: cfg.bgmc.then(Bgmc::new),
            raw: vec![0; channels * channel_size],
            prev_raw: vec![0; max_order],
            lpc_rev: vec![0; max_order],
            const_block: vec![false; num_buffers],
            shift_lsbs: vec![0; num_buffers],
            opt_order: vec![0; num_buffers],
            store_prev_samples: vec![false; num_buffers],
            use_ltp: vec![false; num_buffers],
            ltp_lag: vec![0; num_buffers],
            ltp_gain: vec![[0; 5]; num_buffers],
            quant_cof: vec![vec![0; max_order]; num_buffers],
            lpc_cof: vec![vec![0; max_order]; num_buffers],
            chan_data: if cfg.mc_coding {
                vec![ChannelData::default(); channels * channels]
            }
            else {
                Vec::new()
            },
            reverted: if cfg.mc_coding { vec![false; channels] } else { Vec::new() },
            mlz,
            acf,
            last_acf_mantissa: vec![0; channels],
            shift_value: vec![0; channels],
            last_shift_value: vec![0; channels],
            raw_mantissa,
            larray,
            nbits,
            crc: verify_crc.then(Crc::new),
            verify_ok: None,
            buf,
            acc: vec![Vec::new(); channels],
            cfg,
        })
    }

    /// The position of the first sample of channel `c` in the decoded sample buffer.
    fn chan_base(&self, c: usize) -> usize {
        c * self.channel_size + self.cfg.max_order
    }

    /// Move the last `max_order` samples of the frame in front of the channel as history.
    fn store_carryover(&mut self, c: usize) {
        let max_order = self.cfg.max_order;
        let start = self.chan_base(c) - max_order;
        self.raw.copy_within(
            start + self.cfg.frame_length..start + self.cfg.frame_length + max_order,
            start,
        );
    }

    /// Read the block switching field if necessary and set the actual block sizes. Also assures
    /// that the block sizes of the last frame correspond to the actual number of samples.
    fn get_block_sizes(&mut self, bits: &mut Bits<'_>, bs_info: &mut u32) {
        if self.cfg.block_switching != 0 {
            let bs_info_len = 1u32 << (self.cfg.block_switching + 2);
            *bs_info = bits.get(bs_info_len);
            *bs_info <<= 32 - bs_info_len;
        }

        self.div_blocks.clear();
        parse_bs_info(*bs_info, 0, 0, &mut self.div_blocks);

        for div in self.div_blocks.iter_mut() {
            *div = (self.cfg.frame_length >> *div) as u32;
        }

        self.num_blocks = self.div_blocks.len();

        // The last frame may have an overdetermined block structure given in the bitstream. The
        // block structure is actually used but the block sizes are adapted to fit the actual
        // number of available samples (this is what the reference codec RM22 revision 2 does).
        if self.cur_frame_length != self.cfg.frame_length {
            let mut remaining = self.cur_frame_length as u32;

            for b in 0..self.num_blocks {
                if remaining <= self.div_blocks[b] {
                    self.div_blocks[b] = remaining;
                    self.num_blocks = b + 1;
                    break;
                }

                remaining -= self.div_blocks[b];
            }
        }
    }

    fn read_const_block(&mut self, bits: &mut Bits<'_>, bd: &mut Block) -> Result<()> {
        if bd.len == 0 {
            return decode_error("als: empty block");
        }

        self.raw[bd.pos] = 0;
        let constant = bits.get1() != 0; // 1: constant value, 0: zero block (silence)
        bd.js = bits.get1() != 0;
        bits.skip(5);

        if constant {
            let const_val_bits = if self.cfg.floating { 24 } else { self.cfg.bits_per_sample };
            self.raw[bd.pos] = bits.get_signed(const_val_bits);
        }

        // Ensure constant block decoding.
        self.const_block[bd.ci] = true;

        Ok(())
    }

    fn read_var_block(&mut self, bits: &mut Bits<'_>, bd: &mut Block) -> Result<()> {
        let ci = bd.ci;
        let max_order = self.cfg.max_order;

        // Ensure variable block decoding.
        self.const_block[ci] = false;
        self.opt_order[ci] = 1;
        bd.js = bits.get1() != 0;

        // The number of sub-blocks for the entropy decoding.
        let log2_sub_blocks = if !self.cfg.bgmc && !self.cfg.sb_part {
            0
        }
        else if self.cfg.bgmc && self.cfg.sb_part {
            bits.get(2)
        }
        else {
            2 * bits.get1()
        };

        let sub_blocks = 1usize << log2_sub_blocks;

        // The block length must be evenly divisible by the number of sub-blocks.
        if bd.len & (sub_blocks - 1) != 0 || bd.len == 0 {
            return decode_error("als: block length is not divisible by the sub-blocks");
        }

        let sb_length = bd.len >> log2_sub_blocks;

        let mut s = [0u32; 8];
        let mut sx = [0u32; 8];

        if self.cfg.bgmc {
            s[0] = bits.get(8 + u32::from(self.cfg.resolution > 1));
            for k in 1..sub_blocks {
                s[k] = s[k - 1].wrapping_add(bits.rice(2) as u32);
            }

            for k in 0..sub_blocks {
                sx[k] = s[k] & 0x0f;
                s[k] >>= 4;
            }
        }
        else {
            s[0] = bits.get(4 + u32::from(self.cfg.resolution > 1));
            for k in 1..sub_blocks {
                s[k] = s[k - 1].wrapping_add(bits.rice(0) as u32);
            }
        }

        for k in 1..sub_blocks {
            if s[k] > 32 {
                return decode_error("als: invalid Rice parameter");
            }
        }

        self.shift_lsbs[ci] = 0;
        if bits.get1() != 0 {
            self.shift_lsbs[ci] = bits.get(4) + 1;
        }

        self.store_prev_samples[ci] = (bd.js && bd.other.is_some()) || self.shift_lsbs[ci] != 0;

        let opt_order;

        if self.cfg.adapt_order && max_order != 0 {
            let clipped = ((bd.len >> 3) as i64 - 1).clamp(2, max_order as i64 + 1);
            let opt_order_length = ceil_log2(clipped as u32);
            opt_order = bits.get(opt_order_length) as usize;
            if opt_order > max_order {
                return decode_error("als: predictor order too large");
            }
        }
        else {
            opt_order = max_order;
        }
        self.opt_order[ci] = opt_order;

        if opt_order != 0 {
            let quant_cof = &mut self.quant_cof[ci];
            let add_base: u32;

            if self.cfg.coef_table == 3 {
                add_base = 0x7f;

                quant_cof[0] = 32 * i32::from(PARCOR_SCALED_VALUES[bits.get(7) as usize]);

                if opt_order > 1 {
                    quant_cof[1] = -32 * i32::from(PARCOR_SCALED_VALUES[bits.get(7) as usize]);
                }

                for k in 2..opt_order {
                    quant_cof[k] = bits.get(7) as i32;
                }
            }
            else {
                add_base = 1;

                let table = &PARCOR_RICE_TABLE[self.cfg.coef_table as usize];

                // Coefficients 0 to 19.
                let k_max = opt_order.min(20);
                for k in 0..k_max {
                    let (offset, rice_param) = table[k];
                    let q = bits.rice(u32::from(rice_param)).wrapping_add(i32::from(offset));
                    if q < -64 || q > 63 {
                        return decode_error("als: quantized coefficient out of range");
                    }
                    quant_cof[k] = q;
                }

                // Coefficients 20 to 126.
                let k_max = opt_order.min(127);
                for k in 20..k_max {
                    quant_cof[k] = bits.rice(2).wrapping_add((k & 1) as i32);
                }

                // Coefficients 127 to opt_order.
                for k in 127..opt_order {
                    quant_cof[k] = bits.rice(1);
                }

                quant_cof[0] = 32 * i32::from(PARCOR_SCALED_VALUES[(quant_cof[0] + 64) as usize]);

                if opt_order > 1 {
                    quant_cof[1] =
                        -32 * i32::from(PARCOR_SCALED_VALUES[(quant_cof[1] + 64) as usize]);
                }
            }

            for k in 2..opt_order {
                quant_cof[k] =
                    (quant_cof[k] as u32).wrapping_mul(1 << 14).wrapping_add(add_base << 13) as i32;
            }
        }

        // The LTP gain and lag values.
        if self.cfg.long_term_prediction {
            self.use_ltp[ci] = bits.get1() != 0;

            if self.use_ltp[ci] {
                let gain = &mut self.ltp_gain[ci];

                gain[0] = bits.rice(1).wrapping_mul(8);
                gain[1] = bits.rice(2).wrapping_mul(8);

                let r = bits.get_unary(4) as usize;
                let c = bits.get(2) as usize;
                if r >= 4 {
                    return decode_error("als: LTP gain overflow");
                }

                gain[2] = i32::from(LTP_GAIN_VALUES[r][c]);
                gain[3] = bits.rice(2).wrapping_mul(8);
                gain[4] = bits.rice(1).wrapping_mul(8);

                self.ltp_lag[ci] =
                    (bits.get(self.ltp_lag_length) as usize + (opt_order + 1).max(4)) as i32;
            }
        }

        // The first values and residuals of a random access block.
        let mut start = 0;
        if bd.ra {
            start = opt_order.min(3);
            if sb_length <= start {
                return decode_error("als: sub-block shorter than the random access start");
            }

            if opt_order > 0 {
                self.raw[bd.pos] = bits.rice(self.cfg.bits_per_sample - 4);
            }
            if opt_order > 1 {
                self.raw[bd.pos + 1] = bits.rice((s[0] + 3).min(self.s_max));
            }
            if opt_order > 2 {
                self.raw[bd.pos + 2] = bits.rice((s[0] + 1).min(self.s_max));
            }
        }

        // All the residuals.
        if let Some(bgmc) = self.bgmc.as_mut() {
            let mut delta = [0i32; 8];
            let mut k = [0u32; 8];
            let b = ((ceil_log2(bd.len as u32) as i32 - 3) >> 1).clamp(0, 5) as u32;

            // The most significant bits.
            let Some(mut state) = Bgmc::init(bits)
            else {
                return decode_error("als: out of bits");
            };

            let mut pos = bd.pos + start;

            for sb in 0..sub_blocks {
                let sb_len = sb_length - if sb == 0 { start } else { 0 };

                k[sb] = s[sb].saturating_sub(b);
                delta[sb] = 5 - s[sb] as i32 + k[sb] as i32;

                if k[sb] >= 32 {
                    return decode_error("als: invalid BGMC parameter");
                }

                if !bgmc.decode(
                    bits,
                    &mut self.raw[pos..pos + sb_len],
                    delta[sb],
                    sx[sb] as usize,
                    &mut state,
                ) {
                    return decode_error("als: invalid BGMC data");
                }

                pos += sb_len;
            }

            Bgmc::finish(bits);

            // The least significant bits and the tails.
            let mut pos = bd.pos + start;
            let mut first = start;

            for sb in 0..sub_blocks {
                let cur_tail_code = u32::from(TAIL_CODE[sx[sb] as usize][delta[sb] as usize]);
                let cur_k = k[sb];
                let cur_s = s[sb];

                for _ in first..sb_length {
                    let mut res = self.raw[pos];

                    if res as u32 == cur_tail_code {
                        let max_msb =
                            (2 + u32::from(sx[sb] > 2) + u32::from(sx[sb] > 10)) << (5 - delta[sb]);

                        res = bits.rice(cur_s);

                        if res >= 0 {
                            res = res.wrapping_add((max_msb << cur_k) as i32);
                        }
                        else {
                            res = res.wrapping_sub(((max_msb - 1) << cur_k) as i32);
                        }
                    }
                    else {
                        if res as u32 > cur_tail_code {
                            res -= 1;
                        }

                        if res & 1 != 0 {
                            res = res.wrapping_neg();
                        }

                        res >>= 1;

                        if cur_k != 0 {
                            res = ((res as u32).wrapping_mul(1 << cur_k) | bits.get(cur_k)) as i32;
                        }
                    }

                    self.raw[pos] = res;
                    pos += 1;
                }

                first = 0;
            }
        }
        else {
            let mut pos = bd.pos + start;
            let mut first = start;

            for sb in 0..sub_blocks {
                for _ in first..sb_length {
                    self.raw[pos] = bits.rice(s[sb]);
                    pos += 1;
                }
                first = 0;
            }
        }

        Ok(())
    }

    /// Reconstruct the samples of a non-constant block from its residuals.
    fn decode_var_block(&mut self, bd: &Block) {
        let ci = bd.ci;
        let opt_order = self.opt_order[ci];
        let max_order = self.cfg.max_order;
        let len = bd.len;
        let pos = bd.pos;

        // Reverse the long-term prediction.
        if self.use_ltp[ci] {
            let lag = i64::from(self.ltp_lag[ci]);
            let gain = self.ltp_gain[ci];
            let mut ltp_smp = (lag - 2).max(0) as usize;

            while ltp_smp < len {
                let center = ltp_smp as i64 - lag;
                let begin = (center - 2).max(0);
                let end = center + 3;
                let tab0 = (5 - (end - begin)) as usize;

                let mut y = 1i64 << 6;

                for (tab, base) in (tab0..).zip(begin..end) {
                    y = y.wrapping_add(
                        i64::from(gain[tab]).wrapping_mul(i64::from(self.raw[pos + base as usize])),
                    );
                }

                self.raw[pos + ltp_smp] = self.raw[pos + ltp_smp].wrapping_add((y >> 7) as i32);
                ltp_smp += 1;
            }
        }

        let quant_cof = &self.quant_cof[ci];
        let lpc_cof = &mut self.lpc_cof[ci];
        let raw = &mut self.raw;

        // Reconstruct all samples from the residuals.
        let mut smp = 0;

        if bd.ra {
            let n = opt_order.min(len);

            for s in 0..n {
                let mut y = 1i64 << 19;

                for sb in 0..s {
                    y = y.wrapping_add(
                        i64::from(lpc_cof[sb]).wrapping_mul(i64::from(raw[pos + s - 1 - sb])),
                    );
                }

                raw[pos + s] = raw[pos + s].wrapping_sub((y >> 20) as i32);
                parcor_to_lpc(s, quant_cof, lpc_cof);
            }

            smp = n;
        }
        else {
            for k in 0..opt_order {
                parcor_to_lpc(k, quant_cof, lpc_cof);
            }

            // Store the previous samples in case that they have to be altered.
            if self.store_prev_samples[ci] {
                self.prev_raw.copy_from_slice(&raw[pos - max_order..pos]);
            }

            // Reconstruct the difference signal for the prediction (joint stereo).
            if let (true, Some(other)) = (bd.js, bd.other) {
                let (left, right) = if other > pos { (pos, other) } else { (other, pos) };

                for sb in 1..=max_order {
                    raw[pos - sb] = raw[right - sb].wrapping_sub(raw[left - sb]);
                }
            }

            // Reconstruct the shifted signal.
            if self.shift_lsbs[ci] != 0 {
                for sb in 1..=max_order {
                    raw[pos - sb] >>= self.shift_lsbs[ci];
                }
            }
        }

        // Reverse the linear prediction coefficients for efficiency.
        let lpc_rev = &mut self.lpc_rev[..opt_order];
        for (sb, c) in lpc_rev.iter_mut().enumerate() {
            *c = lpc_cof[opt_order - 1 - sb];
        }

        // Reconstruct the raw samples.
        for p in smp..len {
            let base = pos + p;
            let mut y = 1i64 << 19;

            for (c, r) in lpc_rev.iter().zip(&raw[base - opt_order..base]) {
                y = y.wrapping_add(i64::from(*c).wrapping_mul(i64::from(*r)));
            }

            raw[base] = raw[base].wrapping_sub((y >> 20) as i32);
        }

        // Restore the previous samples in case that they have been altered.
        if self.store_prev_samples[ci] {
            raw[pos - max_order..pos].copy_from_slice(&self.prev_raw);
        }
    }

    fn decode_block(&mut self, bd: &Block) {
        if self.const_block[bd.ci] {
            let val = self.raw[bd.pos];
            self.raw[bd.pos + 1..bd.pos + bd.len].fill(val);
        }
        else {
            self.decode_var_block(bd);
        }

        let shift = self.shift_lsbs[bd.ci];
        if shift != 0 {
            for s in &mut self.raw[bd.pos..bd.pos + bd.len] {
                *s = ((*s as u32) << shift) as i32;
            }
        }
    }

    fn read_block(&mut self, bits: &mut Bits<'_>, bd: &mut Block) -> Result<()> {
        self.shift_lsbs[bd.ci] = 0;

        if bits.left() < 7 {
            return decode_error("als: out of bits");
        }

        // Read the block type flag and read the samples accordingly.
        let ret = if bits.get1() != 0 {
            self.read_var_block(bits, bd)
        }
        else {
            self.read_const_block(bits, bd)
        };

        if !self.cfg.mc_coding || self.js_switch {
            bits.align();
        }

        ret
    }

    /// Decode the blocks of a channel independently.
    fn decode_blocks_ind(&mut self, bits: &mut Bits<'_>, ra_frame: bool, c: usize) -> Result<()> {
        let mut bd =
            Block { len: 0, ra: ra_frame, ci: 0, js: false, pos: self.chan_base(c), other: None };

        for b in 0..self.num_blocks {
            bd.len = self.div_blocks[b] as usize;

            self.read_block(bits, &mut bd)?;
            self.decode_block(&bd);

            bd.pos += bd.len;
            bd.ra = false;
        }

        Ok(())
    }

    /// Decode the blocks of a channel pair dependently.
    fn decode_blocks(&mut self, bits: &mut Bits<'_>, ra_frame: bool, c: usize) -> Result<()> {
        let base0 = self.chan_base(c);
        let base1 = self.chan_base(c + 1);
        let mut offset = 0;

        let mut bd0 = Block { len: 0, ra: ra_frame, ci: 0, js: false, pos: 0, other: None };
        let mut bd1 = bd0;

        for b in 0..self.num_blocks {
            let len = self.div_blocks[b] as usize;

            bd0.len = len;
            bd1.len = len;
            bd0.pos = base0 + offset;
            bd1.pos = base1 + offset;
            bd0.other = Some(bd1.pos);
            bd1.other = Some(bd0.pos);

            self.read_block(bits, &mut bd0)?;
            self.decode_block(&bd0);
            self.read_block(bits, &mut bd1)?;
            self.decode_block(&bd1);

            // Reconstruct the joint-stereo blocks.
            if bd0.js {
                for s in 0..len {
                    self.raw[bd0.pos + s] =
                        self.raw[bd1.pos + s].wrapping_sub(self.raw[bd0.pos + s]);
                }
            }
            else if bd1.js {
                for s in 0..len {
                    self.raw[bd1.pos + s] =
                        self.raw[bd1.pos + s].wrapping_add(self.raw[bd0.pos + s]);
                }
            }

            offset += len;
            bd0.ra = false;
            bd1.ra = false;
        }

        // Store the carryover samples. The ones of the other channel are stored by the caller.
        self.store_carryover(c);

        Ok(())
    }

    fn als_weighting(bits: &mut Bits<'_>, k: u32, off: i32) -> i32 {
        let idx = bits.rice(k).wrapping_add(off).clamp(0, MCC_WEIGHTINGS.len() as i32 - 1);
        i32::from(MCC_WEIGHTINGS[idx as usize])
    }

    /// Read the channel data of the multi-channel correlation of channel `c`.
    fn read_channel_data(&mut self, bits: &mut Bits<'_>, c: usize) -> Result<()> {
        let channels = self.cfg.channels;
        let mut entries = 0;

        while entries < channels {
            let cd = &mut self.chan_data[c * channels + entries];

            cd.stop_flag = bits.get1() != 0;
            if cd.stop_flag {
                break;
            }

            cd.master_channel = bits.get(ceil_log2(channels as u32)) as usize;

            if cd.master_channel >= channels {
                return decode_error("als: invalid master channel");
            }

            if cd.master_channel != c {
                cd.time_diff_flag = bits.get1() != 0;
                cd.weighting[0] = Self::als_weighting(bits, 1, 16);
                cd.weighting[1] = Self::als_weighting(bits, 2, 14);
                cd.weighting[2] = Self::als_weighting(bits, 1, 16);

                if cd.time_diff_flag {
                    cd.weighting[3] = Self::als_weighting(bits, 1, 16);
                    cd.weighting[4] = Self::als_weighting(bits, 1, 16);
                    cd.weighting[5] = Self::als_weighting(bits, 1, 16);

                    cd.time_diff_sign = bits.get1() != 0;
                    cd.time_diff_index = bits.get(self.ltp_lag_length - 3) as i32 + 3;
                }
            }

            entries += 1;
        }

        if entries == channels {
            return decode_error("als: damaged channel data");
        }

        bits.align();
        Ok(())
    }

    /// Recursively revert the inter-channel correlation of channel `c` for a block.
    fn revert_channel_correlation(&mut self, len: usize, offset: usize, c: usize) -> Result<()> {
        let channels = self.cfg.channels;
        let total = (channels * self.channel_size) as isize;

        if self.reverted[c] {
            return Ok(());
        }

        self.reverted[c] = true;

        let mut dep = 0;
        while dep < channels && !self.chan_data[c * channels + dep].stop_flag {
            let master = self.chan_data[c * channels + dep].master_channel;
            // The reference ignores the result of the recursion.
            let _ = self.revert_channel_correlation(len, offset, master);
            dep += 1;
        }

        if dep == channels {
            return decode_error("als: invalid channel correlation");
        }

        let dst = self.chan_base(c) + offset;

        for dep in 0..channels {
            let ch = self.chan_data[c * channels + dep];

            if ch.stop_flag {
                break;
            }

            if ch.master_channel == c {
                continue;
            }

            let master = (self.chan_base(ch.master_channel) + offset) as isize;
            let mut begin = 1isize;
            let mut end = len as isize - 1;

            if ch.time_diff_flag {
                let mut t = ch.time_diff_index as isize;

                if ch.time_diff_sign {
                    t = -t;
                    if begin < t {
                        return decode_error("als: time difference out of range");
                    }
                    begin -= t;
                }
                else {
                    if end < t {
                        return decode_error("als: time difference out of range");
                    }
                    end -= t;
                }

                if master + (begin - 1).min(begin - 1 + t) < 0
                    || master + (end + 1).max(end + 1 + t) > total
                {
                    return decode_error("als: channel correlation out of range");
                }

                let w = ch.weighting;

                for smp in begin..end {
                    let at = |i: isize| i64::from(self.raw[(master + i) as usize]);

                    let y = (1i64 << 6)
                        .wrapping_add(i64::from(w[0]) * at(smp - 1))
                        .wrapping_add(i64::from(w[1]) * at(smp))
                        .wrapping_add(i64::from(w[2]) * at(smp + 1))
                        .wrapping_add(i64::from(w[3]) * at(smp - 1 + t))
                        .wrapping_add(i64::from(w[4]) * at(smp + t))
                        .wrapping_add(i64::from(w[5]) * at(smp + 1 + t));

                    let d = dst + smp as usize;
                    self.raw[d] = self.raw[d].wrapping_add((y >> 7) as i32);
                }
            }
            else {
                if master + begin - 1 < 0 || master + end + 1 > total {
                    return decode_error("als: channel correlation out of range");
                }

                let w = ch.weighting;

                for smp in begin..end {
                    let at = |i: isize| i64::from(self.raw[(master + i) as usize]);

                    let y = (1i64 << 6)
                        .wrapping_add(i64::from(w[0]) * at(smp - 1))
                        .wrapping_add(i64::from(w[1]) * at(smp))
                        .wrapping_add(i64::from(w[2]) * at(smp + 1));

                    let d = dst + smp as usize;
                    self.raw[d] = self.raw[d].wrapping_add((y >> 7) as i32);
                }
            }
        }

        Ok(())
    }

    /// Read the difference signal of floating-point streams and reconstruct the floats, which
    /// replace the integer samples in the decoded sample buffer.
    fn read_diff_float_data(&mut self, bits: &mut Bits<'_>, ra_frame: bool) -> Result<()> {
        let channels = self.cfg.channels;
        let frame_length = self.cur_frame_length;

        let (Some(mlz), true) = (self.mlz.as_mut(), self.larray.len() >= frame_length * 4)
        else {
            return decode_error("als: no floating-point state");
        };

        bits.skip(32); // num_bytes_diff_float
        let use_acf = bits.get1() != 0;

        if ra_frame {
            self.last_acf_mantissa.fill(0);
            self.last_shift_value.fill(0);
            mlz.flush_dict();
        }

        if (channels * 8) as i64 > bits.left() {
            return decode_error("als: out of bits");
        }

        for c in 0..channels {
            let base = c * self.channel_size + self.cfg.max_order;

            self.acf[c] = if use_acf {
                // The acf_flag.
                let tmp = if bits.get1() != 0 {
                    let tmp = bits.get(23);
                    self.last_acf_mantissa[c] = tmp as i32;
                    tmp
                }
                else {
                    self.last_acf_mantissa[c] as u32
                };
                SoftFloat::from_bits(tmp)
            }
            else {
                FLOAT_1
            };

            let acf_is_one = self.acf[c].equals(FLOAT_1);

            let highest_byte = bits.get(2);
            let part_a_flag = bits.get1() != 0;
            let shift_amp = bits.get1() != 0;

            if shift_amp {
                self.shift_value[c] = bits.get(8) as i32;
                self.last_shift_value[c] = self.shift_value[c];
            }
            else {
                self.shift_value[c] = self.last_shift_value[c];
            }

            let samples = &self.raw[base..base + frame_length];
            let mantissa = &mut self.raw_mantissa[c];

            if part_a_flag {
                if bits.get1() == 0 {
                    // Uncompressed.
                    for i in 0..frame_length {
                        if samples[i] == 0 {
                            mantissa[i] = bits.get(32) as i32;
                        }
                    }
                }
                else {
                    // Compressed.
                    let nchars = 4 * samples.iter().filter(|&&s| s == 0).count();

                    if mlz.decompress(bits, nchars, &mut self.larray) != nchars {
                        return decode_error("als: error in MLZ decompression");
                    }

                    let mut j = 0;
                    for i in 0..frame_length {
                        if samples[i] == 0 {
                            mantissa[i] = u32::from_be_bytes([
                                self.larray[j],
                                self.larray[j + 1],
                                self.larray[j + 2],
                                self.larray[j + 3],
                            ]) as i32;
                            j += 4;
                        }
                    }
                }
            }

            // Part B.
            if highest_byte != 0 {
                for i in 0..frame_length {
                    if samples[i] != 0 {
                        // This follows Tables 14.45 and 14.46 of the specification.
                        self.nbits[i] = if acf_is_one {
                            let nbit = 31 - samples[i].unsigned_abs().leading_zeros() as i32;
                            if nbit > 23 {
                                return decode_error("als: invalid floating-point sample");
                            }
                            23 - nbit
                        }
                        else {
                            23
                        };

                        self.nbits[i] = self.nbits[i].min(highest_byte as i32 * 8);
                    }
                }

                if bits.get1() == 0 {
                    // Uncompressed.
                    for i in 0..frame_length {
                        if samples[i] != 0 {
                            mantissa[i] = bits.get(self.nbits[i] as u32) as i32;
                        }
                    }
                }
                else {
                    // Compressed.
                    let mut nchars = 0;
                    for i in 0..frame_length {
                        if samples[i] != 0 {
                            nchars += (self.nbits[i] / 8) as usize;
                            if self.nbits[i] & 7 != 0 {
                                nchars += 1;
                            }
                        }
                    }

                    if mlz.decompress(bits, nchars, &mut self.larray) != nchars {
                        return decode_error("als: error in MLZ decompression");
                    }

                    let mut j = 0;
                    for i in 0..frame_length {
                        if samples[i] != 0 {
                            let nbits = self.nbits[i];
                            let nbits_aligned =
                                if nbits & 7 != 0 { 8 * (nbits / 8 + 1) } else { nbits };

                            let mut acc = 0u64;
                            for _ in 0..nbits_aligned / 8 {
                                acc = (acc << 8) + u64::from(self.larray[j]);
                                j += 1;
                            }
                            acc >>= nbits_aligned - nbits;
                            mantissa[i] = acc as i32;
                        }
                    }
                }
            }

            for i in 0..frame_length {
                let pcm = self.raw[base + i];

                self.raw[base + i] = if pcm != 0 {
                    reconstruct(pcm, self.acf[c], mantissa[i], self.shift_value[c]) as i32
                }
                else {
                    mantissa[i]
                };
            }

            bits.align();
        }

        Ok(())
    }

    fn read_frame_data(&mut self, bits: &mut Bits<'_>, ra_frame: bool) -> Result<()> {
        let channels = self.cfg.channels;
        let mut bs_info = 0u32;

        // Skip the size of the random access unit if present in the frame.
        if self.cfg.ra_flag == 1 && ra_frame {
            bits.skip(32);
        }

        if self.cfg.mc_coding && self.cfg.joint_stereo {
            self.js_switch = bits.get1() != 0;
            bits.align();
        }

        if !self.cfg.mc_coding || self.js_switch {
            let mut independent_bs = u32::from(!self.cfg.joint_stereo);

            if bits.left() < channels as i64 {
                return decode_error("als: out of bits");
            }

            let mut c = 0;
            while c < channels {
                self.get_block_sizes(bits, &mut bs_info);

                // If joint stereo and block switching are set, independent decoding is signalled
                // via the first bit of bs_info.
                if self.cfg.joint_stereo && self.cfg.block_switching != 0 && bs_info >> 31 != 0 {
                    independent_bs = 2;
                }

                // The last channel has to be decoded independently.
                if c == channels - 1 || (c & 1) != 0 {
                    independent_bs = 1;
                }

                if independent_bs != 0 {
                    self.decode_blocks_ind(bits, ra_frame, c)?;
                    independent_bs -= 1;
                }
                else {
                    self.decode_blocks(bits, ra_frame, c)?;
                    c += 1;
                }

                // Store the carryover samples.
                self.store_carryover(c);
                c += 1;
            }
        }
        else {
            // Multi-channel coding.
            self.reverted.fill(false);

            let mut bd = Block { len: 0, ra: ra_frame, ci: 0, js: false, pos: 0, other: None };
            let mut offset = 0;

            self.get_block_sizes(bits, &mut bs_info);

            for b in 0..self.num_blocks {
                bd.len = self.div_blocks[b] as usize;
                if bd.len == 0 {
                    continue;
                }

                for c in 0..channels {
                    bd.ci = c;
                    bd.pos = self.chan_base(c) + offset;
                    bd.other = None;

                    self.read_block(bits, &mut bd)?;
                    self.read_channel_data(bits, c)?;
                }

                for c in 0..channels {
                    self.revert_channel_correlation(bd.len, offset, c)?;
                }

                for c in 0..channels {
                    bd.ci = c;
                    bd.pos = self.chan_base(c) + offset;

                    self.decode_block(&bd);
                }

                self.reverted.fill(false);
                offset += bd.len;
                bd.ra = false;
            }

            // Store the carryover samples.
            for c in 0..channels {
                self.store_carryover(c);
            }
        }

        if self.cfg.floating {
            self.read_diff_float_data(bits, ra_frame)?;
        }

        if bits.left() < 0 {
            return decode_error("als: frame overread");
        }

        Ok(())
    }

    /// Append the decoded frame to the planar sample accumulators of the packet.
    fn push_frame(&mut self) {
        let channels = self.cfg.channels;
        let n = self.cur_frame_length;
        let channel_size = self.channel_size;
        let max_order = self.cfg.max_order;
        let float = self.cfg.floating;
        // The samples are scaled to the width of the output sample format.
        let shift = match &self.buf {
            Output::S16(_) => 16 - self.cfg.bits_per_sample,
            Output::S32(_) if !float => 32 - self.cfg.bits_per_sample,
            _ => 0,
        };

        for c in 0..channels {
            let ch = self.cfg.chan_pos.as_ref().map_or(c, |p| p[c]);
            let src = &self.raw[ch * channel_size + max_order..ch * channel_size + max_order + n];
            self.acc[c].extend(src.iter().map(|&s| ((s as u32) << shift) as i32));
        }
    }

    /// Fill the output buffer with the frames accumulated for the packet.
    fn fill_output(&mut self) {
        let channels = self.cfg.channels;
        let n = self.acc.first().map_or(0, Vec::len);
        let spec = AudioSpec::new(
            self.cfg.sample_rate,
            self.params.channels.clone().unwrap_or(Channels::Discrete(channels as u16)),
        );

        match &mut self.buf {
            Output::S16(buf) => {
                if buf.capacity() < n {
                    *buf = AudioBuffer::new(spec, n);
                }
                buf.clear();
                buf.resize_uninit(n);
                for c in 0..channels {
                    if let Some(plane) = buf.plane_mut(c) {
                        for (d, s) in plane.iter_mut().zip(&self.acc[c]) {
                            *d = *s as i16;
                        }
                    }
                }
            }
            Output::S32(buf) => {
                if buf.capacity() < n {
                    *buf = AudioBuffer::new(spec, n);
                }
                buf.clear();
                buf.resize_uninit(n);
                for c in 0..channels {
                    if let Some(plane) = buf.plane_mut(c) {
                        plane.copy_from_slice(&self.acc[c]);
                    }
                }
            }
            Output::F32(buf) => {
                if buf.capacity() < n {
                    *buf = AudioBuffer::new(spec, n);
                }
                buf.clear();
                buf.resize_uninit(n);
                for c in 0..channels {
                    if let Some(plane) = buf.plane_mut(c) {
                        for (d, s) in plane.iter_mut().zip(&self.acc[c]) {
                            *d = f32::from_bits(*s as u32);
                        }
                    }
                }
            }
        }
    }

    /// Update the CRC with the decoded frame, and check it after the last frame.
    fn update_crc(&mut self) {
        let Some(crc) = self.crc.as_mut()
        else {
            return;
        };

        let channels = self.cfg.channels;
        let n = self.cur_frame_length;
        let bytes = (self.cfg.bits_per_sample / 8) as usize;
        let channel_size = self.channel_size;
        let max_order = self.cfg.max_order;

        for i in 0..n {
            for c in 0..channels {
                let ch = self.cfg.chan_pos.as_ref().map_or(c, |p| p[c]);
                let sample = self.raw[ch * channel_size + max_order + i] as u32;

                for k in 0..bytes {
                    let k = if self.cfg.msb_first { bytes - 1 - k } else { k };
                    crc.update((sample >> (8 * k)) as u8);
                }
            }
        }

        crc.samples += n as u64;

        if self.cfg.samples != u32::MAX && crc.samples == u64::from(self.cfg.samples) {
            self.verify_ok = Some(self.cfg.crc == Some(!crc.value));
        }
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        let mut bits = Bits::new(packet.data);

        self.acc.iter_mut().for_each(Vec::clear);

        // A packet holds one frame, or all frames of the stream if it has no random access
        // units (as written by some MP4 muxers), and the frames are byte aligned. FFmpeg decodes
        // the data of a packet frame by frame as well.
        loop {
            if let Err(err) = self.decode_frame(&mut bits) {
                // A damaged or cut-off frame ends the packet: keep the frames before it.
                if self.acc.first().is_none_or(Vec::is_empty) {
                    return Err(err);
                }

                warn!("als: {err}; dropping the rest of the packet");
                break;
            }

            // Without a known length of the stream the frame count of a packet is not known.
            if self.cfg.samples == u32::MAX || bits.left() < 8 {
                break;
            }

            let frame_length = self.cfg.frame_length as u64;
            if self.frame_id.wrapping_mul(frame_length) >= u64::from(self.cfg.samples) {
                break;
            }
        }

        self.fill_output();

        if self.opts.gapless {
            let n = self.acc.first().map_or(0, Vec::len);
            let start = (packet.trim_start.get() as usize).min(n);
            let end = (packet.trim_end.get() as usize).min(n - start);

            match &mut self.buf {
                Output::S16(b) => b.trim(start, end),
                Output::S32(b) => b.trim(start, end),
                Output::F32(b) => b.trim(start, end),
            }
        }

        Ok(())
    }

    fn decode_frame(&mut self, bits: &mut Bits<'_>) -> Result<()> {
        // If the distance between random access frames is zero, no frame is treated as a random
        // access frame. For the first frame, if prediction is used, all samples used from the
        // previous frame are assumed to be zero.
        let ra_frame =
            self.cfg.ra_distance != 0 && self.frame_id % u64::from(self.cfg.ra_distance) == 0;

        // The last frame to decode might have a different length.
        let frame_length = self.cfg.frame_length as u64;
        self.cur_frame_length = if self.cfg.samples != u32::MAX {
            u64::from(self.cfg.samples)
                .saturating_sub(self.frame_id.saturating_mul(frame_length))
                .min(frame_length) as usize
        }
        else {
            self.cfg.frame_length
        };

        self.frame_id += 1;

        if self.cur_frame_length == 0 {
            return decode_error("als: frame beyond the end of the stream");
        }

        self.read_frame_data(bits, ra_frame)?;
        bits.align();

        self.push_frame();
        self.update_crc();

        Ok(())
    }

    fn clear_buf(&mut self) {
        match &mut self.buf {
            Output::S16(b) => b.clear(),
            Output::S32(b) => b.clear(),
            Output::F32(b) => b.clear(),
        }
    }
}

impl AudioDecoder for AlsDecoder {
    fn reset(&mut self) {
        // The frames are numbered from the start of the stream.
        self.frame_id = 0;
        self.js_switch = false;
        // The history of the previous frame is not that of the next one.
        self.raw.fill(0);
        self.prev_raw.fill(0);
        // The CRC can only be checked when decoding from the start.
        self.crc = None;
        self.clear_buf();
    }

    fn codec_info(&self) -> &CodecInfo {
        // Only one codec is supported.
        &Self::supported_codecs().first().expect("at least one codec registered").info
    }

    fn codec_params(&self) -> &AudioCodecParameters {
        &self.params
    }

    fn decode_ref(&mut self, packet: &PacketRef<'_>) -> Result<GenericAudioBufferRef<'_>> {
        if let Err(e) = self.decode_inner(packet) {
            self.clear_buf();
            return Err(e);
        }

        Ok(self.last_decoded())
    }

    fn finalize(&mut self) -> FinalizeResult {
        FinalizeResult { verify_ok: self.verify_ok }
    }

    fn last_decoded(&self) -> GenericAudioBufferRef<'_> {
        match &self.buf {
            Output::S16(b) => b.as_generic_audio_buffer_ref(),
            Output::S32(b) => b.as_generic_audio_buffer_ref(),
            Output::F32(b) => b.as_generic_audio_buffer_ref(),
        }
    }
}

impl RegisterableAudioDecoder for AlsDecoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(AlsDecoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(CODEC_ID_ALS, "als", "MPEG-4 Audio Lossless Coding (ALS)")]
    }
}
