// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! ITU-T G.729 decoder.
//!
//! G.729 is a CS-ACELP speech codec, 8 kHz mono, with 10 ms (80 sample) frames. This decoder
//! follows the FFmpeg `g729` decoder (see `NOTICE`) and implements:
//!
//! * G.729 at 8 kbit/s (10 byte frames), including the Annex A reduced-complexity variant: Annex A
//!   changes only the encoder, the bitstream and the decoder are the same.
//! * G.729 Annex D at 6.4 kbit/s (8 byte frames), which are recognised by a packet size of
//!   exactly 8 bytes.
//! * The postfilter (section 4.2), the high-pass filter, and concealment of erased frames. As in
//!   FFmpeg, a frame whose bytes are all zero is treated as erased.
//!
//! A packet holds one or more whole 10 byte frames, or a single 8 byte Annex D frame; any other
//! size (including the 2 byte SID frames and zero byte "untransmitted" frames of Annex B) is
//! rejected with a decode error, like FFmpeg. Annex B comfort noise generation is not supported.
//!
//! The arithmetic is the fixed-point arithmetic of FFmpeg's non bit-exact build, so the output
//! matches FFmpeg's decoder exactly.

#![warn(rust_2018_idioms)]
#![forbid(unsafe_code)]
// The following lints are allowed in all Symphonia crates. Please see clippy.toml for their
// justification.
#![allow(clippy::comparison_chain)]
#![allow(clippy::excessive_precision)]
#![allow(clippy::identity_op)]
#![allow(clippy::manual_range_contains)]
// Disable to better express the specification.
#![allow(clippy::collapsible_else_if)]
// The fixed-point routines mirror the reference implementation's signatures and index loops.
#![allow(clippy::too_many_arguments)]
#![allow(clippy::needless_range_loop)]

mod celp;
mod lsp;
mod postfilter;
mod tables;

use symphonia_core::audio::{
    AsGenericAudioBufferRef, Audio, AudioBuffer, AudioMut, AudioSpec, GenericAudioBufferRef,
    layouts,
};
use symphonia_core::codecs::CodecInfo;
use symphonia_core::codecs::audio::well_known::CODEC_ID_G729;
use symphonia_core::codecs::audio::{
    AudioCodecParameters, AudioDecoder, AudioDecoderOptions, FinalizeResult,
};
use symphonia_core::codecs::registry::{RegisterableAudioDecoder, SupportedAudioCodec};
use symphonia_core::errors::{Result, decode_error, unsupported_error};
use symphonia_core::io::{BitReaderLtr, ReadBitsLtr};
use symphonia_core::packet::PacketRef;
use symphonia_core::support_audio_codec;

use celp::{
    PITCH_DELAY_MAX, PITCH_DELAY_MIN, convolve_circ, decode_4bit_to_2nd_delay3,
    decode_5_6_bit_to_2nd_delay3, decode_8bit_to_1st_delay3, decode_gain_code, fc_pulse_per_track,
    high_pass_filter, interpolate_exc, lp_synthesis_filter, update_past_gain, weighted_sum,
};
use lsp::{LP_ORDER, lp_decode, lsf2lsp, reorder_lsf};
use postfilter::{Postfilter, adaptive_gain_control};
use tables::*;

/// Subframe size in samples.
pub(crate) const SUBFRAME_SIZE: usize = 40;
/// Samples per frame (two subframes).
const FRAME_SIZE: usize = 2 * SUBFRAME_SIZE;
/// Size in bytes of a G.729 frame at 8 kbit/s.
const G729_8K_BLOCK_SIZE: usize = 10;
/// Size in bytes of a G.729 Annex D frame at 6.4 kbit/s.
const G729D_6K4_BLOCK_SIZE: usize = 8;

/// Order of the moving average predictor of the LSP quantizer.
const MA_NP: usize = 4;
/// Bits of the first stage vector of the LSP quantizer.
const VQ_1ST_BITS: u32 = 7;
/// Bits of the second stage vectors of the LSP quantizer.
const VQ_2ND_BITS: u32 = 5;

