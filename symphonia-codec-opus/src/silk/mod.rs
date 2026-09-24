// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The SILK sub-decoder. Ported from libopus `silk/*.c` (fixed-point / `SILK_FIXED`, which is
//! what libopus uses even in float builds -- `silk_float` is only used by the SILK *encoder*'s
//! float variant, never the decoder). This module is kept strictly integer for bit-exactness
//! with the reference. Ported from libopus (BSD-3-Clause), see NOTICE.
//!
//! This file (`mod.rs`) implements the `silk_decoder`/`silk_Decode` entry point from
//! `silk/dec_API.c` directly (rather than a separate `dec_api.rs`), since it is the thin
//! multi-channel/resampling orchestration layer that ties every other module together.

mod bwexpander;
mod cng;
mod code_signs;
mod decode_core;
mod decode_frame;
mod decode_indices;
mod decode_parameters;
mod decode_pitch;
mod decode_pulses;
mod gain_quant;
mod lpc_analysis_filter;
mod lpc_fit;
mod lpc_inv_pred_gain;
mod macros;
mod nlsf2a;
mod nlsf_decode;
mod nlsf_stabilize;
mod nlsf_unpack;
mod plc;
mod resampler;
mod shell_coder;
mod sort;
mod stereo_decode_pred;
mod stereo_ms_to_lr;
mod structs;
mod sum_sqr_shift;
mod tables;
mod tables_nlsf;

use crate::range::RangeDecoder;
use crate::silk::decode_frame::{silk_decode_frame, LostFlag};
use crate::silk::stereo_decode_pred::{silk_stereo_decode_mid_only, silk_stereo_decode_pred};
use crate::silk::stereo_ms_to_lr::silk_stereo_ms_to_lr;
use crate::silk::structs::{
    SilkDecoderState, StereoDecState, CODE_CONDITIONALLY, CODE_INDEPENDENTLY, CODE_INDEPENDENTLY_NO_LTP_SCALING,
    MAX_FRAME_LENGTH, MAX_LPC_ORDER, MAX_SUB_FRAME_LENGTH, TYPE_NO_VOICE_ACTIVITY,
};
use crate::silk::tables::LBRR_FLAGS_ICDF_PTR;

/// C: `MAX_API_FS_KHZ` (`silk/define.h`, `48`).
const MAX_API_FS_KHZ: i32 = 48;

/// C: `FLAG_DECODE_NORMAL` / `FLAG_PACKET_LOST` / `FLAG_DECODE_LBRR`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeFlag {
    Normal,
    PacketLost,
    Lbrr,
}

/// C: `silk_DecControlStruct`. `internalSampleRate` and `prevPitchLag` are output (`O:`) fields
/// in the C struct; kept `pub` here since Rust has no in/out parameter distinction.
#[derive(Debug, Clone, Copy, Default)]
pub struct DecControl {
    pub n_channels_api: i32,
    pub n_channels_internal: i32,
    pub api_sample_rate: i32,
    /// O: internal sampling rate actually used (8000/12000/16000).
    pub internal_sample_rate: i32,
    pub payload_size_ms: i32,
    /// O: pitch lag of the previous frame (0 if unvoiced), in samples at 48 kHz.
    pub prev_pitch_lag: i32,
}

/// Decode errors specific to SILK (distinct from packet-framing errors).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SilkError {
    /// Mirrors libopus's few `SILK_*` internal error returns surfaced through `silk_Decode`.
    Internal,
}

pub type Result<T, E = SilkError> = std::result::Result<T, E>;

/// The SILK decoder state. C: the anonymous decoder object sized by
/// `silk_Get_Decoder_Size`/initialized by `silk_InitDecoder` (a `silk_decoder` wrapping one
/// `silk_decoder_state` per channel, per `silk/structs.h`).
pub struct SilkDecoder {
    channel_state: [SilkDecoderState; 2],
    s_stereo: StereoDecState,
    n_channels_api: i32,
    n_channels_internal: i32,
    prev_decode_only_middle: bool,
}

impl SilkDecoder {
    /// C: `silk_InitDecoder`.
    pub fn new() -> Self {
        SilkDecoder {
            channel_state: [SilkDecoderState::new(), SilkDecoderState::new()],
            s_stereo: StereoDecState::default(),
            n_channels_api: 0,
            n_channels_internal: 0,
            prev_decode_only_middle: false,
        }
    }

