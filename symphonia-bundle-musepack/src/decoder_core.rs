// Symphonia Musepack demuxer+decoder
// Copyright (c) 2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The core (format-agnostic) Musepack decoder state machine: SV7/SV8 bitstream parsing,
//! requantization and the frame decode driver.
//!
//! Ported from libmpcdec `mpc_decoder.c` (BSD-3-Clause), see `NOTICE`. Function names below
//! mirror the C names (`read_bitstream_sv7`/`read_bitstream_sv8`/`requantize`/`decode_frame`) so
//! this file can be diffed against the reference frame-by-frame.

use crate::bits::{can_dec, huff_dec, BitReader};
use crate::cnk::{enum_dec, log_dec};
use crate::huffman::{sv7_tables, sv8_tables};
use crate::requant;
use crate::synth;

/// Samples per Musepack frame (`MPC_FRAME_LENGTH`).
pub const FRAME_LENGTH: usize = 36 * 32;
/// Synthesis filter group delay, in samples (`MPC_DECODER_SYNTH_DELAY`).
pub const SYNTH_DELAY: u32 = 481;

const MAX_BANDS: usize = 32;

#[inline]
fn absi(x: i32) -> i32 {
    x.wrapping_abs()
}

/// Result of decoding one Musepack frame.
#[allow(dead_code)] // `bits`/`end_of_stream` are diagnostic (oracle-diff/test) fields.
pub struct FrameResult {
    /// Number of sample-frames written to the output buffer (after `samples_to_skip` trimming).
    /// `0` with `end_of_stream == false` means the whole frame was consumed by skipping.
    pub samples: usize,
    /// Number of bits consumed from the bitstream by this frame (informational).
    pub bits: u64,
    /// Set when the decoder determined the stream has no more samples (SV7 only; SV8 end-of-
    /// stream is signalled by the demuxer's `SE` block instead).
    pub end_of_stream: bool,
}

/// The core Musepack decoder state. Shared between SV7 and SV8; instantiate once per logical
/// stream and reuse across all packets (`decode_frame` mutates persistent history: the
/// scale-factor deltas, `last_max_band`, `DSCF_Flag`, and the synthesis filter's `V` buffers).
pub struct Decoder {
    pub stream_version: u32,
    pub max_band: i32,
    pub ms: bool,
    pub channels: u32,

    pub samples: u64,
    pub decoded_samples: u64,
    pub samples_to_skip: u32,
    last_max_band: i32,

    r1: u32,
    r2: u32,

    scf_index_l: [[i32; 3]; MAX_BANDS],
    scf_index_r: [[i32; 3]; MAX_BANDS],
    q_l: [[i16; 36]; MAX_BANDS],
    q_r: [[i16; 36]; MAX_BANDS],
    res_l: [i32; MAX_BANDS],
    res_r: [i32; MAX_BANDS],
    scfi_l: [i32; MAX_BANDS],
    scfi_r: [i32; MAX_BANDS],
    dscf_flag_l: [bool; MAX_BANDS],
    dscf_flag_r: [bool; MAX_BANDS],
    ms_flag: [bool; MAX_BANDS],

    v_l: Vec<f32>,
    v_r: Vec<f32>,
    y_l: [[f32; 32]; 36],
    y_r: [[f32; 32]; 36],
    scf: [f32; 256],
}

impl Decoder {
    /// Ported from libmpcdec `mpc_decoder_setup` + `mpc_decoder_set_streaminfo` +
    /// `mpc_decoder_init_quant(d, 1.0)`.
    pub fn new(stream_version: u32, max_band: i32, ms: bool, channels: u32) -> Self {
        Decoder {
            stream_version,
            max_band: max_band.clamp(0, MAX_BANDS as i32 - 1),
            ms,
            channels: channels.clamp(1, 2),
            samples: 0,
            decoded_samples: 0,
            samples_to_skip: SYNTH_DELAY,
            last_max_band: 0,
            r1: 1,
            r2: 1,
            scf_index_l: [[0; 3]; MAX_BANDS],
            scf_index_r: [[0; 3]; MAX_BANDS],
            q_l: [[0; 36]; MAX_BANDS],
            q_r: [[0; 36]; MAX_BANDS],
            res_l: [0; MAX_BANDS],
            res_r: [0; MAX_BANDS],
            scfi_l: [0; MAX_BANDS],
            scfi_r: [0; MAX_BANDS],
            dscf_flag_l: [false; MAX_BANDS],
            dscf_flag_r: [false; MAX_BANDS],
            ms_flag: [false; MAX_BANDS],
            v_l: vec![0.0; synth::V_BUF_LEN],
            v_r: vec![0.0; synth::V_BUF_LEN],
            y_l: [[0.0; 32]; 36],
            y_r: [[0.0; 32]; 36],
            scf: requant::build_scf_table(1.0),
        }
    }