/// Minimum quantized LSF value (3.2.4), 0.005 in Q13.
const LSFQ_MIN: i32 = 40;
/// Maximum quantized LSF value (3.2.4), 3.135 in Q13.
const LSFQ_MAX: i32 = 25681;
/// Minimum LSF distance (3.2.4), 0.0391 in Q13.
const LSFQ_DIFF_MIN: i32 = 321;

/// Interpolation filter length.
const INTERPOL_LEN: usize = 11;

/// Minimum gain pitch value (3.8, equation 47), 0.2 in (1.14).
const SHARP_MIN: i32 = 3277;
/// Maximum gain pitch value (3.8, equation 47).
///
/// This does not comply with the specification, which says 0.8 (13107 in (1.14)); the reference C
/// code, and FFmpeg after it, use 13017 (0.7945).
const SHARP_MAX: i32 = 13017;

/// Mean removed energy: `mean_energy + 10 * log10(2^26 * subframe_size)` in (7.13).
const MR_ENERGY: i32 = 1018156;

const DECISION_NOISE: i16 = 0;
const DECISION_INTERMEDIATE: i16 = 1;
const DECISION_VOICE: i16 = 2;

/// Length of the excitation history buffer.
const EXC_LEN: usize = 2 * SUBFRAME_SIZE + PITCH_DELAY_MAX as usize + INTERPOL_LEN;
/// Start of the current frame's excitation within the buffer.
const EXC_START: usize = PITCH_DELAY_MAX as usize + INTERPOL_LEN;

/// Number of frames the output buffer is initially sized for. Larger packets grow it.
const INITIAL_FRAMES: usize = 8;

/// The two frame formats.
#[derive(Clone, Copy, PartialEq, Eq)]
enum FrameFormat {
    /// G.729, 8 kbit/s.
    G729_8k,
    /// G.729 Annex D, 6.4 kbit/s.
    G729D6k4,
}

/// Bit layout of a frame format (`G729FormatDescription`).
struct FormatDescription {
    /// Adaptive codebook index bits, per subframe.
    ac_index_bits: [u32; 2],
    /// Whether the first subframe carries a pitch delay parity bit.
    parity_bit: bool,
    gc_1st_index_bits: u32,
    gc_2nd_index_bits: u32,
    /// Number of sign bits of the fixed-codebook vector.
    fc_signs_bits: u32,
    /// Fixed-codebook index bits.
    fc_indexes_bits: u32,
    /// Size of a frame in bytes.
    block_size: usize,
}

const FORMAT_G729_8K: FormatDescription = FormatDescription {
    ac_index_bits: [8, 5],
    parity_bit: true,
    gc_1st_index_bits: 3,
    gc_2nd_index_bits: 4,
    fc_signs_bits: 4,
    fc_indexes_bits: 13,
    block_size: G729_8K_BLOCK_SIZE,
};

const FORMAT_G729D_6K4: FormatDescription = FormatDescription {
    ac_index_bits: [8, 4],
    parity_bit: false,
    gc_1st_index_bits: 3,
    gc_2nd_index_bits: 3,
    fc_signs_bits: 2,
    fc_indexes_bits: 9,
    block_size: G729D_6K4_BLOCK_SIZE,
};

/// Pseudo random number generator used to conceal erased frames (4.4.4).
#[inline]
fn g729_prng(value: u16) -> u16 {
    value.wrapping_mul(31821).wrapping_add(13849)
}

/// Decoder state (`G729ChannelContext`).
#[derive(Clone)]
struct Core {
    /// Past excitation signal buffer.
    exc_base: [i16; EXC_LEN],
    /// Integer part of the previous subframe's pitch delay (4.1.3).
    pitch_delay_int_prev: i32,
    /// (2.13) LSP quantizer outputs of the current and four previous frames.
    past_quantizer_outputs: [[i16; LP_ORDER]; MA_NP + 1],
    /// (2.13) quantized LSF coefficients from the previous frame.
    lsfq: [i16; LP_ORDER],
    /// (0.15) LSP coefficients of the previous frame (3.2.5).
    lsp_prev: [i16; LP_ORDER],
    /// (5.10) past quantized energy.
    quant_energy: [i16; 4],
    /// Previous speech data for the LP synthesis filter.
    syn_filter_data: [i16; 10],
    postfilter: Postfilter,
    /// (1.14) pitch gain of the current and five previous subframes.
    past_gain_pitch: [i16; 6],
    /// (14.1) gain code from the current and previous subframe.
    past_gain_code: [i16; 2],
    /// Voice decision on the previous subframe (0 noise, 1 intermediate, 2 voice), Annex D.
    voice_decision: i16,
    /// Detected onset level (0-2).
    onset: i16,
    /// Whether the previous frame was declared periodic (4.4).
    was_periodic: bool,
    /// (1.14) gain coefficient (4.2.4).
    gain_coeff: i16,
    /// Random number generator value (4.4.4).
    rand_value: u16,
    /// Switched MA predictor of the LSP quantizer from the last good frame.
    ma_predictor_prev: usize,
    /// (14.14) high-pass filter data (past input).
    hpf_f: [i32; 2],
    /// High-pass filter data (past output).
    hpf_z: [i16; 2],
}

