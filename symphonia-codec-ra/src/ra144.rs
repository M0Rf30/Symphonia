// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! RealAudio 1.0 (14.4K) core.
//!
//! Integer-exact port of FFmpeg's `libavcodec/ra144dec.c` and `libavcodec/ra144.c`. Every
//! 20-byte frame holds four 40-sample sub-blocks (160 samples at 8 kHz). All arithmetic follows the
//! reference's `int`/`unsigned` semantics, with wrapping operations where the reference relies on
//! overflow.

use symphonia_core::errors::{Result, decode_error};
use symphonia_core::io::{BitReaderLtr, ReadBitsLtr};

use crate::tables144::{
    CB1_BASE, CB1_VECTS, CB2_BASE, CB2_VECTS, ENERGY_TAB, GAIN_EXP_TAB, GAIN_VAL_TAB, LPC_REFL_CB,
};

/// Number of sub-blocks within a frame.
const NBLOCKS: usize = 4;
/// Sub-block size in samples.
pub(crate) const BLOCKSIZE: usize = 40;
/// Size of the adaptive codebook.
const BUFFERSIZE: usize = 146;
/// Size of an encoded frame in bytes.
pub(crate) const FRAME_SIZE: usize = 20;
/// Order of the LPC filter.
const LPC_ORDER: usize = 10;
/// Number of samples decoded from one frame.
pub(crate) const FRAME_SAMPLES: usize = NBLOCKS * BLOCKSIZE;

/// Bits used to code each reflection coefficient.
const REFL_BITS: [u32; LPC_ORDER] = [6, 5, 5, 4, 4, 3, 3, 3, 3, 2];

/// Evaluates `sqrt(x << 24)`. `x` must fit in 20 bits. The value is evaluated in an odd way to make
/// the output identical to the binary decoder (`ff_t_sqrt`).
fn t_sqrt(mut x: u32) -> i32 {
    let mut s = 2;
    while x > 0xfff {
        s += 1;
        x >>= 2;
    }
    // `x <= 0xfff` so `x << 20` cannot overflow. The root is below 2^16, `s` is at most 13.
    ((x << 20).isqrt() << s) as i32
}

/// Root mean square of the reflection coefficients (`ff_rms`).
fn rms(data: &[i32; LPC_ORDER]) -> u32 {
    let mut res: u32 = 0x10000;
    let mut b: u32 = LPC_ORDER as u32;

    for &d in data {
        let t = (0x100_0000i32.wrapping_sub(d.wrapping_mul(d))) >> 12;
        res = (t as u32).wrapping_mul(res) >> 12;

        if res == 0 {
            return 0;
        }

        while res <= 0x3fff {
            b += 1;
            res <<= 2;
        }
    }

    (t_sqrt(res) as u32).checked_shr(b).unwrap_or(0)
}

fn rescale_rms(rms: u32, energy: u32) -> u32 {
    rms.wrapping_mul(energy) >> 10
}

/// Inverse root mean square of a sub-block (`ff_irms`).
fn irms(data: &[i16; BLOCKSIZE]) -> i32 {
    let sum =
        data.iter().fold(0u32, |acc, &d| acc.wrapping_add((i32::from(d) * i32::from(d)) as u32));

    if sum == 0 {
        return 0;
    }

    // `t_sqrt(sum) >> 8` is at least 16 for any non-zero `sum`.
    0x2000_0000 / (t_sqrt(sum) >> 8)
}

/// Evaluates the reflection coefficients from the filter coefficients (`ff_eval_refl`).
///
/// Returns `None` if one of the reflection coefficients is out of range.
fn eval_refl(coefs: &[i16; LPC_ORDER]) -> Option<[i32; LPC_ORDER]> {
    let mut refl = [0i32; LPC_ORDER];
    let mut bp2 = [0i32; LPC_ORDER];
    let mut bp1 = [0i32; LPC_ORDER];

    for (d, &s) in bp2.iter_mut().zip(coefs) {
        *d = i32::from(s);
    }

    refl[LPC_ORDER - 1] = bp2[LPC_ORDER - 1];

    if (bp2[LPC_ORDER - 1] as u32).wrapping_add(0x1000) > 0x1fff {
        return None;
    }

    for i in (0..LPC_ORDER - 1).rev() {
        // `bp2[i + 1]` is within +-0x1000, so the square cannot overflow.
        let mut b = 0x1000 - ((bp2[i + 1] * bp2[i + 1]) >> 12);

        if b == 0 {
            b = -2;
        }

        // `b` is in 1..=0x1000 (or -2): the division is neither by zero nor overflowing.
        b = 0x100_0000 / b;

        for j in 0..=i {
            let t = bp2[j].wrapping_sub(refl[i + 1].wrapping_mul(bp2[i - j]) >> 12);
            bp1[j] = t.wrapping_mul(b) >> 12;
        }

        if (bp1[i] as u32).wrapping_add(0x1000) > 0x1fff {
            return None;
        }

        refl[i] = bp1[i];

        std::mem::swap(&mut bp1, &mut bp2);
    }

    Some(refl)
}

