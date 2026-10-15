// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! GSM 06.10 frame parsing and synthesis (ETSI EN 300 961, clause 4.3: the decoder).
//!
//! All arithmetic uses the 16-bit saturating primitives of the standard's fixed-point description
//! (`add`, `sub`, `mult_r`, `asl`, `asr`), exactly as in the reference implementation (libgsm).

use symphonia_core::errors::Result;

/// Number of samples in a frame.
pub const FRAME_SAMPLES: usize = 160;
/// Number of samples in a sub-segment of a frame.
const SUB_SAMPLES: usize = 40;
/// Number of RPE pulses in a sub-segment.
const RPE_PULSES: usize = 13;
/// Length of the reconstructed short-term residual history required by the long-term predictor
/// (the maximum lag).
const HISTORY: usize = 120;

/// Minimum and maximum valid long-term predictor lags (clause 4.2.11).
const MIN_LAG: i16 = 40;
const MAX_LAG: i16 = 120;

/// Smallest value of a 16-bit word.
const MIN_WORD: i16 = i16::MIN;
/// Largest value of a 16-bit word.
const MAX_WORD: i16 = i16::MAX;

/// Offsets of the LARc (log-area ratio) quantisers, MIC[i] (clause 4.2.8, table 4.1).
const MIC: [i16; 8] = [-32, -32, -16, -16, -8, -8, -4, -4];
/// Offsets B[i] of the LAR dequantisation (clause 4.2.8, table 4.1).
const B: [i16; 8] = [0, 0, 2048, -2560, 94, -1792, -341, -1144];
/// Inverse scale factors INVA[i] of the LAR dequantisation (clause 4.2.8, table 4.1).
const INVA: [i16; 8] = [13107, 13107, 13107, 13107, 19223, 17476, 31454, 29708];
/// Bit widths of the eight LARc parameters (clause 1, table 1.1).
pub const LARC_BITS: [u32; 8] = [6, 6, 5, 5, 4, 4, 3, 3];

/// Long-term predictor gains QLB[bc] (clause 4.3.2, table 4.3a).
const QLB: [i16; 4] = [3277, 11469, 21299, 32767];
/// Normalised inverse mantissa FAC[mant] used to dequantise RPE samples (clause 4.2.15,
/// table 4.6).
const FAC: [i16; 8] = [18431, 20479, 22527, 24575, 26623, 28671, 30719, 32767];

/// `GSM_ADD`: saturating 16-bit addition.
#[inline]
fn add(a: i16, b: i16) -> i16 {
    a.saturating_add(b)
}

/// `GSM_SUB`: saturating 16-bit subtraction.
#[inline]
fn sub(a: i16, b: i16) -> i16 {
    a.saturating_sub(b)
}

/// `GSM_MULT_R`: Q15 multiplication with rounding.
#[inline]
fn mult_r(a: i16, b: i16) -> i16 {
    if a == MIN_WORD && b == MIN_WORD {
        MAX_WORD
    }
    else {
        ((i32::from(a) * i32::from(b) + 16384) >> 15) as i16
    }
}

/// `gsm_asl`: arithmetic shift left by `n` (right if negative), truncated to a word.
#[inline]
fn asl(a: i16, n: i32) -> i16 {
    if n >= 16 {
        0
    }
    else if n <= -16 {
        -i16::from(a < 0)
    }
    else if n < 0 {
        asr(a, -n)
    }
    else {
        (i32::from(a) << n) as i16
    }
}

/// `gsm_asr`: arithmetic shift right by `n` (left if negative), truncated to a word.
#[inline]
fn asr(a: i16, n: i32) -> i16 {
    if n >= 16 {
        -i16::from(a < 0)
    }
    else if n <= -16 {
        0
    }
    else if n < 0 {
        (i32::from(a) << -n) as i16
    }
    else {
        a >> n
    }
}

/// One long-term/RPE sub-segment of a frame as transmitted.
#[derive(Clone, Copy, Default)]
struct SubFrame {
    /// LTP lag, Nc.
    nc: i16,
    /// LTP gain index, bc.
    bc: usize,
    /// RPE grid position, Mc.
    mc: usize,
    /// Block amplitude, xmaxc.
    xmaxc: i16,
    /// RPE pulses, xMc.
    xmc: [i16; RPE_PULSES],
}

/// The parameters of a frame as transmitted (clause 1, table 1.1).
#[derive(Clone, Default)]
pub struct Frame {
    larc: [i16; 8],
    sub: [SubFrame; 4],
}