impl Core {
    fn new() -> Self {
        let mut past_quantizer_outputs = [[0i16; LP_ORDER]; MA_NP + 1];
        for row in past_quantizer_outputs.iter_mut() {
            for (i, v) in row.iter_mut().enumerate() {
                *v = ((18717 * (i as i32 + 1)) >> 3) as i16;
            }
        }

        Core {
            exc_base: [0; EXC_LEN],
            pitch_delay_int_prev: PITCH_DELAY_MIN,
            past_quantizer_outputs,
            lsfq: [0; LP_ORDER],
            lsp_prev: LSP_INIT,
            // -14 in (5.10).
            quant_energy: [-14336; 4],
            syn_filter_data: [0; 10],
            postfilter: Postfilter::new(),
            past_gain_pitch: [0; 6],
            past_gain_code: [0; 2],
            voice_decision: 0,
            onset: 0,
            was_periodic: false,
            // 1.0 in (1.14).
            gain_coeff: 16384,
            // Random seed initialization.
            rand_value: 21845,
            ma_predictor_prev: 0,
            hpf_f: [0; 2],
            hpf_z: [0; 2],
        }
    }

    /// Decodes the LSFs from L0-L3 (3.2.4).
    fn lsf_decode(&mut self, ma: usize, vq_1st: usize, vq_2nd_low: usize, vq_2nd_high: usize) {
        const MIN_DISTANCE: [i32; 2] = [10, 5]; // (2.13)

        let mut q = [0i16; LP_ORDER];
        for i in 0..5 {
            q[i] = CB_LSP_1ST[vq_1st][i].wrapping_add(CB_LSP_2ND[vq_2nd_low][i]);
            q[i + 5] = CB_LSP_1ST[vq_1st][i + 5].wrapping_add(CB_LSP_2ND[vq_2nd_high][i + 5]);
        }

        for &min_distance in &MIN_DISTANCE {
            for i in 1..LP_ORDER {
                let diff = (i32::from(q[i - 1]) - i32::from(q[i]) + min_distance) >> 1;
                if diff > 0 {
                    q[i - 1] = (i32::from(q[i - 1]) - diff) as i16;
                    q[i] = (i32::from(q[i]) + diff) as i16;
                }
            }
        }

        for i in 0..LP_ORDER {
            let mut sum = i32::from(q[i]) * i32::from(CB_MA_PREDICTOR_SUM[ma][i]);
            for j in 0..MA_NP {
                sum = sum.wrapping_add(
                    i32::from(self.past_quantizer_outputs[j][i])
                        * i32::from(CB_MA_PREDICTOR[ma][j][i]),
                );
            }
            self.lsfq[i] = (sum >> 15) as i16;
        }
        self.past_quantizer_outputs[MA_NP] = q;

        reorder_lsf(&mut self.lsfq, LSFQ_DIFF_MIN, LSFQ_MIN, LSFQ_MAX);
    }

    /// Restores the past LSP quantizer output from the LSFs of the previous frame, to conceal an
    /// erased frame.
    fn lsf_restore_from_previous(&mut self) {
        let ma = self.ma_predictor_prev;

        for i in 0..LP_ORDER {
            let mut tmp = i32::from(self.lsfq[i]) << 15;

            for k in 0..MA_NP {
                tmp = tmp.wrapping_sub(
                    i32::from(self.past_quantizer_outputs[k][i])
                        * i32::from(CB_MA_PREDICTOR[ma][k][i]),
                );
            }

            self.past_quantizer_outputs[MA_NP][i] = (((tmp >> 15)
                .wrapping_mul(i32::from(CB_MA_PREDICTOR_SUM_INV[ma][i])))
                >> 12) as i16;
        }
    }