/// Evaluates the LPC filter coefficients from the reflection coefficients (`ff_eval_coefs`). Does
/// the inverse of `eval_refl`.
fn eval_coefs(refl: &[i32; LPC_ORDER]) -> [i32; LPC_ORDER] {
    let mut prev = [0i32; LPC_ORDER];
    let mut cur = [0i32; LPC_ORDER];

    for i in 0..LPC_ORDER {
        cur[i] = refl[i].wrapping_mul(16);

        for j in 0..i {
            cur[j] = (refl[i].wrapping_mul(prev[i - j - 1]) >> 12).wrapping_add(prev[j]);
        }

        prev = cur;
    }

    prev.map(|c| c >> 4)
}

fn int_to_int16(inp: &[i32; LPC_ORDER]) -> [i16; LPC_ORDER] {
    inp.map(|v| v as i16)
}

/// Copies the last `offset` values of the adaptive codebook to `target`. If those values are not
/// enough to fill the target buffer, fills it with another copy of those values (`ff_copy_and_dup`).
fn copy_and_dup(target: &mut [i16; BLOCKSIZE], source: &[i16; BUFFERSIZE], offset: usize) {
    let source = &source[BUFFERSIZE - offset..];

    let n = BLOCKSIZE.min(offset);
    target[..n].copy_from_slice(&source[..n]);

    if offset < BLOCKSIZE {
        let rest = BLOCKSIZE - offset;
        target[offset..].copy_from_slice(&source[..rest]);
    }
}

/// RealAudio 1.0 (14.4K) decoder state.
pub(crate) struct Ra144 {
    /// Previous frame energy.
    old_energy: u32,
    /// LPC coefficients of the current (`[0]`) and previous (`[1]`) frame.
    lpc_coef: [[i32; LPC_ORDER]; 2],
    lpc_refl_rms: [u32; 2],
    /// The current sub-block padded by the last 10 values of the previous one.
    curr_sblock: [i16; BLOCKSIZE + LPC_ORDER],
    /// Adaptive codebook.
    adapt_cb: [i16; BUFFERSIZE],
    buffer_a: [i16; BLOCKSIZE],
}

impl Ra144 {
    pub(crate) fn new() -> Self {
        Ra144 {
            old_energy: 0,
            lpc_coef: [[0; LPC_ORDER]; 2],
            lpc_refl_rms: [0; 2],
            curr_sblock: [0; BLOCKSIZE + LPC_ORDER],
            adapt_cb: [0; BUFFERSIZE],
            buffer_a: [0; BLOCKSIZE],
        }
    }

    /// Interpolates the coefficients of a sub-block from the fourth block of this frame and of the
    /// last frame (`ff_interp`). Returns the sub-block's gain.
    fn interp(&self, out: &mut [i16; LPC_ORDER], a: i32, copyold: usize, energy: u32) -> u32 {
        let b = NBLOCKS as i32 - a;

        for (i, o) in out.iter_mut().enumerate() {
            let c = a
                .wrapping_mul(self.lpc_coef[0][i])
                .wrapping_add(b.wrapping_mul(self.lpc_coef[1][i]));
            *o = (c >> 2) as i16;
        }

        match eval_refl(out) {
            Some(work) => rescale_rms(rms(&work), energy),
            None => {
                // The interpolated coefficients are unstable, copy either new or old coefficients.
                *out = int_to_int16(&self.lpc_coef[copyold]);
                rescale_rms(self.lpc_refl_rms[copyold], energy)
            }
        }
    }