impl Frame {
    /// Reads the 260 payload bits of a frame with `read(n)` returning the next `n`-bit field.
    pub fn parse(mut read: impl FnMut(u32) -> Result<u32>) -> Result<Frame> {
        let mut f = Frame::default();

        for (larc, bits) in f.larc.iter_mut().zip(LARC_BITS) {
            *larc = read(bits)? as i16;
        }

        for sub in f.sub.iter_mut() {
            sub.nc = read(7)? as i16;
            sub.bc = read(2)? as usize;
            sub.mc = read(2)? as usize;
            sub.xmaxc = read(6)? as i16;
            for xmc in sub.xmc.iter_mut() {
                *xmc = read(3)? as i16;
            }
        }

        Ok(f)
    }
}

/// Decodes the coded LARs into LARpp (clause 4.2.8).
fn decode_lars(larc: &[i16; 8]) -> [i16; 8] {
    let mut larpp = [0i16; 8];

    for i in 0..8 {
        // The sum is at most 6 bits so the shift cannot overflow a word.
        let t = (larc[i] + MIC[i]) << 10;
        let t = sub(t, B[i] << 1);
        let t = mult_r(INVA[i], t);
        larpp[i] = add(t, t);
    }

    larpp
}

/// Converts interpolated LARp into reflection coefficients rp in place (clause 4.2.9.2).
fn lar_to_rp(larp: &mut [i16; 8]) {
    for v in larp.iter_mut() {
        let neg = *v < 0;
        let mut t = if neg { if *v == MIN_WORD { MAX_WORD } else { -*v } } else { *v };

        if t < 11059 {
            t <<= 1;
        }
        else if t < 20070 {
            t += 11059;
        }
        else {
            t = add(t >> 2, 26112);
        }

        *v = if neg { -t } else { t };
    }
}

/// Dequantises the 13 RPE pulses of a sub-segment and places them on the grid, returning the
/// 40-sample long-term residual estimate `erp` (clauses 4.2.15 - 4.2.16).
fn rpe_decode(sub: &SubFrame) -> [i16; SUB_SAMPLES] {
    // xmaxc -> exp, mant (clause 4.2.15).
    let xmaxc = sub.xmaxc;
    let mut exp: i16 = 0;
    if xmaxc > 15 {
        exp = (xmaxc >> 3) - 1;
    }
    let mut mant = xmaxc - (exp << 3);
    if mant == 0 {
        exp = -4;
        mant = 7;
    }
    else {
        while mant <= 7 {
            mant = (mant << 1) | 1;
            exp -= 1;
        }
        mant -= 8;
    }

    let temp1 = FAC[(mant & 7) as usize];
    let temp2 = 6 - i32::from(exp);
    let temp3 = asl(1, temp2 - 1);

    let mut erp = [0i16; SUB_SAMPLES];

    for (i, &xmc) in sub.xmc.iter().enumerate() {
        // Clause 4.2.15 (inverse quantisation of the RPE sample, equations 4.2-16/17).
        let t = ((xmc << 1) - 7) << 12;
        let t = mult_r(temp1, t);
        let t = add(t, temp3);
        // Clause 4.2.16: RPE grid positioning. Mc is 2 bits so the index is at most 3 + 36.
        erp[sub.mc + 3 * i] = asr(t, temp2);
    }

    erp
}

/// The decoder state that persists between frames.
#[derive(Clone)]
pub struct Synth {
    /// The reconstructed short-term residual: `HISTORY` samples of the previous frames followed
    /// by the current frame (`dp0` and `drp` of the reference).
    drp: [i16; HISTORY + FRAME_SAMPLES],
    /// Lattice filter state (`v`).
    v: [i16; 9],
    /// LARpp of the previous frame (`LARpp_j_1`).
    larpp_prev: [i16; 8],
    /// The last valid LTP lag (`nrp`).
    nrp: i16,
    /// De-emphasis filter memory (`msr`).
    msr: i16,
}

impl Synth {
    /// The state of a freshly created decoder: `nrp` is 40 and everything else is zero.
    pub fn new() -> Self {
        Synth {
            drp: [0; HISTORY + FRAME_SAMPLES],
            v: [0; 9],
            larpp_prev: [0; 8],
            nrp: MIN_LAG,
            msr: 0,
        }
    }