    /// Decodes one frame of `format` from `buf` (exactly `format.block_size` bytes) into the
    /// `FRAME_SIZE` samples of `out`.
    fn decode_frame(
        &mut self,
        buf: &[u8],
        packet_type: FrameFormat,
        format: &FormatDescription,
        out: &mut [i16],
    ) -> Result<()> {
        // The reference resets the voice decision at the start of every 8 kbit/s frame.
        if packet_type == FrameFormat::G729_8k {
            self.onset = 0;
            self.voice_decision = DECISION_VOICE;
        }

        // A frame of all zero bytes is declared erased.
        let frame_erasure = buf.iter().all(|&b| b == 0);
        let mut bad_pitch = false; // Parity check failed.
        let mut is_periodic = false; // Whether one of the subframes is periodic.

        let mut br = BitReaderLtr::new(buf);
        let mut read = |n: u32| -> Result<u32> {
            match br.read_bits_leq32(n) {
                Ok(v) => Ok(v),
                Err(_) => decode_error("g729: truncated frame"),
            }
        };

        let ma_predictor = read(1)? as usize;
        let quantizer_1st = read(VQ_1ST_BITS)? as usize;
        let quantizer_2nd_lo = read(VQ_2ND_BITS)? as usize;
        let quantizer_2nd_hi = read(VQ_2ND_BITS)? as usize;

        if frame_erasure {
            self.lsf_restore_from_previous();
        }
        else {
            self.lsf_decode(ma_predictor, quantizer_1st, quantizer_2nd_lo, quantizer_2nd_hi);
            self.ma_predictor_prev = ma_predictor;
        }

        // Rotate the quantizer outputs: the newest becomes index 0.
        self.past_quantizer_outputs.rotate_right(1);

        let mut lsp_cur = [0i16; LP_ORDER];
        lsf2lsp(&mut lsp_cur, &self.lsfq);

        let mut lp = [[0i16; LP_ORDER + 1]; 2]; // (3.12)
        {
            let [lp0, lp1] = &mut lp;
            lp_decode(lp0, lp1, &lsp_cur, &self.lsp_prev);
        }
        self.lsp_prev = lsp_cur;

        let mut pitch_delay_int = [0i32; 2];

        for i in 0..2 {
            let ac_index = read(format.ac_index_bits[i])? as i32;
            if i == 0 && format.parity_bit {
                let parity = (ac_index >> 2).count_ones() & 1;
                bad_pitch = parity == read(1)?;
            }
            let mut fc_indexes = read(format.fc_indexes_bits)?;
            let mut pulses_signs = read(format.fc_signs_bits)?;
            let gc_1st_index = read(format.gc_1st_index_bits)? as usize;
            let gc_2nd_index = read(format.gc_2nd_index_bits)? as usize;

            let pitch_delay_3x;
            if frame_erasure {
                pitch_delay_3x = 3 * self.pitch_delay_int_prev;
            }
            else if i == 0 {
                if bad_pitch {
                    pitch_delay_3x = 3 * self.pitch_delay_int_prev;
                }
                else {
                    pitch_delay_3x = decode_8bit_to_1st_delay3(ac_index);
                }
            }
            else {
                let pitch_delay_min =
                    (self.pitch_delay_int_prev - 5).clamp(PITCH_DELAY_MIN, PITCH_DELAY_MAX - 9);

                if packet_type == FrameFormat::G729D6k4 {
                    pitch_delay_3x = decode_4bit_to_2nd_delay3(ac_index, pitch_delay_min);
                }
                else {
                    pitch_delay_3x = decode_5_6_bit_to_2nd_delay3(ac_index, pitch_delay_min);
                }
            }

            // Round the pitch delay to nearest (used everywhere except in the interpolation).
            pitch_delay_int[i] = ((pitch_delay_3x + 1) / 3).min(PITCH_DELAY_MAX);

            if frame_erasure {
                self.rand_value = g729_prng(self.rand_value);
                fc_indexes = u32::from(self.rand_value) & ((1 << format.fc_indexes_bits) - 1);

                self.rand_value = g729_prng(self.rand_value);
                pulses_signs = u32::from(self.rand_value as u8);
            }

            // Fixed-codebook vector.
            let mut fc = [0i16; SUBFRAME_SIZE];
            match packet_type {
                FrameFormat::G729_8k => fc_pulse_per_track(
                    &mut fc,
                    &FC_4PULSES_8BITS_TRACKS_13,
                    &FC_4PULSES_8BITS_TRACK_4,
                    fc_indexes,
                    pulses_signs,
                    3,
                    3,
                ),
                FrameFormat::G729D6k4 => fc_pulse_per_track(
                    &mut fc,
                    &FC_2PULSES_9BITS_TRACK1_GRAY,
                    &FC_2PULSES_9BITS_TRACK2_GRAY,
                    fc_indexes,
                    pulses_signs,
                    1,
                    4,
                ),
            }

            // This filter enhances the harmonic components of the fixed-codebook vector to
            // improve the quality of the reconstructed speech.
            //
            //            / fc_v[i],                                    i < pitch_delay
            // fc_v[i] = <
            //            \ fc_v[i] + gain_pitch * fc_v[i-pitch_delay], i >= pitch_delay
            //
            // The update is sequential: for short delays it reads values it has just written.
            let pd = pitch_delay_int[i] as usize;
            if SUBFRAME_SIZE > pd {
                let sharp = i32::from(self.past_gain_pitch[0]).clamp(SHARP_MIN, SHARP_MAX) as i16;
                for j in 0..SUBFRAME_SIZE - pd {
                    fc[pd + j] = weighted_sum(fc[pd + j], fc[j], 1 << 14, sharp, 0, 14);
                }
            }

            self.past_gain_pitch.copy_within(0..5, 1);
            self.past_gain_code[1] = self.past_gain_code[0];

            let mut gain_corr_factor;
            if frame_erasure {
                // 0.90 (0.15)
                self.past_gain_pitch[0] =
                    ((29491 * i32::from(self.past_gain_pitch[0])) >> 15) as i16;
                // 0.98 (0.11)
                self.past_gain_code[0] = ((2007 * i32::from(self.past_gain_code[0])) >> 11) as i16;

                gain_corr_factor = 0;
            }
            else {
                if packet_type == FrameFormat::G729D6k4 {
                    self.past_gain_pitch[0] = CB_GAIN_1ST_6K4[gc_1st_index][0]
                        .wrapping_add(CB_GAIN_2ND_6K4[gc_2nd_index][0]);
                    gain_corr_factor = i32::from(CB_GAIN_1ST_6K4[gc_1st_index][1])
                        + i32::from(CB_GAIN_2ND_6K4[gc_2nd_index][1]);

                    // Without the check below an overflow can occur in `update_past_gain`. It is
                    // not an issue for G.729, because gain_corr_factor is always greater than
                    // 1024 there, while in G.729D it can even be zero.
                    gain_corr_factor = gain_corr_factor.max(1024);
                    // FFmpeg's default build halves the factor here; the reference code
                    // corrects the factor and the gain after decoding the gain instead.
                    gain_corr_factor >>= 1;
                }
                else {
                    self.past_gain_pitch[0] = CB_GAIN_1ST_8K[gc_1st_index][0]
                        .wrapping_add(CB_GAIN_2ND_8K[gc_2nd_index][0]);
                    gain_corr_factor = i32::from(CB_GAIN_1ST_8K[gc_1st_index][1])
                        + i32::from(CB_GAIN_2ND_8K[gc_2nd_index][1]);
                }

                // Decode the fixed-codebook gain.
                self.past_gain_code[0] = decode_gain_code(
                    gain_corr_factor,
                    &fc,
                    MR_ENERGY,
                    &self.quant_energy,
                    &MA_PREDICTION_COEFF,
                );
            }
            update_past_gain(&mut self.quant_energy, gain_corr_factor, frame_erasure);

            // Adaptive codebook contribution; the routine requires rounding to lowest.
            let exc_i = EXC_START + i * SUBFRAME_SIZE;
            let lag = (pitch_delay_3x / 3) as usize;
            interpolate_exc(
                &mut self.exc_base,
                exc_i,
                exc_i - lag,
                ((pitch_delay_3x % 3) << 1) as usize,
                SUBFRAME_SIZE,
            );

            let wa = if !self.was_periodic && frame_erasure { 0 } else { self.past_gain_pitch[0] };
            let wb = if self.was_periodic && frame_erasure { 0 } else { self.past_gain_code[0] };
            for j in 0..SUBFRAME_SIZE {
                self.exc_base[exc_i + j] =
                    weighted_sum(self.exc_base[exc_i + j], fc[j], wa, wb, 1 << 13, 14);
            }

            let mut synth = [0i16; SUBFRAME_SIZE + 10];
            synth[..10].copy_from_slice(&self.syn_filter_data);

            let overflow = lp_synthesis_filter(
                &mut synth,
                10,
                &lp[i][1..],
                &self.exc_base[exc_i..exc_i + SUBFRAME_SIZE],
                SUBFRAME_SIZE,
                10,
                true,
                0x800,
            );
            if overflow {
                // Overflow occurred: downscale the excitation signal...
                for v in self.exc_base.iter_mut() {
                    *v >>= 2;
                }
            }

            // ...and synthesize again.
            if packet_type == FrameFormat::G729D6k4 {
                self.onset = g729d_onset_decision(self.onset, &self.past_gain_code);
                self.voice_decision =
                    g729d_voice_decision(self.onset, self.voice_decision, &self.past_gain_pitch);

                let mut exc_new = [0i16; SUBFRAME_SIZE];
                g729d_get_new_exc(
                    &mut exc_new,
                    &self.exc_base[exc_i..exc_i + SUBFRAME_SIZE],
                    &fc,
                    self.voice_decision,
                    self.past_gain_code[0],
                );

                lp_synthesis_filter(
                    &mut synth,
                    10,
                    &lp[i][1..],
                    &exc_new,
                    SUBFRAME_SIZE,
                    10,
                    false,
                    0x800,
                );
            }
            else {
                lp_synthesis_filter(
                    &mut synth,
                    10,
                    &lp[i][1..],
                    &self.exc_base[exc_i..exc_i + SUBFRAME_SIZE],
                    SUBFRAME_SIZE,
                    10,
                    false,
                    0x800,
                );
            }

            // Save the data (without postfilter) for use in the next subframe.
            self.syn_filter_data.copy_from_slice(&synth[SUBFRAME_SIZE..SUBFRAME_SIZE + 10]);

            // Calculate the gain of the unfiltered signal for use in the AGC.
            let gain_before: i32 = synth[10..].iter().map(|&s| i32::from(s).abs()).sum();

            // Call the postfilter and also update the voicing decision for use in the next frame.
            self.postfilter.run(&mut is_periodic, &lp[i], pitch_delay_int[0], &mut synth);

            // Calculate the gain of the filtered signal for use in the AGC.
            let gain_after: i32 = synth[10..].iter().map(|&s| i32::from(s).abs()).sum();

            self.gain_coeff =
                adaptive_gain_control(gain_before, gain_after, &mut synth[10..], self.gain_coeff);

            if frame_erasure {
                self.pitch_delay_int_prev = (self.pitch_delay_int_prev + 1).min(PITCH_DELAY_MAX);
            }
            else {
                self.pitch_delay_int_prev = pitch_delay_int[i];
            }

            synth[8..10].copy_from_slice(&self.hpf_z);
            high_pass_filter(
                &mut out[i * SUBFRAME_SIZE..(i + 1) * SUBFRAME_SIZE],
                &mut self.hpf_f,
                &synth[8..],
                SUBFRAME_SIZE,
            );
            self.hpf_z.copy_from_slice(&synth[8 + SUBFRAME_SIZE..10 + SUBFRAME_SIZE]);
        }

        self.was_periodic = is_periodic;

        // Save the signal for use in the next frame.
        self.exc_base.copy_within(FRAME_SIZE..FRAME_SIZE + EXC_START, 0);

        Ok(())
    }
}

