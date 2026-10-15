// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Pitch and formant postfilters, voiced/unvoiced classification and residual interpolation.
//!
//! Follows FFmpeg `libavcodec/g723_1dec.c` (`comp_ppf_coeff`, `comp_ppf_gains`,
//! `comp_interp_index`, `residual_interp`, `formant_postfilter`, `gain_scale`); see ITU-T G.723.1
//! sections 3.6 - 3.7.

use crate::celp::{
    clip16, clipl32, div, dot_product_sat, normalize_bits, sat_add32, sat_dadd32, scale_vector,
    shl, shr, square_root,
};
use crate::channel::Channel;
use crate::{FRAME_LEN, LPC_ORDER, PITCH_MAX, SUBFRAME_LEN, SUBFRAMES};

/// Postfilter gain weighting factors scaled by 2^15, indexed by rate (6.3, 5.3 kbit/s).
const PPF_GAIN_WEIGHT: [i32; 2] = [0x1800, 0x2000];

/// 0.65^i (zero part) and 0.75^i (pole part) scaled by 2^15.
const POSTFILTER_TBL: [[i32; LPC_ORDER]; 2] = [
    [21299, 13844, 8999, 5849, 3802, 2471, 1606, 1044, 679, 441],
    [24576, 18432, 13824, 10368, 7776, 5832, 4374, 3281, 2460, 1845],
];

/// Pitch postfilter parameters.
#[derive(Clone, Copy)]
pub struct PpfParam {
    /// Postfilter backward/forward lag.
    pub index: i32,
    /// Optimal gain.
    pub opt_gain: i32,
    /// Scaling gain.
    pub sc_gain: i32,
}

/// Returns `buf[start..]`, or an empty slice if `start` is out of range.
#[inline]
fn tail(buf: &[i16], start: isize) -> &[i16] {
    usize::try_from(start).ok().and_then(|s| buf.get(s..)).unwrap_or(&[])
}

/// Estimates the maximum auto-correlation around the pitch lag and returns the lag.
///
/// `base` is the index of the signal start in `buf`, `offset` the offset of the excitation vector.
fn autocorr_max(
    buf: &[i16],
    base: usize,
    offset: i32,
    ccr_max: &mut i32,
    pitch_lag: i32,
    length: usize,
    dir: i32,
) -> i32 {
    let pitch_lag = pitch_lag.min(PITCH_MAX as i32 - 3);
    let limit = if dir > 0 {
        (FRAME_LEN as i32 + PITCH_MAX as i32 - offset - length as i32).min(pitch_lag + 3)
    }
    else {
        pitch_lag + 3
    };

    let mut lag = 0;
    let cur = tail(buf, base as isize);
    for i in pitch_lag - 3..=limit {
        let other = tail(buf, base as isize + (dir * i) as isize);
        let ccr = dot_product_sat(cur, other, length);
        if ccr > *ccr_max {
            *ccr_max = ccr;
            lag = i;
        }
    }
    lag
}

/// Calculates the pitch postfilter optimal and scaling gains.
fn comp_ppf_gains(lag: i32, rate_5300: bool, tgt_eng: i32, ccr: i32, res_eng: i32) -> PpfParam {
    let weight = PPF_GAIN_WEIGHT[usize::from(rate_5300)];

    let temp1 = tgt_eng.wrapping_mul(res_eng) >> 1;
    let temp2 = ccr.wrapping_mul(ccr).wrapping_shl(1);

    let (mut opt_gain, sc_gain);
    if temp2 > temp1 {
        opt_gain = if ccr >= res_eng {
            weight
        }
        else {
            // Stored in an `int16_t` in the reference.
            i32::from((div(ccr << 15, res_eng).wrapping_mul(weight) >> 15) as i16)
        };
        // pf_res^2 = tgt_eng + 2*ccr*gain + res_eng*gain^2
        let temp1 = (tgt_eng << 15).wrapping_add(ccr.wrapping_mul(opt_gain).wrapping_shl(1));
        let temp2 = (opt_gain.wrapping_mul(opt_gain) >> 15).wrapping_mul(res_eng);
        let pf_residual = sat_add32(temp1, temp2.wrapping_add(1 << 15)) >> 16;

        let temp1 = if tgt_eng >= pf_residual.wrapping_shl(1) {
            0x7fff
        }
        else {
            div(tgt_eng << 14, pf_residual)
        };

        // scaling_gain = sqrt(tgt_eng/pf_res^2)
        sc_gain = square_root(temp1.wrapping_shl(16));
    }
    else {
        opt_gain = 0;
        sc_gain = 0x7fff;
    }

    opt_gain = i32::from(clip16(opt_gain.wrapping_mul(sc_gain) >> 15));

    PpfParam { index: lag, opt_gain, sc_gain }
}

