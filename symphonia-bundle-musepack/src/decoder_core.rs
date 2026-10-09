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

/// The core Musepack decoder state. Shared between SV7 and SV8; instantiate once per logical
/// stream and reuse across all packets (`decode_frame` mutates persistent history: the
/// scale-factor deltas, `last_max_band`, `DSCF_Flag`, and the synthesis filter's `V` buffers).
pub struct Decoder {
    pub stream_version: u32,
    pub max_band: i32,
    pub ms: bool,
    pub channels: u32,

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

    /// Ported from libmpcdec `mpc_decoder.c` (`mpc_decoder_decode_frame`), minus its gapless
    /// bookkeeping (see below).
    ///
    /// Decodes exactly one Musepack frame from `r` (which must be positioned at the start of a
    /// frame). `out` must have room for `FRAME_LENGTH * channels` interleaved `f32` samples; the
    /// full frame is always written (`FRAME_LENGTH` sample-frames).
    ///
    /// The reference decoder also tracks the stream's total sample count and the encoder delay
    /// here (`samples`/`decoded_samples`/`samples_to_skip`) and trims the output itself. That
    /// state is position dependent, which makes it unusable after a seek, so it is expressed
    /// instead through `Packet::trim_start`/`trim_end`, computed by the demuxer from the very same
    /// quantities (see `demuxer::StreamInfo::skip_samples`) and applied by the `AudioDecoder`.
    ///
    /// Returns `false` (and writes nothing) if `out` is too small.
    pub fn decode_frame(
        &mut self,
        r: &mut BitReader<'_>,
        is_key_frame: bool,
        out: &mut [f32],
    ) -> bool {
        let channels = self.channels as usize;
        let frame_buf_len = FRAME_LENGTH * channels;
        // `synth_channel` writes the full 36*32 grid unconditionally; malformed/undersized
        // buffers must never cause a panic.
        if out.len() < frame_buf_len {
            return false;
        }

        if self.stream_version >= 8 {
            self.read_bitstream_sv8(r, is_key_frame);
        }
        else {
            self.read_bitstream_sv7(r);
        }

        self.requantize();
        let buf = &mut out[..frame_buf_len];
        synth::synth_channel(&mut self.v_l, &self.y_l, buf, channels, 0);
        if channels > 1 {
            synth::synth_channel(&mut self.v_r, &self.y_r, buf, channels, 1);
        }
        true
    }

    /// Parses one SV7 frame's bitstream without requantizing or synthesizing it, advancing only
    /// the state that carries over between frames (scale-factor deltas, noise generator).
    /// Used by the demuxer to recover that state at an arbitrary frame for seeking.
    pub fn skip_frame_sv7(&mut self, r: &mut BitReader<'_>) {
        self.read_bitstream_sv7(r);
    }

    /// SV8 counterpart of [`Decoder::skip_frame_sv7`]; `is_key_frame` is true for the first frame
    /// of a packet.
    pub fn skip_frame_sv8(&mut self, r: &mut BitReader<'_>, is_key_frame: bool) {
        self.read_bitstream_sv8(r, is_key_frame);
    }

    /// The noise-substitution generator state (`r1`, `r2`).
    pub fn noise_state(&self) -> [u32; 2] {
        [self.r1, self.r2]
    }

    /// Restores a state captured by [`Decoder::noise_state`].
    pub fn set_noise_state(&mut self, [r1, r2]: [u32; 2]) {
        self.r1 = r1;
        self.r2 = r2;
    }

    /// Snapshot of the SV7 inter-frame state (see [`Sv7Sync`]).
    pub fn sv7_sync(&self) -> Sv7Sync {
        Sv7Sync { scf_l: self.scf_index_l, scf_r: self.scf_index_r, r1: self.r1, r2: self.r2 }
    }

    /// Restores the SV7 inter-frame state captured by [`Decoder::sv7_sync`].
    pub fn set_sv7_sync(&mut self, sync: &Sv7Sync) {
        self.scf_index_l = sync.scf_l;
        self.scf_index_r = sync.scf_r;
        self.r1 = sync.r1;
        self.r2 = sync.r2;
    }
}

/// The SV7 decoder state that is *not* reconstructible from a single frame.
///
/// SV7 scale factors are delta-coded against the previous frame (`decode_dscf_sv7`), with no
/// periodic key frames, so decoding from an arbitrary frame requires knowing the scale factors
/// at that point (resetting them to a constant, as libmpcdec's seek does, leaves a gain error
/// that persists until each band happens to be re-coded with an escape). The noise-substitution
/// generator (`Res == -1`) is likewise sequential. Everything else read by
/// `read_bitstream_sv7` is rewritten by every frame.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Sv7Sync {
    scf_l: [[i32; 3]; MAX_BANDS],
    scf_r: [[i32; 3]; MAX_BANDS],
    r1: u32,
    r2: u32,
}

impl Sv7Sync {
    /// Serialized size in bytes, see [`Sv7Sync::write_to`].
    pub const ENCODED_LEN: usize = 2 * MAX_BANDS * 3 * 4 + 8;

    /// Appends the little-endian serialization of the state to `out`.
    pub fn write_to(&self, out: &mut Vec<u8>) {
        for band in self.scf_l.iter().chain(self.scf_r.iter()) {
            for v in band {
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
        out.extend_from_slice(&self.r1.to_le_bytes());
        out.extend_from_slice(&self.r2.to_le_bytes());
    }

    /// Parses the serialization written by [`Sv7Sync::write_to`]. Returns `None` if `data` is
    /// shorter than [`Sv7Sync::ENCODED_LEN`].
    pub fn read_from(data: &[u8]) -> Option<Self> {
        let data = data.get(..Self::ENCODED_LEN)?;
        let mut words = data.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]));
        let mut scf_l = [[0i32; 3]; MAX_BANDS];
        let mut scf_r = [[0i32; 3]; MAX_BANDS];
        for band in scf_l.iter_mut().chain(scf_r.iter_mut()) {
            for v in band.iter_mut() {
                *v = words.next()? as i32;
            }
        }
        let r1 = words.next()?;
        let r2 = words.next()?;
        Some(Sv7Sync { scf_l, scf_r, r1, r2 })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sv7_sync_round_trips() {
        let mut core = Decoder::new(7, 20, true, 2);
        // Garbage frame data exercises the scale-factor and noise-generator state.
        let data: Vec<u8> = (0..400u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
        for _ in 0..5 {
            core.skip_frame_sv7(&mut BitReader::new(&data));
        }
        let sync = core.sv7_sync();
        assert_ne!(sync, Decoder::new(7, 20, true, 2).sv7_sync());

        let mut bytes = Vec::new();
        sync.write_to(&mut bytes);
        assert_eq!(bytes.len(), Sv7Sync::ENCODED_LEN);
        assert_eq!(Sv7Sync::read_from(&bytes), Some(sync));
        assert_eq!(Sv7Sync::read_from(&bytes[..bytes.len() - 1]), None);
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