/// Makes the onset decision for the current subframe (Annex D). `past_gain_code` holds the gain
/// code of the current and previous subframe.
fn g729d_onset_decision(past_onset: i16, past_gain_code: &[i16; 2]) -> i16 {
    if (past_gain_code[0] >> 1) > past_gain_code[1] {
        return 2;
    }

    (past_onset - 1).max(0)
}

/// Makes the voice decision for the current subframe (Annex D).
fn g729d_voice_decision(onset: i16, prev_voice_decision: i16, past_gain_pitch: &[i16; 6]) -> i16 {
    let mut voice_decision = if past_gain_pitch[0] >= 14745 {
        // 0.9
        DECISION_VOICE
    }
    else if past_gain_pitch[0] <= 9830 {
        // 0.6
        DECISION_NOISE
    }
    else {
        DECISION_INTERMEDIATE
    };

    let low_gain_pitch_cnt = past_gain_pitch.iter().filter(|&&g| g < 9830).count();

    if low_gain_pitch_cnt > 2 && onset == 0 {
        voice_decision = DECISION_NOISE;
    }

    if onset == 0 && voice_decision > prev_voice_decision + 1 {
        voice_decision -= 1;
    }

    if onset != 0 && voice_decision < DECISION_VOICE {
        voice_decision += 1;
    }

    voice_decision
}