/// Calculates the pitch postfilter parameters of the subframe starting at `offset` in the
/// scaled excitation held in `audio[LPC_ORDER..]`.
pub fn comp_ppf_coeff(audio: &[i16], offset: i32, pitch_lag: i32, rate_5300: bool) -> PpfParam {
    // 0 - target energy, 1 - forward cross-correlation, 2 - forward residual energy,
    // 3 - backward cross-correlation, 4 - backward residual energy.
    let mut energy = [0i32; 5];
    let base = LPC_ORDER + offset as usize;

    let (mut ccr_fwd, mut ccr_back) = (0, 0);
    let fwd_lag = autocorr_max(audio, base, offset, &mut ccr_fwd, pitch_lag, SUBFRAME_LEN, 1);
    let back_lag = autocorr_max(audio, base, offset, &mut ccr_back, pitch_lag, SUBFRAME_LEN, -1);
    energy[1] = ccr_fwd;
    energy[3] = ccr_back;

    // Case 0, Section 3.6.
    if back_lag == 0 && fwd_lag == 0 {
        return PpfParam { index: 0, opt_gain: 0, sc_gain: 0x7fff };
    }

    let cur = tail(audio, base as isize);

    // Compute target energy.
    energy[0] = dot_product_sat(cur, cur, SUBFRAME_LEN);

    // Compute forward residual energy.
    if fwd_lag != 0 {
        let s = tail(audio, (base as isize) + fwd_lag as isize);
        energy[2] = dot_product_sat(s, s, SUBFRAME_LEN);
    }

    // Compute backward residual energy.
    if back_lag != 0 {
        let s = tail(audio, (base as isize) - back_lag as isize);
        energy[4] = dot_product_sat(s, s, SUBFRAME_LEN);
    }

    // Normalize and shorten.
    let max = energy.iter().fold(0, |m, &e| m.max(e));
    let scale = normalize_bits(max, 31);
    for e in &mut energy {
        *e = shl(*e, scale) >> 16;
    }

    if fwd_lag != 0 && back_lag == 0 {
        // Case 1.
        comp_ppf_gains(fwd_lag, rate_5300, energy[0], energy[1], energy[2])
    }
    else if fwd_lag == 0 {
        // Case 2.
        comp_ppf_gains(-back_lag, rate_5300, energy[0], energy[3], energy[4])
    }
    else {
        // Case 3: select the largest of energy[1]^2/energy[2] and energy[3]^2/energy[4].
        let temp1 =
            energy[4].wrapping_mul((energy[1].wrapping_mul(energy[1]).wrapping_add(1 << 14)) >> 15);
        let temp2 =
            energy[2].wrapping_mul((energy[3].wrapping_mul(energy[3]).wrapping_add(1 << 14)) >> 15);
        if temp1 >= temp2 {
            comp_ppf_gains(fwd_lag, rate_5300, energy[0], energy[1], energy[2])
        }
        else {
            comp_ppf_gains(-back_lag, rate_5300, energy[0], energy[3], energy[4])
        }
    }
}