    /// Synthesises the 160 samples of `frame` into `out` (clause 4.3).
    pub fn decode(&mut self, frame: &Frame, out: &mut [i16]) {
        let larpp = decode_lars(&frame.larc);

        // Clauses 4.3.1 - 4.3.2: RPE decoding and long-term synthesis of each sub-segment.
        for (j, sub) in frame.sub.iter().enumerate() {
            let erp = rpe_decode(sub);

            // A lag outside 40..=120 is a transmission error: keep the previous lag.
            let nr = if (MIN_LAG..=MAX_LAG).contains(&sub.nc) { sub.nc } else { self.nrp };
            self.nrp = nr;

            let gain = QLB[sub.bc & 3];
            let base = HISTORY + j * SUB_SAMPLES;
            for (k, &e) in erp.iter().enumerate() {
                // base >= 120 >= nr so the history index cannot underflow.
                let pred = mult_r(gain, self.drp[base + k - nr as usize]);
                self.drp[base + k] = add(e, pred);
            }
        }

        self.short_term_synthesis(&larpp, out);

        // Slide the history window: the last 120 samples of this frame become the history.
        self.drp.copy_within(FRAME_SAMPLES.., 0);

        // Clause 4.3.4: de-emphasis, upscaling and truncation to 13 bits.
        for s in out.iter_mut().take(FRAME_SAMPLES) {
            let t = mult_r(self.msr, 28180);
            self.msr = add(*s, t);
            *s = add(self.msr, self.msr) & !7;
        }
    }

    /// Clause 4.3.3: short-term synthesis filtering with the LARpp interpolated over four
    /// segments of the frame (clause 4.2.9.1).
    fn short_term_synthesis(&mut self, larpp: &[i16; 8], out: &mut [i16]) {
        let prev = self.larpp_prev;
        let residual = &self.drp[HISTORY..];

        // (start, end) of the four interpolation segments.
        const SEGMENTS: [(usize, usize); 4] = [(0, 13), (13, 27), (27, 40), (40, FRAME_SAMPLES)];

        for (seg, &(start, end)) in SEGMENTS.iter().enumerate() {
            let mut rrp = [0i16; 8];
            for i in 0..8 {
                rrp[i] = match seg {
                    0 => add(add(prev[i] >> 2, larpp[i] >> 2), prev[i] >> 1),
                    1 => add(prev[i] >> 1, larpp[i] >> 1),
                    2 => add(add(prev[i] >> 2, larpp[i] >> 2), larpp[i] >> 1),
                    _ => larpp[i],
                };
            }
            lar_to_rp(&mut rrp);

            for (o, &r) in out[start..end].iter_mut().zip(&residual[start..end]) {
                let mut sri = r;
                for i in (0..8).rev() {
                    sri = sub(sri, mult_r(rrp[i], self.v[i]));
                    self.v[i + 1] = add(self.v[i], mult_r(rrp[i], sri));
                }
                self.v[0] = sri;
                *o = sri;
            }
        }

        self.larpp_prev = *larpp;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The APCM dequantisation computed from the standard's formulas must equal the
    /// precomputed table of FFmpeg's `ff_gsm_dequant_tab` (spot-checked rows).
    #[test]
    fn dequant_matches_reference_table() {
        let rows: [(i16, [i16; 8]); 6] = [
            (0, [-28, -20, -12, -4, 4, 12, 20, 28]),
            (15, [-448, -320, -192, -64, 64, 192, 320, 448]),
            (16, [-504, -360, -216, -72, 72, 216, 360, 504]),
            (47, [-7168, -5120, -3072, -1024, 1024, 3072, 5120, 7168]),
            (48, [-8063, -5759, -3456, -1152, 1152, 3456, 5760, 8064]),
            (63, [-28671, -20479, -12288, -4096, 4096, 12288, 20479, 28671]),
        ];

        for (xmaxc, expected) in rows {
            for (xmc, &want) in expected.iter().enumerate() {
                let sub = SubFrame {
                    xmaxc,
                    xmc: [xmc as i16, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
                    ..SubFrame::default()
                };
                assert_eq!(rpe_decode(&sub)[0], want, "xmaxc {xmaxc} xmc {xmc}");
            }
        }
    }

    #[test]
    fn arithmetic_primitives_saturate() {
        assert_eq!(mult_r(MIN_WORD, MIN_WORD), MAX_WORD);
        assert_eq!(add(MAX_WORD, 1), MAX_WORD);
        assert_eq!(sub(MIN_WORD, 1), MIN_WORD);
        assert_eq!(asl(1, -1), 0);
        assert_eq!(asr(-1, 20), -1);
    }
}