    /// Sets the total sample count and initial skip (`mpc_decoder_set_streaminfo`), given the
    /// stream's declared sample count and (SV8) leading silence / (SV7) gapless padding already
    /// folded in by the caller (see `demuxer::sv7`/`demuxer::sv8`).
    pub fn set_total_samples(&mut self, samples: u64, beg_silence: u64) {
        self.samples = samples;
        self.samples_to_skip = SYNTH_DELAY + (beg_silence.min(u64::from(u32::MAX)) as u32);
    }

    /// Resets seek-sensitive persistent state (used for SV7's `mpc_decoder_reset_scf`). Does
    /// *not* reset the synthesis filter history, matching the reference (see NOTICE-adjacent
    /// discussion of seek pre-roll in `demuxer/sv7.rs`/`demuxer/sv8.rs`).
    pub fn reset_scf(&mut self, value: i32) {
        for b in self.scf_index_l.iter_mut().chain(self.scf_index_r.iter_mut()) {
            *b = [value; 3];
        }
    }

    // ------------------------------------------------------------------
    // SV7 bitstream
    // ------------------------------------------------------------------

    /// Ported from libmpcdec `mpc_decoder.c` (`mpc_decoder_read_bitstream_sv7`).
    fn read_bitstream_sv7(&mut self, r: &mut BitReader<'_>) {
        const IDX30: [i32; 27] = [
            -1, 0, 1, -1, 0, 1, -1, 0, 1, -1, 0, 1, -1, 0, 1, -1, 0, 1, -1, 0, 1, -1, 0, 1, -1, 0,
            1,
        ];
        const IDX31: [i32; 27] = [
            -1, -1, -1, 0, 0, 0, 1, 1, 1, -1, -1, -1, 0, 0, 0, 1, 1, 1, -1, -1, -1, 0, 0, 0, 1, 1,
            1,
        ];
        const IDX32: [i32; 27] = [
            -1, -1, -1, -1, -1, -1, -1, -1, -1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1,
            1,
        ];
        const IDX50: [i32; 25] = [
            -2, -1, 0, 1, 2, -2, -1, 0, 1, 2, -2, -1, 0, 1, 2, -2, -1, 0, 1, 2, -2, -1, 0, 1, 2,
        ];
        const IDX51: [i32; 25] = [
            -2, -2, -2, -2, -2, -1, -1, -1, -1, -1, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 2, 2, 2, 2, 2,
        ];

        let max_band = self.max_band.max(0) as usize;
        let mut max_used_band: i32 = 0;

        self.res_l[0] = r.read_bits(4) as i32;
        self.res_r[0] = r.read_bits(4) as i32;
        if !(self.res_l[0] == 0 && self.res_r[0] == 0) {
            if self.ms {
                self.ms_flag[0] = r.read_bit() != 0;
            }
            max_used_band = 1;
        }

        for n in 1..=max_band {
            let idx = huff_dec(r, &sv7_tables::HUFF_HDR);
            self.res_l[n] = if idx != 4 { self.res_l[n - 1] + idx } else { r.read_bits(4) as i32 };
            let idx = huff_dec(r, &sv7_tables::HUFF_HDR);
            self.res_r[n] = if idx != 4 { self.res_r[n - 1] + idx } else { r.read_bits(4) as i32 };
            if !(self.res_l[n] == 0 && self.res_r[n] == 0) {
                if self.ms {
                    self.ms_flag[n] = r.read_bit() != 0;
                }
                max_used_band = n as i32 + 1;
            }
        }

        let mub = max_used_band.clamp(0, MAX_BANDS as i32) as usize;

        for n in 0..mub {
            if self.res_l[n] != 0 {
                self.scfi_l[n] = huff_dec(r, &sv7_tables::HUFF_SCFI);
            }
            if self.res_r[n] != 0 {
                self.scfi_r[n] = huff_dec(r, &sv7_tables::HUFF_SCFI);
            }
        }

        for n in 0..mub {
            decode_dscf_sv7(r, self.res_l[n], self.scfi_l[n], &mut self.scf_index_l[n]);
            decode_dscf_sv7(r, self.res_r[n], self.scfi_r[n], &mut self.scf_index_r[n]);
        }

        for n in 0..mub {
            let res_l = self.res_l[n];
            let res_r = self.res_r[n];
            decode_quant_sv7(
                r,
                res_l,
                &mut self.q_l[n],
                &IDX30,
                &IDX31,
                &IDX32,
                &IDX50,
                &IDX51,
                &mut self.r1,
                &mut self.r2,
            );
            decode_quant_sv7(
                r,
                res_r,
                &mut self.q_r[n],
                &IDX30,
                &IDX31,
                &IDX32,
                &IDX50,
                &IDX51,
                &mut self.r1,
                &mut self.r2,
            );
        }
    }

