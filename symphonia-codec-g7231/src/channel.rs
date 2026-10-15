// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! G.723.1 frame unpacking, excitation generation and synthesis for one channel.
//!
//! Follows FFmpeg `libavcodec/g723_1dec.c` (`unpack_bitstream`, `gen_fcb_excitation`,
//! `g723_1_decode_frame`) and ITU-T G.723.1 section 4.

use symphonia_core::io::{BitReaderRtl, ReadBitsRtl};

use crate::celp::{clip16, gen_acb_excitation, gen_dirac_train, lp_synthesis_filter};
use crate::lsp::{inverse_quant, lsp_interpolate};
use crate::postfilter::{PpfParam, comp_ppf_coeff};
use crate::tables::{COMBINATORIAL_TABLE, DC_LSP, FIXED_CB_GAIN, PITCH_CONTRIB};
use crate::{
    FRAME_LEN, GRID_SIZE, LPC_ORDER, PITCH_MAX, PITCH_MIN, PULSE_MAX, SUBFRAME_LEN, SUBFRAMES,
};

/// Frame size in bytes, indexed by the two least significant bits of the first byte.
pub const FRAME_SIZE: [usize; 4] = [24, 20, 4, 1];

/// Number of non-zero pulses in the MP-MLQ excitation of each subframe.
pub const PULSES: [usize; 4] = [6, 5, 6, 5];

/// Size of the MP-MLQ fixed excitation codebooks.
const MAX_POS: [i32; 4] = [593775, 142506, 593775, 142506];

const CNG_RANDOM_SEED: i32 = 12345;

/// Length of the persistent synthesis buffer.
const AUDIO_LEN: usize = FRAME_LEN + LPC_ORDER + PITCH_MAX + 4;

#[derive(Clone, Copy, PartialEq, Eq)]
enum FrameType {
    Active,
    Sid,
    Untransmitted,
}

/// Unpacked parameters of one subframe.
#[derive(Clone, Copy, Default)]
pub struct Subframe {
    pub ad_cb_lag: i32,
    pub ad_cb_gain: i32,
    pub dirac_train: i32,
    pub pulse_sign: i32,
    pub grid_index: i32,
    pub amp_index: i32,
    pub pulse_pos: i32,
}

/// The decoder state of one channel.
///
/// The field layout deliberately mirrors `G723_1_ChannelContext`: the reference decoder reuses the
/// `audio` buffer across frames (including stale contents in the comfort noise and frame erasure
/// paths) and bit-exactness requires doing the same.
pub struct Channel {
    pub subframe: [Subframe; SUBFRAMES],
    cur_frame_type: FrameType,
    past_frame_type: FrameType,
    /// `true` if the last active frame was coded at 5.3 kbit/s.
    pub rate_5300: bool,
    lsp_index: [u8; 3],
    pub pitch_lag: [i32; 2],
    erased_frames: i32,

    prev_lsp: [i16; LPC_ORDER],
    sid_lsp: [i16; LPC_ORDER],
    pub prev_excitation: [i16; PITCH_MAX],
    pub excitation: [i16; PITCH_MAX + FRAME_LEN + 4],
    synth_mem: [i16; LPC_ORDER],
    pub fir_mem: [i16; LPC_ORDER],
    pub iir_mem: [i32; LPC_ORDER],

    pub random_seed: i32,
    pub cng_random_seed: i32,
    pub interp_index: i32,
    pub interp_gain: i32,
    pub sid_gain: i32,
    pub cur_gain: i32,
    pub reflection_coef: i32,
    /// Formant postfilter gain scaling unit memory.
    pub pf_gain: i32,

    pub audio: [i16; AUDIO_LEN],
}

/// Reads `n` bits as a signed value, failing on end of data.
fn bits(br: &mut BitReaderRtl<'_>, n: u32) -> Option<i32> {
    br.read_bits_leq32(n).ok().map(|v| v as i32)
}