/// Constructs the new excitation signal with the phase filter applied (Annex D, D.6.2).
fn g729d_get_new_exc(
    out: &mut [i16; SUBFRAME_SIZE],
    exc: &[i16],
    fc_cur: &[i16; SUBFRAME_SIZE],
    dstate: i16,
    gain_code: i16,
) {
    let mut fc_new = [0i16; SUBFRAME_SIZE];

    // The state is always within 0..=2: it is either a decision constant or incremented to at
    // most `DECISION_VOICE`.
    let state = usize::from(dstate.clamp(0, 2) as u16);
    convolve_circ(&mut fc_new, fc_cur, &PHASE_FILTER[state], SUBFRAME_SIZE);

    let gain = i32::from(gain_code);
    for i in 0..SUBFRAME_SIZE {
        let mut o = exc[i];
        o = (i32::from(o) - ((gain * i32::from(fc_cur[i]) + 0x2000) >> 14)) as i16;
        o = (i32::from(o) + ((gain * i32::from(fc_new[i]) + 0x2000) >> 14)) as i16;
        out[i] = o;
    }
}

/// G.729 decoder.
pub struct G729Decoder {
    params: AudioCodecParameters,
    opts: AudioDecoderOptions,
    buf: AudioBuffer<i16>,
    core: Core,
}