    // ------------------------------------------------------------------
    // SV8 bitstream
    // ------------------------------------------------------------------

    /// Ported from libmpcdec `mpc_decoder.c` (`mpc_decoder_read_bitstream_sv8`).
    fn read_bitstream_sv8(&mut self, r: &mut BitReader<'_>, is_key_frame: bool) {
        const IDX50: [i8; 125] = build_idx_repeat5([-2, -1, 0, 1, 2]);
        const IDX51: [i8; 125] = build_idx_repeat_block([-2, -1, 0, 1, 2], 5);
        const IDX52: [i8; 125] = build_idx_repeat_block([-2, -1, 0, 1, 2], 25);
        const THRES: [u32; 9] = [0, 0, 3, 0, 0, 1, 3, 4, 8];
        #[rustfmt::skip]
        const HUFFQ2_VAR: [i8; 125] = [
            6, 5, 4, 5, 6, 5, 4, 3, 4, 5, 4, 3, 2, 3, 4, 5, 4, 3, 4, 5, 6, 5, 4, 5, 6, 5, 4, 3, 4,
            5, 4, 3, 2, 3, 4, 3, 2, 1, 2, 3, 4, 3, 2, 3, 4, 5, 4, 3, 4, 5, 4, 3, 2, 3, 4, 3, 2, 1,
            2, 3, 2, 1, 0, 1, 2, 3, 2, 1, 2, 3, 4, 3, 2, 3, 4, 5, 4, 3, 4, 5, 4, 3, 2, 3, 4, 3, 2,
            1, 2, 3, 4, 3, 2, 3, 4, 5, 4, 3, 4, 5, 6, 5, 4, 5, 6, 5, 4, 3, 4, 5, 4, 3, 2, 3, 4, 5,
            4, 3, 4, 5, 6, 5, 4, 5, 6,
        ];

        let max_band = self.max_band.max(0);

        let mut max_used_band = if is_key_frame {
            log_dec(r, (max_band + 1) as u32) as i32
        }
        else {
            let mut v = self.last_max_band + can_dec(r, &sv8_tables::CAN_BANDS);
            if v > 32 {
                v -= 33;
            }
            v
        };
        max_used_band = max_used_band.clamp(0, MAX_BANDS as i32);
        self.last_max_band = max_used_band;

        if max_used_band > 0 {
            let top = (max_used_band - 1) as usize;
            self.res_l[top] = can_dec(r, &sv8_tables::CAN_RES[0]);
            self.res_r[top] = can_dec(r, &sv8_tables::CAN_RES[0]);
            if self.res_l[top] > 15 {
                self.res_l[top] -= 17;
            }
            if self.res_r[top] > 15 {
                self.res_r[top] -= 17;
            }
            for n in (0..top).rev() {
                let sub = usize::from(self.res_l[n + 1] > 2);
                self.res_l[n] = can_dec(r, &sv8_tables::CAN_RES[sub]) + self.res_l[n + 1];
                if self.res_l[n] > 15 {
                    self.res_l[n] -= 17;
                }
                let sub = usize::from(self.res_r[n + 1] > 2);
                self.res_r[n] = can_dec(r, &sv8_tables::CAN_RES[sub]) + self.res_r[n + 1];
                if self.res_r[n] > 15 {
                    self.res_r[n] -= 17;
                }
            }

            if self.ms {
                let mub = max_used_band as usize;
                let tot = (0..mub).filter(|&n| self.res_l[n] != 0 || self.res_r[n] != 0).count()
                    as u32;
                let cnt = log_dec(r, tot);
                let mut tmp: u32 = 0;
                if cnt != 0 && cnt != tot {
                    tmp = enum_dec(r, cnt.min(tot - cnt), tot);
                }
                if cnt * 2 > tot {
                    tmp = !tmp;
                }
                for n in (0..mub).rev() {
                    if self.res_l[n] != 0 || self.res_r[n] != 0 {
                        self.ms_flag[n] = tmp & 1 != 0;
                        tmp >>= 1;
                    }
                }
            }
        }

        for n in max_used_band.max(0) as usize..=max_band as usize {
            if n < MAX_BANDS {
                self.res_l[n] = 0;
                self.res_r[n] = 0;
            }
        }

        let mub = max_used_band as usize;

        if is_key_frame {
            self.dscf_flag_l = [true; MAX_BANDS];
            self.dscf_flag_r = [true; MAX_BANDS];
        }

        for n in 0..mub {
            let mut cnt: i32 = -1;
            if self.res_l[n] != 0 {
                cnt += 1;
            }
            if self.res_r[n] != 0 {
                cnt += 1;
            }
            if cnt >= 0 {
                let table = &sv8_tables::CAN_SCFI[cnt as usize];
                let tmp = can_dec(r, table);
                if self.res_l[n] != 0 {
                    self.scfi_l[n] = tmp >> (2 * cnt);
                }
                if self.res_r[n] != 0 {
                    self.scfi_r[n] = tmp & 3;
                }
            }
        }

        for n in 0..mub {
            decode_dscf_sv8(
                r,
                self.res_l[n],
                self.scfi_l[n],
                &mut self.dscf_flag_l[n],
                &mut self.scf_index_l[n],
            );
            decode_dscf_sv8(
                r,
                self.res_r[n],
                self.scfi_r[n],
                &mut self.dscf_flag_r[n],
                &mut self.scf_index_r[n],
            );
        }

        for n in 0..mub {
            let res_l = self.res_l[n];
            let res_r = self.res_r[n];
            decode_quant_sv8(
                r,
                res_l,
                &mut self.q_l[n],
                &IDX50,
                &IDX51,
                &IDX52,
                &THRES,
                &HUFFQ2_VAR,
                &mut self.r1,
                &mut self.r2,
            );
            decode_quant_sv8(
                r,
                res_r,
                &mut self.q_r[n],
                &IDX50,
                &IDX51,
                &IDX52,
                &THRES,
                &HUFFQ2_VAR,
                &mut self.r1,
                &mut self.r2,
            );
        }
    }