/// Generates the fixed codebook excitation vector of a subframe (`gen_fcb_excitation`).
///
/// `vector` has room for the 4 samples the 5.3 kbit/s codebook may place past the subframe end;
/// the reference lets those fall into the following subframe, where they are discarded.
fn gen_fcb_excitation(
    vector: &mut [i16; SUBFRAME_LEN + 4],
    sub: &Subframe,
    rate_5300: bool,
    pitch_lag: i32,
    index: usize,
) {
    vector.fill(0);

    let gain = fixed_gain(sub.amp_index);

    if !rate_5300 {
        if sub.pulse_pos >= MAX_POS[index] || sub.pulse_pos < 0 {
            return;
        }

        // Decode amplitudes and positions.
        let mut j = PULSE_MAX - PULSES[index];
        let mut temp = sub.pulse_pos;
        for i in 0..SUBFRAME_LEN / GRID_SIZE {
            temp -= COMBINATORIAL_TABLE[j][i];
            if temp >= 0 {
                continue;
            }
            temp += COMBINATORIAL_TABLE[j][i];
            j += 1;
            let pos = sub.grid_index as usize + GRID_SIZE * i;
            vector[pos] = if sub.pulse_sign & (1 << (PULSE_MAX - j)) != 0 { -gain } else { gain };
            if j == PULSE_MAX {
                break;
            }
        }
        if sub.dirac_train == 1 {
            gen_dirac_train(vector, pitch_lag as usize);
        }
    }
    else {
        let mut cb_pos = sub.pulse_pos;
        let mut cb_sign = sub.pulse_sign;

        for i in (0..8).step_by(2) {
            let offset = (((cb_pos & 7) << 3) + sub.grid_index) as usize + i;
            vector[offset] = if cb_sign & 1 != 0 { gain } else { -gain };
            cb_pos >>= 3;
            cb_sign >>= 1;
        }

        // Enhance harmonic components.
        let g = (sub.ad_cb_gain.clamp(0, 169) << 1) as usize;
        let lag = i32::from(PITCH_CONTRIB[g]) + pitch_lag + sub.ad_cb_lag - 1;
        let beta = i32::from(PITCH_CONTRIB[g + 1]);

        if lag > 0 && lag < SUBFRAME_LEN as i32 - 2 {
            let lag = lag as usize;
            for i in lag..SUBFRAME_LEN {
                let add = (beta * i32::from(vector[i - lag])) >> 15;
                vector[i] = (i32::from(vector[i]) + add) as i16;
            }
        }
    }
}

#[inline]
fn fixed_gain(amp_index: i32) -> i16 {
    FIXED_CB_GAIN[amp_index.clamp(0, FIXED_CB_GAIN.len() as i32 - 1) as usize]
}

impl Channel {
    pub fn new() -> Self {
        Channel {
            subframe: [Subframe::default(); SUBFRAMES],
            cur_frame_type: FrameType::Active,
            past_frame_type: FrameType::Sid,
            rate_5300: false,
            lsp_index: [0; 3],
            pitch_lag: [0; 2],
            erased_frames: 0,
            prev_lsp: DC_LSP,
            sid_lsp: DC_LSP,
            prev_excitation: [0; PITCH_MAX],
            excitation: [0; PITCH_MAX + FRAME_LEN + 4],
            synth_mem: [0; LPC_ORDER],
            fir_mem: [0; LPC_ORDER],
            iir_mem: [0; LPC_ORDER],
            random_seed: 0,
            cng_random_seed: CNG_RANDOM_SEED,
            interp_index: 0,
            interp_gain: 0,
            sid_gain: 0,
            cur_gain: 0,
            reflection_coef: 0,
            pf_gain: 1 << 12,
            audio: [0; AUDIO_LEN],
        }
    }