impl G729Decoder {
    pub fn try_new(params: &AudioCodecParameters, opts: &AudioDecoderOptions) -> Result<Self> {
        if params.codec != CODEC_ID_G729 {
            return unsupported_error("g729: invalid codec");
        }

        // Only mono is defined for G.729.
        if let Some(channels) = &params.channels {
            if channels.count() != 1 {
                return unsupported_error("g729: only mono is supported");
            }
        }

        let rate = match params.sample_rate {
            Some(rate) if rate > 0 => rate,
            Some(_) => return unsupported_error("g729: invalid sample rate"),
            None => 8000,
        };

        let spec = AudioSpec::new(rate, layouts::CHANNEL_LAYOUT_MONO);
        let buf = AudioBuffer::new(spec, INITIAL_FRAMES * FRAME_SIZE);

        let mut params = params.clone();
        params.with_sample_rate(rate).with_channels(layouts::CHANNEL_LAYOUT_MONO);

        Ok(G729Decoder { params, opts: *opts, buf, core: Core::new() })
    }

    fn decode_inner(&mut self, packet: &PacketRef<'_>) -> Result<()> {
        let data = packet.data;

        // Like FFmpeg: a multiple of 10 bytes is a sequence of G.729 frames, exactly 8 bytes is
        // one Annex D frame. Everything else, e.g. an Annex B SID frame, is rejected.
        let (packet_type, format) = if !data.is_empty() && data.len() % G729_8K_BLOCK_SIZE == 0 {
            (FrameFormat::G729_8k, &FORMAT_G729_8K)
        }
        else if data.len() == G729D_6K4_BLOCK_SIZE {
            (FrameFormat::G729D6k4, &FORMAT_G729D_6K4)
        }
        else {
            return decode_error("g729: unknown packet size");
        };

        let frames = data.len() / format.block_size;

        if frames * FRAME_SIZE > self.buf.capacity() {
            self.buf = AudioBuffer::new(self.buf.spec().clone(), frames * FRAME_SIZE);
        }

        self.buf.clear();
        self.buf.render_uninit(Some(frames * FRAME_SIZE));

        let Some(out) = self.buf.plane_mut(0)
        else {
            return decode_error("g729: no output plane");
        };

        for (block, out) in
            data.chunks_exact(format.block_size).zip(out.chunks_exact_mut(FRAME_SIZE))
        {
            self.core.decode_frame(block, packet_type, format, out)?;
        }

        if self.opts.gapless {
            self.buf.trim(packet.trim_start.get() as usize, packet.trim_end.get() as usize);
        }

        Ok(())
    }
}