    // ------------------------------------------------------------------
    // Requantization
    // ------------------------------------------------------------------

    /// Ported from libmpcdec `mpc_decoder.c` (`mpc_decoder_requantisierung`).
    fn requantize(&mut self) {
        let last_band = self.max_band.clamp(0, MAX_BANDS as i32 - 1) as usize;

        for band in 0..=last_band {
            let ms = self.ms_flag[band];
            let res_l = self.res_l[band];
            let res_r = self.res_r[band];

            if ms {
                if res_l != 0 {
                    if res_r != 0 {
                        for third in 0..3 {
                            let fac_l =
                                requant::cc(res_l) * self.scf[requant::scf_index(self.scf_index_l[band][third])];
                            let fac_r =
                                requant::cc(res_r) * self.scf[requant::scf_index(self.scf_index_r[band][third])];
                            for j in 0..12 {
                                let n = third * 12 + j;
                                let templ = fac_l * f32::from(self.q_l[band][n]);
                                let tempr = fac_r * f32::from(self.q_r[band][n]);
                                self.y_l[n][band] = templ + tempr;
                                self.y_r[n][band] = templ - tempr;
                            }
                        }
                    }
                    else {
                        for third in 0..3 {
                            let fac_l =
                                requant::cc(res_l) * self.scf[requant::scf_index(self.scf_index_l[band][third])];
                            for j in 0..12 {
                                let n = third * 12 + j;
                                let v = fac_l * f32::from(self.q_l[band][n]);
                                self.y_l[n][band] = v;
                                self.y_r[n][band] = v;
                            }
                        }
                    }
                }
                else if res_r != 0 {
                    for third in 0..3 {
                        let fac_r =
                            requant::cc(res_r) * self.scf[requant::scf_index(self.scf_index_r[band][third])];
                        for j in 0..12 {
                            let n = third * 12 + j;
                            let v = fac_r * f32::from(self.q_r[band][n]);
                            self.y_l[n][band] = v;
                            self.y_r[n][band] = -v;
                        }
                    }
                }
                else {
                    for n in 0..36 {
                        self.y_l[n][band] = 0.0;
                        self.y_r[n][band] = 0.0;
                    }
                }
            }
            else if res_l != 0 {
                if res_r != 0 {
                    for third in 0..3 {
                        let fac_l =
                            requant::cc(res_l) * self.scf[requant::scf_index(self.scf_index_l[band][third])];
                        let fac_r =
                            requant::cc(res_r) * self.scf[requant::scf_index(self.scf_index_r[band][third])];
                        for j in 0..12 {
                            let n = third * 12 + j;
                            self.y_l[n][band] = fac_l * f32::from(self.q_l[band][n]);
                            self.y_r[n][band] = fac_r * f32::from(self.q_r[band][n]);
                        }
                    }
                }
                else {
                    for third in 0..3 {
                        let fac_l =
                            requant::cc(res_l) * self.scf[requant::scf_index(self.scf_index_l[band][third])];
                        for j in 0..12 {
                            let n = third * 12 + j;
                            self.y_l[n][band] = fac_l * f32::from(self.q_l[band][n]);
                            self.y_r[n][band] = 0.0;
                        }
                    }
                }
            }
            else if res_r != 0 {
                for third in 0..3 {
                    let fac_r =
                        requant::cc(res_r) * self.scf[requant::scf_index(self.scf_index_r[band][third])];
                    for j in 0..12 {
                        let n = third * 12 + j;
                        self.y_l[n][band] = 0.0;
                        self.y_r[n][band] = fac_r * f32::from(self.q_r[band][n]);
                    }
                }
            }
            else {
                for n in 0..36 {
                    self.y_l[n][band] = 0.0;
                    self.y_r[n][band] = 0.0;
                }
            }
        }
        // Unused higher bands contribute nothing to the synthesis filter (Y_L/Y_R indices
        // `max_band+1..32` are never referenced since the filter loop only reads
        // `Y_L[n][0..=max_band]` via `synth_channel`'s fixed 32-wide subband window -- but the
        // subband window is always the full 32; zero any bands above `last_band` so stale data
        // from a stream that lowered `max_band` never leaks in.
        for band in (last_band + 1)..32 {
            for n in 0..36 {
                self.y_l[n][band] = 0.0;
                self.y_r[n][band] = 0.0;
            }
        }
    }