impl Channel {
    /// Classifies the frame as voiced/unvoiced. Returns the residual interpolation index if
    /// voiced, 0 otherwise. Stores the excitation energy in `sid_gain` and the scale in
    /// `cur_gain`.
    pub fn comp_interp_index(&mut self, pitch_lag: i32) -> i32 {
        const OFFSET: usize = PITCH_MAX + 2 * SUBFRAME_LEN;
        const LEN: usize = FRAME_LEN + PITCH_MAX;

        self.cur_gain =
            scale_vector(&mut self.audio[LPC_ORDER..LPC_ORDER + LEN], &self.excitation[..LEN]);
        let base = LPC_ORDER + OFFSET;
        let buf = &self.audio[..];

        // Compute maximum backward cross-correlation.
        let mut ccr = 0;
        let index =
            autocorr_max(buf, base, OFFSET as i32, &mut ccr, pitch_lag, SUBFRAME_LEN * 2, -1);
        ccr = sat_add32(ccr, 1 << 15) >> 16;

        // Compute target energy.
        let cur = tail(buf, base as isize);
        let tgt_eng = dot_product_sat(cur, cur, SUBFRAME_LEN * 2);
        let exc_eng = sat_add32(tgt_eng, 1 << 15) >> 16;
        self.sid_gain = exc_eng;

        if ccr <= 0 {
            return 0;
        }

        // Compute best energy.
        let best = tail(buf, base as isize - index as isize);
        let best_eng = dot_product_sat(best, best, SUBFRAME_LEN * 2);
        let best_eng = sat_add32(best_eng, 1 << 15) >> 16;

        let temp = best_eng.wrapping_mul(exc_eng) >> 3;

        if temp < ccr.wrapping_mul(ccr) { index } else { 0 }
    }

    /// Performs residual interpolation based on the frame classification, producing 240 samples
    /// in `audio[LPC_ORDER..]` (`residual_interp`).
    pub fn residual_interp(&mut self) {
        let lag = self.interp_index.clamp(0, PITCH_MAX as i32) as usize;
        let gain = self.interp_gain;
        let out = &mut self.audio[LPC_ORDER..LPC_ORDER + FRAME_LEN];

        if lag != 0 {
            // Voiced: attenuate.
            for i in 0..lag {
                out[i] = ((i32::from(self.excitation[PITCH_MAX + i - lag]) * 3) >> 2) as i16;
            }
            for i in lag..FRAME_LEN {
                out[i] = out[i - lag];
            }
        }
        else {
            // Unvoiced.
            for o in out.iter_mut() {
                self.random_seed =
                    i32::from(self.random_seed.wrapping_mul(521).wrapping_add(259) as i16);
                *o = (gain.wrapping_mul(self.random_seed) >> 15) as i16;
            }
            self.excitation[..FRAME_LEN + PITCH_MAX].fill(0);
        }
    }

    /// Adjusts the gain of the postfiltered signal (`gain_scale`).
    fn gain_scale(&mut self, buf: &mut [i16], energy: i32) {
        let mut denom = 0i32;
        for &b in buf.iter() {
            let temp = i32::from(b) >> 2;
            denom = sat_dadd32(denom, temp.wrapping_mul(temp));
        }

        let mut num = energy;
        let gain = if num != 0 && denom != 0 {
            let bits1 = normalize_bits(num, 31);
            let bits2 = normalize_bits(denom, 31);
            num = shr(shl(num, bits1), 1);
            denom = shl(denom, bits2);

            // `av_clip_uintp2(.., 5)`.
            let bits2 = (5 + bits1 - bits2).clamp(0, 31);

            let gain = div(num >> 1, denom >> 16);
            square_root(shr(gain.wrapping_shl(16), bits2))
        }
        else {
            1 << 12
        };

        for b in buf.iter_mut() {
            self.pf_gain =
                (15i32.wrapping_mul(self.pf_gain).wrapping_add(gain).wrapping_add(1 << 3)) >> 4;
            let g = self.pf_gain.wrapping_add(self.pf_gain >> 4);
            *b = clip16(i32::from(*b).wrapping_mul(g).wrapping_add(1 << 10) >> 11);
        }
    }