    /// C: `silk_ResetDecoder` (called on `OPUS_RESET_STATE` and internally on some mode
    /// transitions).
    pub fn reset(&mut self) {
        self.channel_state[0].reset();
        self.channel_state[1].reset();
        self.s_stereo = StereoDecState::default();
        self.prev_decode_only_middle = false;
    }

    /// C: `silk_Decode`. Decodes one SILK frame (or LBRR frame, or runs PLC) from `rd` into
    /// `out`, writing the number of samples produced to `n_samples_out`.
    ///
    /// `new_packet` mirrors the C `newPacketFlag` (`1` on the first `silk_Decode` call for a
    /// given transport packet, resetting internal per-packet frame counters).
    ///
    /// Per libopus 1.5.2, `silk_Decode`'s `samplesOut` buffer is `opus_int16`, NOT float -- the
    /// caller (`crate::decoder`, wave 2) is responsible for converting to `f32` alongside CELT's
    /// float output and/or hybrid mixing. `out` is therefore `&mut [i16]` here to match exactly.
    /// `out` must have room for `n_channels_api * (samples produced at `api_sample_rate`)`,
    /// interleaved if stereo.
    pub fn decode(
        &mut self,
        ctl: &mut DecControl,
        lost_flag: DecodeFlag,
        new_packet: bool,
        rd: &mut RangeDecoder<'_>,
        out: &mut [i16],
        n_samples_out: &mut usize,
    ) -> Result<()> {
        let n_channels_internal = ctl.n_channels_internal as usize;
        debug_assert!(n_channels_internal == 1 || n_channels_internal == 2);

        // Test if first frame in payload.
        if new_packet {
            for n in 0..n_channels_internal {
                self.channel_state[n].n_frames_decoded = 0;
            }
        }

        // If Mono -> Stereo transition in bitstream: init state of second channel.
        if ctl.n_channels_internal > self.n_channels_internal {
            self.channel_state[1] = SilkDecoderState::new();
        }

        let stereo_to_mono = ctl.n_channels_internal == 1
            && self.n_channels_internal == 2
            && (ctl.internal_sample_rate == 1000 * self.channel_state[0].fs_khz);

        if self.channel_state[0].n_frames_decoded == 0 {
            for n in 0..n_channels_internal {
                let (n_frames_per_packet, nb_subfr) = match ctl.payload_size_ms {
                    0 | 10 => (1, 4i32 / 2),
                    20 => (1, 4),
                    40 => (2, 4),
                    60 => (3, 4),
                    _ => return Err(SilkError::Internal),
                };
                self.channel_state[n].n_frames_per_packet = n_frames_per_packet;
                self.channel_state[n].nb_subfr = nb_subfr;

                let fs_khz_dec = (ctl.internal_sample_rate >> 10) + 1;
                if fs_khz_dec != 8 && fs_khz_dec != 12 && fs_khz_dec != 16 {
                    return Err(SilkError::Internal);
                }
                self.channel_state[n].decoder_set_fs(fs_khz_dec, ctl.api_sample_rate);
            }
        }

        if ctl.n_channels_api == 2
            && ctl.n_channels_internal == 2
            && (self.n_channels_api == 1 || self.n_channels_internal == 1)
        {
            self.s_stereo.pred_prev_q13 = [0; 2];
            self.s_stereo.s_side = [0; 2];
            self.channel_state[1].resampler_state = self.channel_state[0].resampler_state.clone();
        }
        self.n_channels_api = ctl.n_channels_api;
        self.n_channels_internal = ctl.n_channels_internal;

        if ctl.api_sample_rate > MAX_API_FS_KHZ * 1000 || ctl.api_sample_rate < 8000 {
            return Err(SilkError::Internal);
        }

        let mut ms_pred_q13 = [0i32; 2];
        let mut decode_only_middle = false;

        if lost_flag != DecodeFlag::PacketLost && self.channel_state[0].n_frames_decoded == 0 {
            // First decoder call for this payload: decode VAD flags and LBRR flag.
            for n in 0..n_channels_internal {
                for i in 0..self.channel_state[n].n_frames_per_packet as usize {
                    self.channel_state[n].vad_flags[i] = rd.dec_bit_logp(1);
                }
                self.channel_state[n].lbrr_flag = rd.dec_bit_logp(1);
            }
            // Decode LBRR flags.
            for n in 0..n_channels_internal {
                self.channel_state[n].lbrr_flags = [false; crate::silk::structs::MAX_FRAMES_PER_PACKET];
                if self.channel_state[n].lbrr_flag {
                    if self.channel_state[n].n_frames_per_packet == 1 {
                        self.channel_state[n].lbrr_flags[0] = true;
                    } else {
                        let lbrr_symbol =
                            rd.dec_icdf(LBRR_FLAGS_ICDF_PTR[self.channel_state[n].n_frames_per_packet as usize - 2], 8) + 1;
                        for i in 0..self.channel_state[n].n_frames_per_packet as usize {
                            self.channel_state[n].lbrr_flags[i] = ((lbrr_symbol >> i) & 1) != 0;
                        }
                    }
                }
            }

            if lost_flag == DecodeFlag::Normal {
                // Regular decoding: skip all LBRR data.
                for i in 0..self.channel_state[0].n_frames_per_packet as usize {
                    for n in 0..n_channels_internal {
                        if self.channel_state[n].lbrr_flags[i] {
                            let mut pulses = [0i16; MAX_FRAME_LENGTH];

                            if n_channels_internal == 2 && n == 0 {
                                ms_pred_q13 = silk_stereo_decode_pred_arr(rd);
                                if !self.channel_state[1].lbrr_flags[i] {
                                    decode_only_middle = silk_stereo_decode_mid_only(rd);
                                }
                            }
                            let cond_coding = if i > 0 && self.channel_state[n].lbrr_flags[i - 1] {
                                CODE_CONDITIONALLY
                            } else {
                                CODE_INDEPENDENTLY
                            };
                            decode_indices::silk_decode_indices(&mut self.channel_state[n], rd, i, true, cond_coding);
                            let l = self.channel_state[n].frame_length as usize;
                            let padded_l = (l + 15) & !15;
                            decode_pulses::silk_decode_pulses(
                                rd,
                                &mut pulses[..padded_l],
                                self.channel_state[n].indices.signal_type as i32,
                                self.channel_state[n].indices.quant_offset_type as i32,
                                l,
                            );
                        }
                    }
                }
            }
        }

        // Get MS predictor index.
        if n_channels_internal == 2 {
            let n_frames_decoded0 = self.channel_state[0].n_frames_decoded as usize;
            if lost_flag == DecodeFlag::Normal
                || (lost_flag == DecodeFlag::Lbrr && self.channel_state[0].lbrr_flags[n_frames_decoded0])
            {
                ms_pred_q13 = silk_stereo_decode_pred_arr(rd);
                if (lost_flag == DecodeFlag::Normal && !self.channel_state[1].vad_flags[n_frames_decoded0])
                    || (lost_flag == DecodeFlag::Lbrr && !self.channel_state[1].lbrr_flags[n_frames_decoded0])
                {
                    decode_only_middle = silk_stereo_decode_mid_only(rd);
                } else {
                    decode_only_middle = false;
                }
            } else {
                ms_pred_q13 = self.s_stereo.pred_prev_q13;
            }
        }

        // Reset side channel decoder prediction memory for first frame with side coding.
        if n_channels_internal == 2 && !decode_only_middle && self.prev_decode_only_middle {
            self.channel_state[1].out_buf = [0; MAX_FRAME_LENGTH + 2 * MAX_SUB_FRAME_LENGTH];
            self.channel_state[1].s_lpc_q14_buf = [0; MAX_LPC_ORDER];
            self.channel_state[1].lag_prev = 100;
            self.channel_state[1].last_gain_index = 10;
            self.channel_state[1].prev_signal_type = TYPE_NO_VOICE_ACTIVITY;
            self.channel_state[1].first_frame_after_reset = true;
        }

        let has_side = if lost_flag == DecodeFlag::Normal {
            !decode_only_middle
        } else {
            !self.prev_decode_only_middle
                || (n_channels_internal == 2
                    && lost_flag == DecodeFlag::Lbrr
                    && self.channel_state[1].lbrr_flags[self.channel_state[1].n_frames_decoded as usize])
        };

        // Call decoder for one frame.
        let mut samples_out1 = [[0i16; MAX_FRAME_LENGTH + 2]; 2];
        let mut n_samples_out_dec = 0usize;
        for n in 0..n_channels_internal {
            if n == 0 || has_side {
                let frame_index = self.channel_state[0].n_frames_decoded - n as i32;
                let cond_coding = if frame_index <= 0 {
                    CODE_INDEPENDENTLY
                } else if lost_flag == DecodeFlag::Lbrr {
                    if self.channel_state[n].lbrr_flags[frame_index as usize - 1] {
                        CODE_CONDITIONALLY
                    } else {
                        CODE_INDEPENDENTLY
                    }
                } else if n > 0 && self.prev_decode_only_middle {
                    CODE_INDEPENDENTLY_NO_LTP_SCALING
                } else {
                    CODE_CONDITIONALLY
                };

                let lost = match lost_flag {
                    DecodeFlag::PacketLost => LostFlag::PacketLost,
                    DecodeFlag::Lbrr => LostFlag::DecodeLbrr,
                    DecodeFlag::Normal => LostFlag::DecodeNormal,
                };
                n_samples_out_dec = silk_decode_frame(&mut self.channel_state[n], rd, &mut samples_out1[n][2..], lost, cond_coding);
            } else {
                for v in samples_out1[n][2..2 + n_samples_out_dec].iter_mut() {
                    *v = 0;
                }
            }
            self.channel_state[n].n_frames_decoded += 1;
        }

        if ctl.n_channels_api == 2 && ctl.n_channels_internal == 2 {
            // Convert Mid/Side to Left/Right.
            let (a, b) = samples_out1.split_at_mut(1);
            silk_stereo_ms_to_lr(
                &mut self.s_stereo,
                &mut a[0],
                &mut b[0],
                &ms_pred_q13,
                self.channel_state[0].fs_khz,
                n_samples_out_dec,
            );
        } else {
            // Buffering.
            samples_out1[0][0..2].copy_from_slice(&self.s_stereo.s_mid);
            self.s_stereo.s_mid.copy_from_slice(&samples_out1[0][n_samples_out_dec..n_samples_out_dec + 2]);
        }

        // Number of output samples.
        *n_samples_out =
            (n_samples_out_dec as i64 * ctl.api_sample_rate as i64 / (self.channel_state[0].fs_khz as i64 * 1000)) as usize;

        let n_channels_api = ctl.n_channels_api as usize;
        for n in 0..n_channels_internal.min(n_channels_api) {
            let mut resample_out = [0i16; (MAX_API_FS_KHZ as usize) * 20 + 4];
            let written = resampler::silk_resampler(
                &mut self.channel_state[n].resampler_state,
                &mut resample_out,
                &samples_out1[n][1..1 + n_samples_out_dec + 1],
                n_samples_out_dec as i32,
            ) as usize;
            debug_assert!(written <= *n_samples_out);

            if n_channels_api == 2 {
                for i in 0..*n_samples_out {
                    out[n + 2 * i] = resample_out[i];
                }
            } else {
                out[..*n_samples_out].copy_from_slice(&resample_out[..*n_samples_out]);
            }
        }

        // Create two channel output from mono stream.
        if n_channels_api == 2 && n_channels_internal == 1 {
            if stereo_to_mono {
                let mut resample_out = [0i16; (MAX_API_FS_KHZ as usize) * 20 + 4];
                let _ = resampler::silk_resampler(
                    &mut self.channel_state[1].resampler_state,
                    &mut resample_out,
                    &samples_out1[0][1..1 + n_samples_out_dec + 1],
                    n_samples_out_dec as i32,
                );
                for i in 0..*n_samples_out {
                    out[1 + 2 * i] = resample_out[i];
                }
            } else {
                for i in 0..*n_samples_out {
                    out[1 + 2 * i] = out[2 * i];
                }
            }
        }

        // Export pitch lag, measured at 48 kHz sampling rate.
        if self.channel_state[0].prev_signal_type == crate::silk::structs::TYPE_VOICED {
            const MULT_TAB: [i32; 3] = [6, 4, 3];
            ctl.prev_pitch_lag =
                self.channel_state[0].lag_prev * MULT_TAB[((self.channel_state[0].fs_khz - 8) >> 2) as usize];
        } else {
            ctl.prev_pitch_lag = 0;
        }

        if lost_flag == DecodeFlag::PacketLost {
            // On packet loss, remove the gain clamping to prevent having the energy "bounce
            // back" if we lose packets when the energy is going down.
            for i in 0..self.n_channels_internal as usize {
                self.channel_state[i].last_gain_index = 10;
            }
        } else {
            self.prev_decode_only_middle = decode_only_middle;
        }

        Ok(())
    }
}

/// Small adapter: [`silk_stereo_decode_pred`] already returns `[i32; 2]` directly; kept as a
/// named wrapper purely so the call sites above read like the C (`silk_stereo_decode_pred(rd,
/// MS_pred_Q13)`).
fn silk_stereo_decode_pred_arr(rd: &mut RangeDecoder<'_>) -> [i32; 2] {
    let mut pred = [0i32; 2];
    silk_stereo_decode_pred(rd, &mut pred);
    pred
}

impl Default for SilkDecoder {
    fn default() -> Self {
        Self::new()
    }
}