    // ------------------------------------------------------------------
    // Frame driver
    // ------------------------------------------------------------------

    /// Ported from libmpcdec `mpc_decoder.c` (`mpc_decoder_decode_frame`).
    ///
    /// Decodes exactly one Musepack frame from `r` (which must be positioned at the start of a
    /// frame). `out` must have room for `FRAME_LENGTH * channels` interleaved `f32` samples;
    /// on return, valid audio occupies `out[..result.samples * channels]` (already shifted to
    /// the front if leading samples were trimmed by `samples_to_skip`).
    pub fn decode_frame(
        &mut self,
        r: &mut BitReader<'_>,
        is_key_frame: bool,
        out: &mut [f32],
    ) -> FrameResult {
        let start_bit = r.bit_pos();

        let samples_left_signed =
            self.samples as i64 - self.decoded_samples as i64 + i64::from(SYNTH_DELAY);
        if samples_left_signed <= 0 && self.samples != 0 {
            return FrameResult { samples: 0, bits: 0, end_of_stream: true };
        }
        let mut samples_left = samples_left_signed.max(0) as u64;

        if self.stream_version >= 8 {
            self.read_bitstream_sv8(r, is_key_frame);
        }
        else {
            self.read_bitstream_sv7(r);
        }

        let channels = self.channels as usize;
        let frame_buf_len = FRAME_LENGTH * channels;
        // `synth_channel` writes the full 36*32 grid unconditionally; only run it when the
        // caller-provided buffer is actually large enough (it always should be, per this
        // method's contract, but malformed/undersized buffers must never cause a panic).
        if out.len() >= frame_buf_len
            && (self.samples_to_skip as u64) < (FRAME_LENGTH as u64 + u64::from(SYNTH_DELAY))
        {
            self.requantize();
            let buf = &mut out[..frame_buf_len];
            synth::synth_channel(&mut self.v_l, &self.y_l, buf, channels, 0);
            if channels > 1 {
                synth::synth_channel(&mut self.v_r, &self.y_r, buf, channels, 1);
            }
        }

        self.decoded_samples += FRAME_LENGTH as u64;

        if self.decoded_samples.saturating_sub(self.samples) < FRAME_LENGTH as u64
            && self.stream_version == 7
        {
            let mut last_frame_samples = r.read_bits(11);
            if self.decoded_samples == self.samples {
                if last_frame_samples == 0 {
                    last_frame_samples = FRAME_LENGTH as u32;
                }
                let delta = i64::from(last_frame_samples) - FRAME_LENGTH as i64;
                self.samples = (self.samples as i64 + delta).max(0) as u64;
                samples_left = (samples_left as i64 + delta).max(0) as u64;
            }
        }

        let mut n_samples = samples_left.min(FRAME_LENGTH as u64) as usize;

        if self.samples_to_skip != 0 {
            let skip = self.samples_to_skip as usize;
            if n_samples <= skip {
                self.samples_to_skip -= n_samples as u32;
                n_samples = 0;
            }
            else {
                let remaining = n_samples - skip;
                let end = (n_samples * channels).min(out.len());
                let start = (skip * channels).min(end);
                out.copy_within(start..end, 0);
                n_samples = remaining;
                self.samples_to_skip = 0;
            }
        }

        let bits = r.bit_pos().saturating_sub(start_bit);
        FrameResult { samples: n_samples, bits, end_of_stream: false }
    }
}