    /// Unpacks a frame of `FRAME_SIZE[data[0] & 3]` bytes into parameters (`unpack_bitstream`).
    ///
    /// Returns `None` if the frame contains a forbidden code, in which case it must be treated
    /// as erased.
    fn unpack(&mut self, data: &[u8]) -> Option<()> {
        let mut br = BitReaderRtl::new(data);

        // Extract frame type and rate info.
        let info_bits = bits(&mut br, 2)?;

        if info_bits == 3 {
            self.cur_frame_type = FrameType::Untransmitted;
            return Some(());
        }

        // Extract 24 bit lsp indices, 8 bit for each band.
        self.lsp_index[2] = bits(&mut br, 8)? as u8;
        self.lsp_index[1] = bits(&mut br, 8)? as u8;
        self.lsp_index[0] = bits(&mut br, 8)? as u8;

        if info_bits == 2 {
            self.cur_frame_type = FrameType::Sid;
            self.subframe[0].amp_index = bits(&mut br, 6)?;
            return Some(());
        }

        // Extract the info common to both rates.
        self.rate_5300 = info_bits != 0;
        self.cur_frame_type = FrameType::Active;

        let lag = bits(&mut br, 7)?;
        self.pitch_lag[0] = lag;
        if lag > 123 {
            // Forbidden code.
            return None;
        }
        self.pitch_lag[0] += PITCH_MIN as i32;
        self.subframe[1].ad_cb_lag = bits(&mut br, 2)?;

        let lag = bits(&mut br, 7)?;
        self.pitch_lag[1] = lag;
        if lag > 123 {
            return None;
        }
        self.pitch_lag[1] += PITCH_MIN as i32;
        self.subframe[3].ad_cb_lag = bits(&mut br, 2)?;
        self.subframe[0].ad_cb_lag = 1;
        self.subframe[2].ad_cb_lag = 1;

        for i in 0..SUBFRAMES {
            // Extract combined gain.
            let mut temp = bits(&mut br, 12)?;
            let mut ad_cb_len = 170;
            self.subframe[i].dirac_train = 0;
            if !self.rate_5300 && self.pitch_lag[i >> 1] < SUBFRAME_LEN as i32 - 2 {
                self.subframe[i].dirac_train = temp >> 11;
                temp &= 0x7ff;
                ad_cb_len = 85;
            }
            self.subframe[i].ad_cb_gain = temp / 24;
            if self.subframe[i].ad_cb_gain < ad_cb_len {
                self.subframe[i].amp_index = temp - self.subframe[i].ad_cb_gain * 24;
            }
            else {
                return None;
            }
        }

        for i in 0..SUBFRAMES {
            self.subframe[i].grid_index = bits(&mut br, 1)?;
        }

        if !self.rate_5300 {
            // Skip reserved bit.
            bits(&mut br, 1)?;

            // Compute pulse_pos index using the 13-bit combined position index.
            let mut temp = bits(&mut br, 13)?;
            let p0 = temp / 810;
            temp -= p0 * 810;
            let p1 = temp / 90;
            temp -= p1 * 90;
            let p2 = temp / 9;
            let p3 = temp - p2 * 9;

            self.subframe[0].pulse_pos = (p0 << 16) + bits(&mut br, 16)?;
            self.subframe[1].pulse_pos = (p1 << 14) + bits(&mut br, 14)?;
            self.subframe[2].pulse_pos = (p2 << 16) + bits(&mut br, 16)?;
            self.subframe[3].pulse_pos = (p3 << 14) + bits(&mut br, 14)?;

            self.subframe[0].pulse_sign = bits(&mut br, 6)?;
            self.subframe[1].pulse_sign = bits(&mut br, 5)?;
            self.subframe[2].pulse_sign = bits(&mut br, 6)?;
            self.subframe[3].pulse_sign = bits(&mut br, 5)?;
        }
        else {
            for i in 0..SUBFRAMES {
                self.subframe[i].pulse_pos = bits(&mut br, 12)?;
            }
            for i in 0..SUBFRAMES {
                self.subframe[i].pulse_sign = bits(&mut br, 4)?;
            }
        }

        Some(())
    }

