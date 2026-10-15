// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Comfort noise generation for SID and untransmitted frames.
//!
//! Follows FFmpeg `libavcodec/g723_1dec.c` (`estimate_sid_gain`, `generate_noise`,
//! `sid_gain_to_lsp_index`, `cng_rand`); see ITU-T G.723.1 Annex A (silence compression).

use crate::celp::{clip16, clipl32, gen_acb_excitation, log2, shl, shr, square_root};
use crate::channel::{Channel, PULSES};
use crate::{LPC_ORDER, PITCH_MAX, SUBFRAME_LEN, SUBFRAMES};

const CNG_ADAPTIVE_CB_LAG: [i32; 4] = [1, 0, 1, 3];
const CNG_FILT: [i32; 4] = [273, 998, 499, 333];
const CNG_BSEG: [i32; 3] = [2048, 18432, 231233];

/// Converts the 6-bit SID gain index to an LSP index scale (`sid_gain_to_lsp_index`).
pub fn sid_gain_to_lsp_index(gain: i32) -> i32 {
    if gain < 0x10 {
        gain << 6
    }
    else if gain < 0x20 {
        (gain - 8) << 7
    }
    else {
        (gain - 20) << 8
    }
}

/// Linear congruential generator returning a value in `0..base` (`cng_rand`).
fn cng_rand(state: &mut i32, base: i32) -> i32 {
    *state = state.wrapping_mul(521).wrapping_add(259) & 0xffff;
    ((*state & 0x7fff) * base) >> 15
}

impl Channel {
    /// Estimates the SID gain from the energy of the last active frame (`estimate_sid_gain`).
    pub fn estimate_sid_gain(&self) -> i32 {
        let sid_gain = self.sid_gain;
        let shift = 16i32.wrapping_sub(self.cur_gain.wrapping_mul(2));

        let t = if shift > 0 {
            if sid_gain == 0 {
                0
            }
            else if shift >= 31 || (((sid_gain as u32) << shift) as i32) >> shift != sid_gain {
                if sid_gain < 0 { i32::MIN } else { i32::MAX }
            }
            else {
                sid_gain.wrapping_mul(1 << shift)
            }
        }
        else if shift < -31 {
            if sid_gain < 0 { -1 } else { 0 }
        }
        else {
            sid_gain >> -shift
        };
        let x = clipl32((i64::from(t) * i64::from(CNG_FILT[0])) >> 16);

        if x >= CNG_BSEG[2] {
            return 0x3f;
        }

        let (shift, seg) = if x >= CNG_BSEG[1] { (4, 3) } else { (3, i32::from(x >= CNG_BSEG[0])) };
        let seg2 = seg.min(3);

        let mut val = 1i32 << shift;
        let mut val_add = val >> 1;
        for _ in 0..shift {
            let t = seg * 32 + (val << seg2);
            let t = t.wrapping_mul(t);
            if x >= t {
                val += val_add;
            }
            else {
                val -= val_add;
            }
            val_add >>= 1;
        }

        let t = seg * 32 + (val << seg2);
        let y = t.wrapping_mul(t).wrapping_sub(x);
        if y <= 0 {
            let t = seg * 32 + ((val + 1) << seg2);
            let t = t.wrapping_mul(t).wrapping_sub(x);
            val += (seg2 - 1) * 16;
            if t >= y {
                val += 1;
            }
        }
        else {
            let t = seg * 32 + ((val - 1) << seg2);
            let t = t.wrapping_mul(t).wrapping_sub(x);
            val += (seg2 - 1) * 16;
            if t >= y {
                val -= 1;
            }
        }

        val
    }