/// Ported from libmpcdec `mpc_decoder.c` (SV7 SCF/DSCF decode within `read_bitstream_sv7`).
fn decode_dscf_sv7(r: &mut BitReader<'_>, res: i32, scfi: i32, scf: &mut [i32; 3]) {
    if res == 0 {
        return;
    }
    match scfi {
        1 => {
            let idx = huff_dec(r, &sv7_tables::HUFF_DSCF);
            scf[0] = if idx != 8 { scf[2] + idx } else { r.read_bits(6) as i32 };
            let idx = huff_dec(r, &sv7_tables::HUFF_DSCF);
            scf[1] = if idx != 8 { scf[0] + idx } else { r.read_bits(6) as i32 };
            scf[2] = scf[1];
        }
        3 => {
            let idx = huff_dec(r, &sv7_tables::HUFF_DSCF);
            scf[0] = if idx != 8 { scf[2] + idx } else { r.read_bits(6) as i32 };
            scf[1] = scf[0];
            scf[2] = scf[1];
        }
        2 => {
            let idx = huff_dec(r, &sv7_tables::HUFF_DSCF);
            scf[0] = if idx != 8 { scf[2] + idx } else { r.read_bits(6) as i32 };
            scf[1] = scf[0];
            let idx = huff_dec(r, &sv7_tables::HUFF_DSCF);
            scf[2] = if idx != 8 { scf[1] + idx } else { r.read_bits(6) as i32 };
        }
        0 => {
            let idx = huff_dec(r, &sv7_tables::HUFF_DSCF);
            scf[0] = if idx != 8 { scf[2] + idx } else { r.read_bits(6) as i32 };
            let idx = huff_dec(r, &sv7_tables::HUFF_DSCF);
            scf[1] = if idx != 8 { scf[0] + idx } else { r.read_bits(6) as i32 };
            let idx = huff_dec(r, &sv7_tables::HUFF_DSCF);
            scf[2] = if idx != 8 { scf[1] + idx } else { r.read_bits(6) as i32 };
        }
        _ => return,
    }
    for v in scf.iter_mut() {
        if *v > 1024 {
            *v = 0x8080;
        }
    }
}