    /// Decodes one frame of `FRAME_SIZE[data[0] & 3]` bytes into 240 samples (`out`).
    pub fn decode_frame(&mut self, data: &[u8], out: &mut [i16]) {
        let mut bad_frame = false;
        let mut lpc = [0i16; SUBFRAMES * LPC_ORDER];

        if self.unpack(data).is_none() {
            bad_frame = true;
            self.cur_frame_type = if self.past_frame_type == FrameType::Active {
                FrameType::Active
            }
            else {
                FrameType::Untransmitted
            };
        }

        if self.cur_frame_type == FrameType::Active {
            if !bad_frame {
                self.erased_frames = 0;
            }
            else if self.erased_frames != 3 {
                self.erased_frames += 1;
            }

            let mut cur_lsp = [0i16; LPC_ORDER];
            inverse_quant(&mut cur_lsp, &self.prev_lsp, &mut self.lsp_index, bad_frame);
            lsp_interpolate(&mut lpc, &cur_lsp, &self.prev_lsp);

            // Save the lsp_vector for the next frame.
            self.prev_lsp = cur_lsp;

            // Generate the excitation for the frame.
            self.excitation[..PITCH_MAX].copy_from_slice(&self.prev_excitation);

            if self.erased_frames == 0 {
                self.decode_excitation();
            }
            else {
                self.interp_gain = (self.interp_gain * 3 + 2) >> 2;
                if self.erased_frames == 3 {
                    // Mute output.
                    self.excitation[..FRAME_LEN + PITCH_MAX].fill(0);
                    self.prev_excitation.fill(0);
                }
                else {
                    // Regenerate frame.
                    self.residual_interp();

                    // Save the excitation for the next frame.
                    self.prev_excitation.copy_from_slice(
                        &self.audio[LPC_ORDER + FRAME_LEN - PITCH_MAX..LPC_ORDER + FRAME_LEN],
                    );
                }
            }
            self.cng_random_seed = CNG_RANDOM_SEED;
        }
        else {
            if self.cur_frame_type == FrameType::Sid {
                self.sid_gain = crate::cng::sid_gain_to_lsp_index(self.subframe[0].amp_index);
                inverse_quant(&mut self.sid_lsp, &self.prev_lsp, &mut self.lsp_index, false);
            }
            else if self.past_frame_type == FrameType::Active {
                self.sid_gain = self.estimate_sid_gain();
            }

            if self.past_frame_type == FrameType::Active {
                self.cur_gain = self.sid_gain;
            }
            else {
                self.cur_gain = (self.cur_gain.wrapping_mul(7).wrapping_add(self.sid_gain)) >> 3;
            }
            self.generate_noise();
            lsp_interpolate(&mut lpc, &self.sid_lsp, &self.prev_lsp);

            // Save the lsp_vector for the next frame.
            self.prev_lsp = self.sid_lsp;
        }

        self.past_frame_type = self.cur_frame_type;

        self.audio[..LPC_ORDER].copy_from_slice(&self.synth_mem);
        for (j, coeffs) in lpc.chunks_exact(LPC_ORDER).enumerate() {
            lp_synthesis_filter(
                &mut self.audio,
                LPC_ORDER + j * SUBFRAME_LEN,
                coeffs,
                SUBFRAME_LEN,
            );
        }
        self.synth_mem.copy_from_slice(&self.audio[FRAME_LEN..FRAME_LEN + LPC_ORDER]);

        self.formant_postfilter(&lpc, out);
    }

    /// Generates the excitation of an intact active frame and applies the pitch postfilter,
    /// leaving the result in `audio[LPC_ORDER..]`.
    fn decode_excitation(&mut self) {
        let rate_5300 = self.rate_5300;

        // Update interpolation gain memory.
        self.interp_gain =
            i32::from(fixed_gain((self.subframe[2].amp_index + self.subframe[3].amp_index) >> 1));

        for i in 0..SUBFRAMES {
            let mut fcb = [0i16; SUBFRAME_LEN + 4];
            let mut acb = [0i16; SUBFRAME_LEN];
            let pitch_lag = self.pitch_lag[i >> 1];
            let sub = &self.subframe[i];

            gen_fcb_excitation(&mut fcb, sub, rate_5300, pitch_lag, i);
            gen_acb_excitation(
                &mut acb,
                &self.excitation[SUBFRAME_LEN * i..SUBFRAME_LEN * i + PITCH_MAX],
                pitch_lag,
                sub.ad_cb_lag,
                sub.ad_cb_gain,
                !rate_5300,
            );

            // Get the total excitation.
            let dst = &mut self.excitation[PITCH_MAX + i * SUBFRAME_LEN..][..SUBFRAME_LEN];
            for ((d, &f), &a) in dst.iter_mut().zip(fcb.iter()).zip(acb.iter()) {
                let v = clip16(i32::from(f) * 2);
                *d = clip16(i32::from(v) + i32::from(a));
            }
        }

        self.interp_index = self.comp_interp_index(self.pitch_lag[1]);

        // Perform pitch postfiltering.
        let mut ppf = [PpfParam { index: 0, opt_gain: 0, sc_gain: 0 }; SUBFRAMES];
        for (j, p) in ppf.iter_mut().enumerate() {
            let offset = (PITCH_MAX + j * SUBFRAME_LEN) as i32;
            *p = comp_ppf_coeff(&self.audio, offset, self.pitch_lag[j >> 1], rate_5300);
        }

        for (j, p) in ppf.iter().enumerate() {
            let i = j * SUBFRAME_LEN;
            for k in 0..SUBFRAME_LEN {
                let a = self.excitation[PITCH_MAX + i + k];
                let b = usize::try_from((PITCH_MAX + i + k) as i32 + p.index)
                    .ok()
                    .and_then(|x| self.excitation.get(x))
                    .copied()
                    .unwrap_or(0);
                self.audio[LPC_ORDER + i + k] =
                    crate::celp::weighted_sum_sample(a, b, p.sc_gain, p.opt_gain, 1 << 14, 15);
            }
        }

        // Save the excitation for the next frame.
        self.prev_excitation.copy_from_slice(&self.excitation[FRAME_LEN..FRAME_LEN + PITCH_MAX]);
    }
}