    /// Decodes one sub-block, leaving its output in `curr_sblock[LPC_ORDER..]`
    /// (`do_output_subblock` and `ff_subblock_synthesis`).
    fn subblock_synthesis(
        &mut self,
        lpc_coefs: &[i16; LPC_ORDER],
        cba_idx: usize,
        cb1_idx: usize,
        cb2_idx: usize,
        gval: i32,
        gain: usize,
    ) {
        let mut m = [0i32; 3];

        // Index of the adaptive codebook, 0 if none.
        let cba = if cba_idx != 0 { cba_idx + BLOCKSIZE / 2 - 1 } else { 0 };

        if cba != 0 {
            copy_and_dup(&mut self.buffer_a, &self.adapt_cb, cba);
            m[0] = ((irms(&self.buffer_a) as u32).wrapping_mul(gval as u32) >> 12) as i32;
        }
        m[1] = i32::from(CB1_BASE[cb1_idx]).wrapping_mul(gval) >> 8;
        m[2] = i32::from(CB2_BASE[cb2_idx]).wrapping_mul(gval) >> 8;

        self.adapt_cb.copy_within(BLOCKSIZE.., 0);

        // add_wav
        let mut v = [0i32; 3];
        let first = if cba != 0 { 0 } else { 1 };
        for i in first..3 {
            let t = (i32::from(GAIN_VAL_TAB[gain][i]) as u32).wrapping_mul(m[i] as u32);
            v[i] = (t >> GAIN_EXP_TAB[gain]) as i32;
        }

        let s2 = &CB1_VECTS[cb1_idx];
        let s3 = &CB2_VECTS[cb2_idx];
        let block = &mut self.adapt_cb[BUFFERSIZE - BLOCKSIZE..];
        for i in 0..BLOCKSIZE {
            let mut acc = i32::from(s2[i])
                .wrapping_mul(v[1])
                .wrapping_add(i32::from(s3[i]).wrapping_mul(v[2]));
            if v[0] != 0 {
                acc = acc.wrapping_add(i32::from(self.buffer_a[i]).wrapping_mul(v[0]));
            }
            block[i] = (acc >> 12) as i16;
        }

        // Synthesis filter (`ff_celp_lp_synthesis_filter` with stop_on_overflow = 1, shift = 0,
        // rounder = 0xfff).
        self.curr_sblock.copy_within(BLOCKSIZE.., 0);

        #[allow(clippy::needless_range_loop)]
        for n in 0..BLOCKSIZE {
            let mut sum: i32 = 0xfff;
            for i in 1..=LPC_ORDER {
                let p =
                    i32::from(lpc_coefs[i - 1]) * i32::from(self.curr_sblock[LPC_ORDER + n - i]);
                sum = sum.wrapping_sub(p);
            }

            let sum1 = (sum >> 12) + i32::from(block[n]);
            let clipped = sum1.clamp(i32::from(i16::MIN), i32::from(i16::MAX));

            if clipped != sum1 {
                self.curr_sblock.fill(0);
                return;
            }

            self.curr_sblock[LPC_ORDER + n] = clipped as i16;
        }
    }

    /// Decodes one 20-byte frame into 160 samples.
    pub(crate) fn decode_frame(&mut self, buf: &[u8], out: &mut [i16]) -> Result<()> {
        if buf.len() < FRAME_SIZE || out.len() < FRAME_SAMPLES {
            return decode_error("ra: frame too small");
        }

        let mut bs = BitReaderLtr::new(&buf[..FRAME_SIZE]);

        let mut read = |n: u32| -> Result<u32> {
            match bs.read_bits_leq32(n) {
                Ok(v) => Ok(v),
                Err(_) => decode_error("ra: truncated frame"),
            }
        };

        let mut lpc_refl = [0i32; LPC_ORDER];
        for (i, r) in lpc_refl.iter_mut().enumerate() {
            *r = i32::from(LPC_REFL_CB[i][read(REFL_BITS[i])? as usize]);
        }

        self.lpc_coef[0] = eval_coefs(&lpc_refl);
        self.lpc_refl_rms[0] = rms(&lpc_refl);

        let energy = ENERGY_TAB[read(5)? as usize] as u32;
        let old_energy = self.old_energy;

        let mut block_coefs = [[0i16; LPC_ORDER]; NBLOCKS];
        let mut refl_rms = [0u32; NBLOCKS];

        refl_rms[0] = self.interp(&mut block_coefs[0], 1, 1, old_energy);
        refl_rms[1] = self.interp(
            &mut block_coefs[1],
            2,
            usize::from(energy <= old_energy),
            (t_sqrt(energy.wrapping_mul(old_energy)) >> 12) as u32,
        );
        refl_rms[2] = self.interp(&mut block_coefs[2], 3, 0, energy);
        refl_rms[3] = rescale_rms(self.lpc_refl_rms[0], energy);
        block_coefs[3] = int_to_int16(&self.lpc_coef[0]);

        for (coefs, (&gval, samples)) in
            block_coefs.iter().zip(refl_rms.iter().zip(out.chunks_exact_mut(BLOCKSIZE)))
        {
            let cba_idx = read(7)? as usize;
            let gain = read(8)? as usize;
            let cb1_idx = read(7)? as usize;
            let cb2_idx = read(7)? as usize;

            self.subblock_synthesis(coefs, cba_idx, cb1_idx, cb2_idx, gval as i32, gain);

            for (o, &s) in samples.iter_mut().zip(&self.curr_sblock[LPC_ORDER..]) {
                *o = (i32::from(s) * 4).clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
            }
        }

        self.old_energy = energy;
        self.lpc_refl_rms[1] = self.lpc_refl_rms[0];
        self.lpc_coef.swap(0, 1);

        Ok(())
    }
}