/// Ported from libmpcdec `mpc_decoder.c` (SV8 SCF/DSCF decode within `read_bitstream_sv8`).
fn decode_dscf_sv8(
    r: &mut BitReader<'_>,
    res: i32,
    scfi: i32,
    dscf_flag: &mut bool,
    scf: &mut [i32; 3],
) {
    if res == 0 {
        return;
    }
    if *dscf_flag {
        scf[0] = r.read_bits(7) as i32 - 6;
        *dscf_flag = false;
    }
    else {
        let mut tmp = can_dec(r, &sv8_tables::CAN_DSCF[1]) as u32;
        if tmp == 64 {
            tmp += r.read_bits(6);
        }
        scf[0] = ((scf[2] - 25 + tmp as i32) & 127) - 6;
    }
    for m in 0..2usize {
        if ((scfi << m) & 2) == 0 {
            let mut tmp = can_dec(r, &sv8_tables::CAN_DSCF[0]) as u32;
            if tmp == 31 {
                tmp = 64 + r.read_bits(6);
            }
            scf[m + 1] = ((scf[m] - 25 + tmp as i32) & 127) - 6;
        }
        else {
            scf[m + 1] = scf[m];
        }
    }
}

/// Ported from libmpcdec `mpc_decoder.c` (SV7 sample decode within `read_bitstream_sv7`).
#[allow(clippy::too_many_arguments)]
fn decode_quant_sv7(
    r: &mut BitReader<'_>,
    res: i32,
    q: &mut [i16; 36],
    idx30: &[i32; 27],
    idx31: &[i32; 27],
    idx32: &[i32; 27],
    idx50: &[i32; 25],
    idx51: &[i32; 25],
    r1: &mut u32,
    r2: &mut u32,
) {
    match res {
        -1 => {
            for v in q.iter_mut() {
                let tmp = synth::random_int(r1, r2);
                let sum = ((tmp >> 24) & 0xFF) + ((tmp >> 16) & 0xFF) + ((tmp >> 8) & 0xFF) + (tmp & 0xFF);
                *v = (sum as i32 - 510) as i16;
            }
        }
        1 => {
            let sub = r.read_bits(1) as usize;
            let table = sv7_tables::huff_q(1, sub & 1);
            let mut k = 0usize;
            while k + 2 < 36 {
                let idx = huff_dec(r, table).clamp(0, 26) as usize;
                q[k] = idx30[idx] as i16;
                q[k + 1] = idx31[idx] as i16;
                q[k + 2] = idx32[idx] as i16;
                k += 3;
            }
        }
        2 => {
            let sub = r.read_bits(1) as usize;
            let table = sv7_tables::huff_q(2, sub & 1);
            let mut k = 0usize;
            while k + 1 < 36 {
                let idx = huff_dec(r, table).clamp(0, 24) as usize;
                q[k] = idx50[idx] as i16;
                q[k + 1] = idx51[idx] as i16;
                k += 2;
            }
        }
        3..=7 => {
            let sub = r.read_bits(1) as usize;
            let table = sv7_tables::huff_q(res as usize, sub & 1);
            for v in q.iter_mut() {
                *v = huff_dec(r, table) as i16;
            }
        }
        8..=17 => {
            let bits = u32::from(requant::RES_BIT[res as usize]);
            let bias = requant::dc(res);
            for v in q.iter_mut() {
                *v = (r.read_bits(bits) as i32 - bias) as i16;
            }
        }
        _ => {}
    }
}

const fn build_idx_repeat5(base: [i32; 5]) -> [i8; 125] {
    let mut out = [0i8; 125];
    let mut i = 0;
    while i < 125 {
        out[i] = base[i % 5] as i8;
        i += 1;
    }
    out
}

const fn build_idx_repeat_block(base: [i32; 5], block: usize) -> [i8; 125] {
    let mut out = [0i8; 125];
    let mut i = 0;
    while i < 125 {
        out[i] = base[(i / block) % 5] as i8;
        i += 1;
    }
    out
}