    /// Generates a frame of comfort noise excitation into `audio[LPC_ORDER..]`
    /// (`generate_noise`).
    pub fn generate_noise(&mut self) {
        let mut off = [0i32; SUBFRAMES];
        let mut signs = [0i32; SUBFRAMES / 2 * 11];
        let mut pos = [0usize; SUBFRAMES / 2 * 11];
        let mut tmp = [0i32; SUBFRAME_LEN * 2];

        self.pitch_lag[0] = cng_rand(&mut self.cng_random_seed, 21) + 123;
        self.pitch_lag[1] = cng_rand(&mut self.cng_random_seed, 19) + 123;

        for (i, sub) in self.subframe.iter_mut().enumerate() {
            sub.ad_cb_gain = cng_rand(&mut self.cng_random_seed, 50) + 1;
            sub.ad_cb_lag = CNG_ADAPTIVE_CB_LAG[i];
        }

        for i in 0..SUBFRAMES / 2 {
            let mut t = cng_rand(&mut self.cng_random_seed, 1 << 13);
            off[i * 2] = t & 1;
            off[i * 2 + 1] = ((t >> 1) & 1) + SUBFRAME_LEN as i32;
            t >>= 2;
            for j in 0..11 {
                signs[i * 11 + j] = ((t & 1) * 2 - 1) * (1 << 14);
                t >>= 1;
            }
        }

        let mut idx = 0;
        for i in 0..SUBFRAMES {
            for (j, t) in tmp.iter_mut().take(SUBFRAME_LEN / 2).enumerate() {
                *t = j as i32;
            }
            let mut t = SUBFRAME_LEN / 2;
            for _ in 0..PULSES[i] {
                let idx2 = cng_rand(&mut self.cng_random_seed, t as i32) as usize;

                pos[idx] = (tmp[idx2] * 2 + off[i]) as usize;
                t -= 1;
                tmp[idx2] = tmp[t];
                idx += 1;
            }
        }

        let rate_6300 = !self.rate_5300;
        self.audio[LPC_ORDER..LPC_ORDER + PITCH_MAX].copy_from_slice(&self.prev_excitation);

        for i in (0..SUBFRAMES).step_by(2) {
            // `vp` is the index of `vector_ptr` in the reference.
            let vp = LPC_ORDER + i * SUBFRAME_LEN;
            let mut acb = [0i16; SUBFRAME_LEN];

            for k in 0..2 {
                let sub = &self.subframe[i + k];
                let s = vp + k * SUBFRAME_LEN;
                gen_acb_excitation(
                    &mut acb,
                    &self.audio[s..s + PITCH_MAX],
                    self.pitch_lag[i >> 1],
                    sub.ad_cb_lag,
                    sub.ad_cb_gain,
                    rate_6300,
                );
                self.audio[s..s + SUBFRAME_LEN].copy_from_slice(&acb);
            }

            let vector = &mut self.audio[vp..vp + SUBFRAME_LEN * 2];

            let t = vector.iter().fold(0u32, |m, &v| m | u32::from(v.unsigned_abs()));
            let t = t.min(0x7fff);
            let mut shift = if t == 0 { 0 } else { (-10 + log2(t)).max(-2) };

            let mut sum = 0i64;
            for (d, &v) in tmp.iter_mut().zip(vector.iter()) {
                let t = if shift < 0 { i32::from(v) << -shift } else { i32::from(v) >> shift };
                sum += i64::from(t) * i64::from(t);
                *d = t;
            }

            let mut b0 = 0i32;
            for j in 0..11 {
                let k = (i / 2) * 11 + j;
                b0 = b0.wrapping_add(tmp[pos[k]].wrapping_mul(signs[k]));
            }
            // Approximated division by 11.
            b0 = ((i64::from(b0) * 2 * 2979 + (1 << 29)) >> 30) as i32;

            let cg = self.cur_gain;
            let mut c = cg.wrapping_mul(cg.wrapping_mul(SUBFRAME_LEN as i32) >> 5);
            c = if shift * 2 + 3 >= 0 { shr(c, shift * 2 + 3) } else { shl(c, -(shift * 2 + 3)) };
            c = ((i64::from(clipl32(sum << 1).wrapping_sub(c)) * 2979) >> 15) as i32;

            let delta = b0.wrapping_mul(b0).wrapping_mul(2).wrapping_sub(c);
            let mut x = if delta <= 0 {
                b0.wrapping_neg()
            }
            else {
                let delta = square_root(delta);
                let x = delta.wrapping_sub(b0);
                let t = delta.wrapping_add(b0);
                if t.unsigned_abs() < x.unsigned_abs() { t.wrapping_neg() } else { x }
            };

            shift += 1;
            x = if shift < 0 { x >> -shift } else { x.wrapping_mul(1 << shift) };
            x = x.clamp(-10000, 10000);

            for j in 0..11 {
                let k = (i / 2) * 11 + j;
                let v = &mut vector[pos[k]];
                *v = clip16(i32::from(*v) + (x.wrapping_mul(signs[k]) >> 15));
            }

            // Copy decoded data to serve as a history for the next decoded subframes.
            self.audio.copy_within(vp..vp + SUBFRAME_LEN * 2, vp + PITCH_MAX);
        }

        // Save the excitation for the next frame.
        self.prev_excitation
            .copy_from_slice(&self.audio[LPC_ORDER + 240..LPC_ORDER + 240 + PITCH_MAX]);
    }
}