impl AudioDecoder for G729Decoder {
    fn reset(&mut self) {
        self.core = Core::new();
    }

    fn codec_info(&self) -> &CodecInfo {
        &Self::supported_codecs().first().expect("at least one codec registered").info
    }

    fn codec_params(&self) -> &AudioCodecParameters {
        &self.params
    }

    fn decode_ref(&mut self, packet: &PacketRef<'_>) -> Result<GenericAudioBufferRef<'_>> {
        if let Err(e) = self.decode_inner(packet) {
            self.buf.clear();
            Err(e)
        }
        else {
            Ok(self.buf.as_generic_audio_buffer_ref())
        }
    }

    fn finalize(&mut self) -> FinalizeResult {
        Default::default()
    }

    fn last_decoded(&self) -> GenericAudioBufferRef<'_> {
        self.buf.as_generic_audio_buffer_ref()
    }
}

impl RegisterableAudioDecoder for G729Decoder {
    fn try_registry_new(
        params: &AudioCodecParameters,
        opts: &AudioDecoderOptions,
    ) -> Result<Box<dyn AudioDecoder>>
    where
        Self: Sized,
    {
        Ok(Box::new(G729Decoder::try_new(params, opts)?))
    }

    fn supported_codecs() -> &'static [SupportedAudioCodec] {
        &[support_audio_codec!(CODEC_ID_G729, "g729", "G.729")]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decoder() -> G729Decoder {
        let mut p = AudioCodecParameters::new();
        p.for_codec(CODEC_ID_G729).with_sample_rate(8000);
        G729Decoder::try_new(&p, &AudioDecoderOptions::default()).unwrap()
    }

    fn packet(data: &[u8]) -> PacketRef<'_> {
        PacketRef::new(0, Default::default(), Default::default(), data)
    }

    /// Arbitrary (corrupt) input must never panic and must always yield the expected duration.
    #[test]
    fn verify_random_frames_do_not_panic() {
        let mut dec = decoder();
        let mut state = 0x1234_5678_9abc_def0u64;
        let mut data = [0u8; 30];
        for n in 0..20000 {
            for b in data.iter_mut() {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *b = (state >> 32) as u8;
            }
            // Mix in erased frames and Annex D frames.
            let len = match n % 5 {
                0 => 30,
                1 => 20,
                2 => 8,
                _ => 10,
            };
            if n % 7 == 0 {
                data[..len].fill(0);
            }
            let buf = dec.decode_ref(&packet(&data[..len])).unwrap();
            assert_eq!(buf.frames(), if len == 8 { 80 } else { len / 10 * 80 });
        }
    }

    #[test]
    fn verify_bad_sizes_are_errors() {
        let mut dec = decoder();
        for len in [0usize, 1, 2, 7, 9, 11, 16, 19] {
            let data = vec![0x55u8; len];
            assert!(dec.decode_ref(&packet(&data)).is_err(), "len {len}");
            assert_eq!(dec.last_decoded().frames(), 0);
        }
    }

    #[test]
    fn verify_zero_frame_is_erasure() {
        // Concealment of an erased frame must be deterministic and finite.
        let mut dec = decoder();
        let data = [0u8; 10];
        for _ in 0..50 {
            assert!(dec.decode_ref(&packet(&data)).is_ok());
        }
    }
}