/// Ported from libmpcdec `mpc_decoder.c` (SV8 sample decode within `read_bitstream_sv8`).
#[allow(clippy::too_many_arguments)]
fn decode_quant_sv8(
    r: &mut BitReader<'_>,
    res: i32,
    q: &mut [i16; 36],
    idx50: &[i8; 125],
    idx51: &[i8; 125],
    idx52: &[i8; 125],
    thres: &[u32; 9],
    huffq2_var: &[i8; 125],
    r1: &mut u32,
    r2: &mut u32,
) {
    if res == 0 {
        return;
    }
    match res {
        2 => {
            let tables = [&sv8_tables::CAN_Q[0][0], &sv8_tables::CAN_Q[0][1]];
            let t = thres[2];
            let mut idx: u32 = 2 * t;
            let mut k = 0usize;
            while k + 2 < 36 {
                let sel = usize::from(idx > t);
                let tmp = can_dec(r, tables[sel]).clamp(0, 124) as usize;
                q[k] = i16::from(idx50[tmp]);
                q[k + 1] = i16::from(idx51[tmp]);
                q[k + 2] = i16::from(idx52[tmp]);
                idx = (idx >> 1).wrapping_add(huffq2_var[tmp] as i32 as u32);
                k += 3;
            }
        }
        1 => {
            let mut k = 0usize;
            while k < 36 {
                let kmax = (k + 18).min(36);
                let cnt = can_dec(r, &sv8_tables::CAN_Q1).clamp(0, 18) as u32;
                let mut idx: u32 = 0;
                if cnt > 0 && cnt < 18 {
                    idx = enum_dec(r, if cnt <= 9 { cnt } else { 18 - cnt }, 18);
                }
                if cnt > 9 {
                    idx = !idx;
                }
                while k < kmax {
                    q[k] = 0;
                    if idx & (1 << 17) != 0 {
                        q[k] = ((r.read_bits(1) << 1) as i32 - 1) as i16;
                    }
                    idx <<= 1;
                    k += 1;
                }
            }
        }
        -1 => {
            for v in q.iter_mut() {
                let tmp = synth::random_int(r1, r2);
                let sum = ((tmp >> 24) & 0xFF) + ((tmp >> 16) & 0xFF) + ((tmp >> 8) & 0xFF) + (tmp & 0xFF);
                *v = (sum as i32 - 510) as i16;
            }
        }
        3 | 4 => {
            let table = &sv8_tables::CAN_Q[1][(res - 3) as usize];
            let mut k = 0usize;
            while k + 1 < 36 {
                let sym = can_dec(r, table) as i8 as u8;
                let lo = (sym & 0x0F) as i8;
                let lo = if lo >= 8 { lo - 16 } else { lo };
                let hi = ((sym >> 4) & 0x0F) as i8;
                let hi = if hi >= 8 { hi - 16 } else { hi };
                q[k] = i16::from(lo);
                q[k + 1] = i16::from(hi);
                k += 2;
            }
        }
        5..=8 => {
            let row = (res - 3) as usize;
            let tables = [&sv8_tables::CAN_Q[row][0], &sv8_tables::CAN_Q[row][1]];
            let t = thres.get(res as usize).copied().unwrap_or(0);
            let mut idx: u32 = 2 * t;
            for v in q.iter_mut() {
                let sel = usize::from(idx > t);
                let sym = can_dec(r, tables[sel]);
                *v = sym as i16;
                idx = (idx >> 1).wrapping_add(absi(sym) as u32);
            }
        }
        _ => {
            // Res >= 9 (or a negative value other than -1, which cannot occur from a
            // well-formed stream but is handled here as a no-op for safety).
            if res < 9 {
                return;
            }
            for v in q.iter_mut() {
                let mut val = can_dec(r, &sv8_tables::CAN_Q9UP) as i32 as u8 as i32;
                if res != 9 {
                    let extra_bits = (res - 9).clamp(0, 16) as u32;
                    val = (val << extra_bits) | r.read_bits(extra_bits) as i32;
                }
                val -= requant::dc(res);
                *v = val as i16;
            }
        }
    }
}