    /// Performs formant postfiltering of the synthesised frame in `audio` into `dst`
    /// (`formant_postfilter`).
    pub fn formant_postfilter(&mut self, lpc: &[i16], dst: &mut [i16]) {
        let mut filter_signal = [0i32; LPC_ORDER + FRAME_LEN];

        self.audio[..LPC_ORDER].copy_from_slice(&self.fir_mem);
        filter_signal[..LPC_ORDER].copy_from_slice(&self.iir_mem);

        for (j, lpc) in lpc.chunks_exact(LPC_ORDER).enumerate().take(SUBFRAMES) {
            let mut fir = [0i32; LPC_ORDER];
            let mut iir = [0i32; LPC_ORDER];
            for k in 0..LPC_ORDER {
                fir[k] = (-i32::from(lpc[k]) * POSTFILTER_TBL[0][k] + (1 << 14)) >> 15;
                iir[k] = (-i32::from(lpc[k]) * POSTFILTER_TBL[1][k] + (1 << 14)) >> 15;
            }

            // `fir` and `iir` are `int16_t` in the reference; their range is below 2^15.
            let start = LPC_ORDER + j * SUBFRAME_LEN;
            for m in start..start + SUBFRAME_LEN {
                let mut filter = 0i64;
                for n in 1..=LPC_ORDER {
                    filter -= i64::from(
                        (fir[n - 1].wrapping_mul(i32::from(self.audio[m - n])))
                            .wrapping_sub(iir[n - 1].wrapping_mul(filter_signal[m - n] >> 16)),
                    );
                }
                filter_signal[m] =
                    clipl32(i64::from(self.audio[m]) * 65536 + filter * 8 + (1 << 15));
            }
        }

        self.fir_mem.copy_from_slice(&self.audio[FRAME_LEN..FRAME_LEN + LPC_ORDER]);
        self.iir_mem.copy_from_slice(&filter_signal[FRAME_LEN..FRAME_LEN + LPC_ORDER]);

        for i in 0..SUBFRAMES {
            let sub = i * SUBFRAME_LEN;
            let buf = &self.audio[LPC_ORDER + sub..LPC_ORDER + sub + SUBFRAME_LEN];
            let dst = &mut dst[sub..sub + SUBFRAME_LEN];

            // Normalize.
            let scale = scale_vector(dst, buf);

            // Compute auto correlation coefficients.
            let auto_corr0 = dot_product_sat(dst, &dst[1..], SUBFRAME_LEN - 1);
            let auto_corr1 = dot_product_sat(dst, dst, SUBFRAME_LEN);

            // Compute reflection coefficient.
            let mut temp = auto_corr1 >> 16;
            if temp != 0 {
                temp = div(auto_corr0 >> 2, temp);
            }
            self.reflection_coef =
                (3i32.wrapping_mul(self.reflection_coef).wrapping_add(temp).wrapping_add(2)) >> 2;
            let temp = (self.reflection_coef.wrapping_neg() >> 1) & !3;

            // Compensation filter.
            let sig = &filter_signal[LPC_ORDER + sub - 1..];
            for (j, d) in dst.iter_mut().enumerate() {
                *d = (sat_dadd32(sig[j + 1], (sig[j] >> 16).wrapping_mul(temp)) >> 16) as i16;
            }

            // Compute normalized signal energy.
            let temp = 2 * scale + 4;
            let energy =
                if temp < 0 { clipl32(i64::from(auto_corr1) << -temp) } else { auto_corr1 >> temp };

            self.gain_scale(dst, energy);
        }
    }
}
