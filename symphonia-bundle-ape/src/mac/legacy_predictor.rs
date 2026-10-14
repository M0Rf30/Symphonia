// Ported from the Monkey's Audio SDK 13.27 (Matthew T. Ashland, BSD 3-clause; see NOTICE in this
// directory): MACLib/Old/AntiPredictorOld.cpp, AntiPredictorFastOld.cpp,
// AntiPredictorNormalOld.cpp, AntiPredictorHighOld.cpp and AntiPredictorExtraHighOld.cpp.

//! The "anti-predictors" of the files older than version 3.93 (3930).
//!
//! The predictors of these versions work on a whole array of residuals (the samples of one channel
//! of a frame) at once. They overwrite their input array as scratch space, exactly as the SDK
//! does, because some of them read back what they have written earlier. All arithmetic wraps like
//! the 32-bit `int` arithmetic of the SDK on the platforms it supports.

use std::num::Wrapping as W;

use crate::mac::error::{ApeError, ApeResult};
use crate::mac::nn_filter::{
    COMPRESSION_EXTRA_HIGH, COMPRESSION_FAST, COMPRESSION_HIGH, COMPRESSION_NORMAL,
};

type I = W<i32>;

#[inline(always)]
fn w(v: i32) -> I {
    W(v)
}

/// `((x >> 30) & 2) - 1`: -1 for a non-negative value, +1 for a negative one.
#[inline(always)]
fn sign30(x: I) -> I {
    ((x >> 30) & w(2)) - w(1)
}

/// `((x >> 28) & 8) - 4`
#[inline(always)]
fn sign28(x: I) -> I {
    ((x >> 28) & w(8)) - w(4)
}

/// `((x >> 29) & 4) - 2`
#[inline(always)]
fn sign29(x: I) -> I {
    ((x >> 29) & w(4)) - w(2)
}

/// `cond ? a : b`
#[inline(always)]
fn sel(cond: bool, a: i32, b: i32) -> I {
    w(if cond { a } else { b })
}

/// What the compression level of a file selects for its anti-predictor
/// (`CreateAntiPredictor` in AntiPredictorOld.cpp).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Fast0000To3320,
    Fast3320To3930,
    Normal0000To3320,
    Normal3320To3800,
    Normal3800To3929,
    High0000To3320,
    High3320To3600,
    High3600To3700,
    High3700To3800,
    High3800To3929,
    ExtraHigh0000To3320,
    ExtraHigh3320To3600,
    ExtraHigh3600To3700,
    ExtraHigh3700To3800,
    ExtraHigh3800To3929,
}

impl Kind {
    /// `CreateAntiPredictor`. The SDK has no anti-predictor for the other compression levels.
    pub fn new(compression_level: u32, version: i32) -> ApeResult<Kind> {
        Ok(match compression_level {
            COMPRESSION_FAST => {
                if version < 3320 {
                    Kind::Fast0000To3320
                }
                else {
                    Kind::Fast3320To3930
                }
            }
            COMPRESSION_NORMAL => {
                if version < 3320 {
                    Kind::Normal0000To3320
                }
                else if version < 3800 {
                    Kind::Normal3320To3800
                }
                else {
                    Kind::Normal3800To3929
                }
            }
            COMPRESSION_HIGH => {
                if version < 3320 {
                    Kind::High0000To3320
                }
                else if version < 3600 {
                    Kind::High3320To3600
                }
                else if version < 3700 {
                    Kind::High3600To3700
                }
                else if version < 3800 {
                    Kind::High3700To3800
                }
                else {
                    Kind::High3800To3929
                }
            }
            COMPRESSION_EXTRA_HIGH => {
                if version < 3320 {
                    Kind::ExtraHigh0000To3320
                }
                else if version < 3600 {
                    Kind::ExtraHigh3320To3600
                }
                else if version < 3700 {
                    Kind::ExtraHigh3600To3700
                }
                else if version < 3800 {
                    Kind::ExtraHigh3700To3800
                }
                else {
                    Kind::ExtraHigh3800To3929
                }
            }
            _ => {
                return Err(ApeError::InvalidFormat(
                    "compression level not supported by this file version",
                ));
            }
        })
    }

    /// The number of bits of the coefficient count and of each coefficient that precede the
    /// residuals of an "extra high" array (`GET_COEFFICIENTS` in APEDecompressCoreOld.cpp), if it
    /// has coefficients.
    pub fn coefficient_bits(self) -> Option<(u32, u32)> {
        match self {
            Kind::ExtraHigh0000To3320 => Some((4, 6)),
            Kind::ExtraHigh3320To3600 => Some((3, 5)),
            Kind::ExtraHigh3600To3700 | Kind::ExtraHigh3700To3800 => Some((3, 6)),
            _ => None,
        }
    }

    /// Whether the residuals are decoded into the output array, which is transformed in place
    /// (`AntiPredict(pInputArray, NULL, ...)` of the fast mode from 3320 on).
    pub fn in_place(self) -> bool {
        self == Kind::Fast3320To3930
    }
}

/// The scratch space of the version 3800 to 3929 "extra high" anti-predictor.
#[derive(Default)]
pub struct Scratch {
    adapt: Vec<i16>,
    short: Vec<i16>,
}

/// The coefficients that precede the residuals of an "extra high" array: `count + 1` pairs.
pub struct Coefficients {
    pub count: usize,
    pub a: [i64; 64],
    pub b: [i64; 64],
}

impl Kind {
    /// Anti-predict `inp` into `out` (both `n` long). `inp` is used as scratch space.
    pub fn anti_predict(
        self,
        version: i32,
        inp: &mut [i32],
        out: &mut [i32],
        coefficients: &Coefficients,
        scratch: &mut Scratch,
    ) {
        // Only the in-place anti-predictor has no output.
        debug_assert!(self.in_place() || out.len() == inp.len());
        let (a, b, z) = (&coefficients.a, &coefficients.b, coefficients.count);

        match self {
            Kind::Fast0000To3320 => fast_0000_to_3320(inp, out),
            Kind::Fast3320To3930 => fast_3320_to_3930(inp),
            Kind::Normal0000To3320 => normal_0000_to_3320(inp, out),
            Kind::Normal3320To3800 => normal_3320_to_3800(inp, out),
            Kind::Normal3800To3929 => normal_3800_to_3929(inp, out),
            Kind::High0000To3320 => high_0000_to_3320(inp, out),
            Kind::High3320To3600 => high_3320_to_3600(inp, out),
            Kind::High3600To3700 => high_3600_to_3700(inp, out),
            Kind::High3700To3800 => high_3700_to_3800(inp, out),
            Kind::High3800To3929 => high_3800_to_3929(inp, out),
            Kind::ExtraHigh0000To3320 => {
                for k in (0..=z).rev() {
                    offset_plain(inp, out, b[k], -1, 64);
                    offset_plain(out, inp, a[k], 1, 64);
                }
                high_0000_to_3320(inp, out);
            }
            Kind::ExtraHigh3320To3600 => {
                for k in (0..=z).rev() {
                    offset_adaptive(inp, out, b[k], -1, 32);
                    offset_adaptive(out, inp, a[k], 1, 32);
                }
                high_0000_to_3320(inp, out);
            }
            Kind::ExtraHigh3600To3700 => {
                offset_pairs(inp, out, z, a, b);
                high_3600_to_3700(inp, out);
            }
            Kind::ExtraHigh3700To3800 => {
                offset_pairs(inp, out, z, a, b);
                high_3700_to_3800(inp, out);
            }
            Kind::ExtraHigh3800To3929 => extra_high_3800_to_3929(version, inp, out, scratch),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Fast
// ---------------------------------------------------------------------------------------------

/// CAntiPredictorFast0000To3320::AntiPredict
fn fast_0000_to_3320(inp: &mut [i32], out: &mut [i32]) {
    let n = inp.len();
    if n < 32 {
        out.copy_from_slice(inp);
        return;
    }

    out[0] = inp[0];
    for i in 1..8 {
        out[i] = (w(inp[i]) + w(out[i - 1])).0;
    }

    let mut m = w(4000);
    let mut p = w(out[7]) * w(2) - w(out[6]);
    let mut pw = (p * m) >> 12;

    for i in 8..n {
        out[i] = (w(inp[i]) + pw).0;

        if inp[i] > 0 {
            m += sel(p.0 > 0, 4, -4);
        }
        else if inp[i] < 0 {
            m += sel(p.0 > 0, -4, 4);
        }

        p = w(out[i]) * w(2) - w(out[i - 1]);
        pw = (p * m) >> 12;
    }
}

/// CAntiPredictorFast3320To3930::AntiPredict (no output: it overwrites its input)
fn fast_3320_to_3930(inp: &mut [i32]) {
    let n = inp.len();
    if n < 3 {
        return;
    }

    let mut m = w(375);
    let mut ip2 = w(inp[1]);
    let mut ip3 = w(inp[0]);
    let mut op1 = w(inp[1]);

    #[allow(clippy::needless_range_loop)] // Mirrors the indexing of the SDK.
    for i in 2..n {
        let p = ip2 + ip2 - ip3;

        ip3 = ip2;
        ip2 = w(inp[i]) + ((p * m) >> 9);

        if (w(inp[i]) ^ p).0 > 0 {
            m += w(1);
        }
        else {
            m -= w(1);
        }

        inp[i] = (ip2 + op1).0;
        op1 = w(inp[i]);
    }
}

// ---------------------------------------------------------------------------------------------
// Normal
// ---------------------------------------------------------------------------------------------

/// CAntiPredictorNormal0000To3320::AntiPredict
fn normal_0000_to_3320(inp: &mut [i32], out: &mut [i32]) {
    let n = inp.len();
    if n < 32 {
        out.copy_from_slice(inp);
        return;
    }

    // order 3
    out[..8].copy_from_slice(&inp[..8]);

    let mut m = w(300);
    let mut p = w(out[7]) * w(3) - w(out[6]) * w(3) + w(out[5]);
    let mut pw = (p * m) >> 12;

    for i in 8..n {
        out[i] = (w(inp[i]) + pw).0;

        if inp[i] > 0 {
            m += sel(p.0 > 0, 4, -4);
        }
        else if inp[i] < 0 {
            m += sel(p.0 > 0, -4, 4);
        }

        p = w(out[i]) * w(3) - w(out[i - 1]) * w(3) + w(out[i - 2]);
        pw = (p * m) >> 12;
    }

    // order 2
    inp[..8].copy_from_slice(&out[..8]);
    m = w(3000);

    p = w(inp[7]) * w(2) - w(inp[6]);
    pw = (p * m) >> 12;

    for i in 8..n {
        inp[i] = (w(out[i]) + pw).0;

        if out[i] > 0 {
            m += sel(p.0 > 0, 12, -12);
        }
        else if out[i] < 0 {
            m += sel(p.0 > 0, -12, 12);
        }

        p = w(inp[i]) * w(2) - w(inp[i - 1]);
        pw = (p * m) >> 12;
    }

    // order 1
    out[0] = inp[0];
    for i in 1..8 {
        out[i] = (w(inp[i]) + w(out[i - 1])).0;
    }

    m = w(3900);
    p = w(out[7]);
    pw = (p * m) >> 12;

    for i in 8..n {
        out[i] = (w(inp[i]) + pw).0;

        if inp[i] > 0 {
            m += sel(p.0 > 0, 1, -1);
        }
        else if inp[i] < 0 {
            m += sel(p.0 > 0, -1, 1);
        }

        p = w(out[i]);
        pw = (p * m) >> 12;
    }
}

/// CAntiPredictorNormal3320To3800::AntiPredict
fn normal_3320_to_3800(inp: &mut [i32], out: &mut [i32]) {
    let n = inp.len();
    if n < 8 {
        out.copy_from_slice(inp);
        return;
    }

    out[..5].copy_from_slice(&inp[..5]);

    let mut m1 = w(0);
    let mut m2 = w(64);
    let mut m3 = w(28);

    let mut p3 = w(3) * (w(out[4]) - w(out[3])) + w(out[2]);
    let mut p2 = w(inp[4]) + ((w(inp[2]) - w(inp[3])) << 3) - w(inp[1]) + w(inp[0]);
    let mut p1 = w(out[4]);

    for q in 5..n {
        let op0 = w(inp[q]) + ((p1 * m1) >> 8);
        m1 += if (w(inp[q]) ^ p1).0 > 0 { w(1) } else { w(-1) };
        p1 = op0;

        inp[q] = (op0 + ((p2 * m2) >> 11)).0;
        m2 += if (op0 ^ p2).0 > 0 { w(1) } else { w(-1) };
        p2 = w(inp[q]) + ((w(inp[q - 2]) - w(inp[q - 1])) << 3) - w(inp[q - 3]) + w(inp[q - 4]);

        out[q] = (w(inp[q]) + ((p3 * m3) >> 9)).0;
        m3 += if (w(inp[q]) ^ p3).0 > 0 { w(1) } else { w(-1) };

        p3 = w(3) * (w(out[q]) - w(out[q - 1])) + w(out[q - 2]);
    }

    let mut m4 = w(370);
    let mut m5 = w(3900);

    out[1] = (w(inp[1]) + w(out[0])).0;
    out[2] = (w(inp[2]) + w(out[1])).0;
    out[3] = (w(inp[3]) + w(out[2])).0;
    out[4] = (w(inp[4]) + w(out[3])).0;

    let mut p4 = w(2) * w(inp[4]) - w(inp[3]);
    let mut p5 = w(out[4]);
    let mut ip1 = w(inp[4]);

    #[allow(clippy::needless_range_loop)] // Mirrors the indexing of the SDK.
    for q in 5..n {
        let ip0 = w(out[q]) + ((p4 * m4) >> 9);
        m4 += if (w(out[q]) ^ p4).0 > 0 { w(1) } else { w(-1) };
        p4 = w(2) * ip0 - ip1;

        out[q] = (ip0 + ((p5 * m5) >> 12)).0;
        m5 += if (ip0 ^ p5).0 > 0 { w(1) } else { w(-1) };
        p5 = w(out[q]);

        ip1 = ip0;
    }
}

/// The state of the final stages shared by the 3800 to 3929 anti-predictors.
struct Tail {
    m2: I,
    m3: I,
    m4: I,
    m5: I,
    m6: I,
    p2: I,
    p3: I,
    p4: I,
    p7: I,
    ipp2: I,
    opp: I,
}

impl Tail {
    /// The state after the first `first` elements of `inp` (`out[first - 1]` is not yet
    /// reconstructed: `opp` is the value `op[-1]` had when it was read).
    fn new(inp: &[i32], first: usize) -> Tail {
        Tail {
            m2: w(64),
            m3: w(115),
            m4: w(64),
            m5: w(740),
            m6: w(0),
            p4: w(inp[first - 1]),
            p3: (w(inp[first - 1]) - w(inp[first - 2])) << 1,
            p2: w(inp[first - 1]) + ((w(inp[first - 3]) - w(inp[first - 2])) << 3),
            ipp2: w(inp[first - 2]),
            p7: w(2) * w(inp[first - 1]) - w(inp[first - 2]),
            opp: w(inp[first - 1]),
        }
    }

    /// The rest of the stages for an input residual `i` (after the stages before them), at output
    /// position `q`. `shift_c` is the shift of the third stage.
    #[inline(always)]
    fn step(&mut self, i: I, prev_out: i32, shift_c: usize) -> i32 {
        let mut o = i + (((self.p2 * self.m2) + (self.p3 * self.m3) + (self.p4 * self.m4)) >> 11);

        if i.0 > 0 {
            self.m2 -= sign30(self.p2);
            self.m3 -= sign28(self.p3);
            self.m4 -= sign28(self.p4);
        }
        else if i.0 < 0 {
            self.m2 += sign30(self.p2);
            self.m3 += sign28(self.p3);
            self.m4 += sign28(self.p4);
        }

        self.p2 = o + ((self.ipp2 - self.p4) << 3);
        self.p3 = (o - self.p4) << 1;
        self.ipp2 = self.p4;
        self.p4 = o;

        o += ((self.p7 * self.m5) - (self.opp * self.m6)) >> shift_c;

        if self.p4.0 > 0 {
            self.m5 -= sign29(self.p7);
            self.m6 += sign30(self.opp);
        }
        else if self.p4.0 < 0 {
            self.m5 += sign29(self.p7);
            self.m6 -= sign30(self.opp);
        }

        self.p7 = w(2) * o - self.opp;
        self.opp = o;

        (o + ((w(prev_out) * w(31)) >> 5)).0
    }
}

/// CAntiPredictorNormal3800To3929::AntiPredict
fn normal_3800_to_3929(inp: &mut [i32], out: &mut [i32]) {
    const FIRST: usize = 4;

    let n = inp.len();
    if n < 8 {
        out.copy_from_slice(inp);
        return;
    }

    out[..FIRST].copy_from_slice(&inp[..FIRST]);
    let mut tail = Tail::new(inp, FIRST);

    for q in 1..FIRST {
        out[q] = (w(out[q]) + w(out[q - 1])).0;
    }

    for q in FIRST..n {
        out[q] = tail.step(w(inp[q]), out[q - 1], 10);
    }
}

// ---------------------------------------------------------------------------------------------
// High
// ---------------------------------------------------------------------------------------------

/// CAntiPredictorHigh0000To3320::AntiPredict
fn high_0000_to_3320(inp: &mut [i32], out: &mut [i32]) {
    let n = inp.len();
    if n < 32 {
        out.copy_from_slice(inp);
        return;
    }

    // order 5
    out[..8].copy_from_slice(&inp[..8]);
    let mut m = w(0);

    for q in 8..n {
        let p = w(5) * w(out[q - 1]) - w(10) * w(out[q - 2]) + w(12) * w(out[q - 3])
            - w(7) * w(out[q - 4])
            + w(out[q - 5]);
        let pw = (p * m) >> 12;

        out[q] = (w(inp[q]) + pw).0;

        if inp[q] > 0 {
            m += sel(p.0 > 0, 1, -1);
        }
        else if inp[q] < 0 {
            m += sel(p.0 > 0, -1, 1);
        }
    }

    // order 4
    inp[..8].copy_from_slice(&out[..8]);
    m = w(0);

    for q in 8..n {
        let p = w(4) * w(inp[q - 1]) - w(6) * w(inp[q - 2]) + w(4) * w(inp[q - 3]) - w(inp[q - 4]);
        let pw = (p * m) >> 12;

        inp[q] = (w(out[q]) + pw).0;

        if out[q] > 0 {
            m += sel(p.0 > 0, 2, -2);
        }
        else if out[q] < 0 {
            m += sel(p.0 > 0, -2, 2);
        }
    }

    normal_0000_to_3320(inp, out);
}

/// CAntiPredictorOffset::AntiPredictOffset
fn anti_predict_offset(inp: &[i32], out: &mut [i32], offset: usize, delta_m: i32) {
    let n = inp.len();
    out[..offset].copy_from_slice(&inp[..offset]);

    let mut m = w(0);
    for op in offset..n {
        let ipo = op - offset;
        out[op] = (w(inp[op]) + ((w(out[ipo]) * m) >> 12)).0;

        if (w(out[ipo]) ^ w(inp[op])).0 > 0 {
            m += w(delta_m);
        }
        else {
            m -= w(delta_m);
        }
    }
}

/// CAntiPredictorHigh3320To3600::AntiPredict
fn high_3320_to_3600(inp: &mut [i32], out: &mut [i32]) {
    let n = inp.len();
    if n < 8 {
        out.copy_from_slice(inp);
        return;
    }

    anti_predict_offset(inp, out, 2, 12);
    anti_predict_offset(out, inp, 3, 12);

    anti_predict_offset(inp, out, 4, 12);
    anti_predict_offset(out, inp, 5, 12);

    anti_predict_offset(inp, out, 6, 12);
    anti_predict_offset(out, inp, 7, 12);

    normal_3320_to_3800(inp, out);
}

/// CAntiPredictorHigh3600To3700::AntiPredict
fn high_3600_to_3700(inp: &mut [i32], out: &mut [i32]) {
    let n = inp.len();
    if n < 16 {
        out.copy_from_slice(inp);
        return;
    }

    out[..13].copy_from_slice(&inp[..13]);

    // bm[k] and bp[k] are bm(k + 1) and bp(k + 1) of the SDK. Which comparison each of the taps
    // makes, and in which direction it adapts, is the SDK's: (adapts with `+` instead of `-`,
    // compares `>= 0` instead of `> 0`).
    let mut bm = [w(0); 13];
    let mut bp = [w(0); 13];
    for (k, p) in bp.iter_mut().enumerate() {
        *p = w(out[12 - k]);
    }

    let mut m2 = w(64);
    let mut m3 = w(28);
    let mut m4 = w(16);

    let mut p4 = w(inp[12]);
    let mut p3 = (w(inp[12]) - w(inp[11])) << 1;
    let mut p2 = w(inp[12]) + ((w(inp[10]) - w(inp[11])) << 3);

    // The taps that add instead of subtract, and the ones that compare with `>=`.
    const ADD: [bool; 13] =
        [false, true, false, false, false, false, false, false, false, true, true, true, true];
    const GE: [bool; 13] =
        [false, true, false, true, false, true, false, true, false, true, false, true, false];

    for q in 13..n {
        inp[q] = (w(inp[q]) - w(1)).0;

        let mut op0 = w(inp[q]);
        for k in 0..13 {
            let t = (bp[k] * bm[k]) >> 8;
            if ADD[k] {
                op0 += t;
            }
            else {
                op0 -= t;
            }
        }
        // The first term of the SDK's sum is `inp[q]` followed by `- bp1 * bm1`, as above.

        if inp[q] > 0 {
            for k in 0..13 {
                let c = if GE[k] { bp[k].0 >= 0 } else { bp[k].0 > 0 };
                let d = if c { 1 } else { -1 };
                if ADD[k] {
                    bm[k] += w(d);
                }
                else {
                    bm[k] -= w(d);
                }
            }
        }
        else if inp[q] < 0 {
            for k in 0..13 {
                // The comparison is the complement of the one above.
                let c = if GE[k] { bp[k].0 < 0 } else { bp[k].0 <= 0 };
                let d = if c { 1 } else { -1 };
                if ADD[k] {
                    bm[k] += w(d);
                }
                else {
                    bm[k] -= w(d);
                }
            }
        }

        for k in (1..13).rev() {
            bp[k] = bp[k - 1];
        }
        bp[0] = op0;

        inp[q] = (op0 + ((p2 * m2) >> 11) + ((p3 * m3) >> 9) + ((p4 * m4) >> 9)).0;

        if op0.0 > 0 {
            m2 -= sel(p2.0 > 0, -1, 1);
            m3 -= sel(p3.0 > 0, -1, 1);
            m4 -= sel(p4.0 > 0, -1, 1);
        }
        else if op0.0 < 0 {
            m2 -= sel(p2.0 > 0, 1, -1);
            m3 -= sel(p3.0 > 0, 1, -1);
            m4 -= sel(p4.0 > 0, 1, -1);
        }

        p2 = w(inp[q]) + ((w(inp[q - 2]) - w(inp[q - 1])) << 3);
        p3 = (w(inp[q]) - w(inp[q - 1])) << 1;
        p4 = w(inp[q]);
        out[q] = inp[q];
    }

    m4 = w(370);

    for q in 1..13 {
        out[q] = (w(inp[q]) + w(out[q - 1])).0;
    }

    p4 = w(2) * w(inp[12]) - w(inp[11]);
    let mut p6 = w(0);
    let mut p5 = w(out[12]);
    let mut m6 = w(0);
    let mut ip1 = w(inp[12]);

    #[allow(clippy::needless_range_loop)] // Mirrors the indexing of the SDK.
    for q in 13..n {
        let ip0 = w(out[q]) + ((p4 * m4) >> 9) - ((p6 * m6) >> 10);

        if (w(out[q]) ^ p4).0 >= 0 {
            m4 += w(1);
        }
        else {
            m4 -= w(1);
        }

        if (w(out[q]) ^ p6).0 >= 0 {
            m6 -= w(1);
        }
        else {
            m6 += w(1);
        }

        p4 = w(2) * ip0 - ip1;
        p6 = ip0;

        out[q] = (ip0 + ((p5 * w(31)) >> 5)).0;
        p5 = w(out[q]);

        ip1 = ip0;
    }
}

/// CAntiPredictorHigh3700To3800::AntiPredict
fn high_3700_to_3800(inp: &mut [i32], out: &mut [i32]) {
    const FIRST: usize = 16;

    let n = inp.len();
    if n < 20 {
        out.copy_from_slice(inp);
        return;
    }

    out[..FIRST].copy_from_slice(&inp[..FIRST]);

    let mut bm = [w(0); FIRST];
    let mut m2 = w(64);
    let mut m3 = w(115);
    let mut m4 = w(64);
    let mut m5 = w(740);
    let mut m6 = w(0);

    let mut p4 = w(inp[FIRST - 1]);
    let mut p3 = (w(inp[FIRST - 1]) - w(inp[FIRST - 2])) << 1;
    let mut p2 = w(inp[FIRST - 1]) + ((w(inp[FIRST - 3]) - w(inp[FIRST - 2])) << 3);
    let mut ipp2 = w(inp[FIRST - 2]);
    let mut ipp1 = w(inp[FIRST - 1]);
    let mut p7 = w(2) * w(inp[FIRST - 1]) - w(inp[FIRST - 2]);
    let mut opp = w(out[FIRST - 1]);

    for q in 1..FIRST {
        out[q] = (w(out[q]) + w(out[q - 1])).0;
    }

    for q in FIRST..n {
        let original = w(inp[q]) - w(1);

        let mut dot = w(0);
        for (k, b) in bm.iter().enumerate() {
            dot += w(inp[q - 1 - k]) * *b;
        }
        inp[q] = (original - (dot >> 8)).0;

        if original.0 > 0 {
            for (k, b) in bm.iter_mut().enumerate() {
                let x = w(inp[q - 1 - k]);
                if k % 2 == 0 {
                    *b -= sel(x.0 > 0, 1, -1);
                }
                else {
                    *b += w((((x.0 as u32) >> 30) & 2) as i32) - w(1);
                }
            }
        }
        else if original.0 < 0 {
            for (k, b) in bm.iter_mut().enumerate() {
                let x = w(inp[q - 1 - k]);
                if k % 2 == 0 {
                    *b -= sel(x.0 <= 0, 1, -1);
                }
                else {
                    *b -= w((((x.0 as u32) >> 30) & 2) as i32) - w(1);
                }
            }
        }

        let ip = w(inp[q]);
        let mut o = ip + (((p2 * m2) + (p3 * m3) + (p4 * m4)) >> 11);

        if ip.0 > 0 {
            m2 -= sel(p2.0 > 0, -1, 1);
            m3 -= sel(p3.0 > 0, -4, 4);
            m4 -= sel(p4.0 > 0, -4, 4);
        }
        else if ip.0 < 0 {
            m2 -= sel(p2.0 > 0, 1, -1);
            m3 -= sel(p3.0 > 0, 4, -4);
            m4 -= sel(p4.0 > 0, 4, -4);
        }

        p4 = o;
        p2 = p4 + ((ipp2 - ipp1) << 3);
        p3 = (p4 - ipp1) << 1;
        ipp2 = ipp1;
        ipp1 = p4;

        o += ((p7 * m5) - (opp * m6)) >> 10;

        if (ipp1 ^ p7).0 >= 0 {
            m5 += w(2);
        }
        else {
            m5 -= w(2);
        }

        if (ipp1 ^ opp).0 >= 0 {
            m6 -= w(1);
        }
        else {
            m6 += w(1);
        }

        p7 = w(2) * o - opp;
        opp = o;

        out[q] = (o + ((w(out[q - 1]) * w(31)) >> 5)).0;
    }
}

/// CAntiPredictorHigh3800To3929::AntiPredict
fn high_3800_to_3929(inp: &mut [i32], out: &mut [i32]) {
    const FIRST: usize = 16;

    let n = inp.len();
    if n < 20 {
        out.copy_from_slice(inp);
        return;
    }

    out[..FIRST].copy_from_slice(&inp[..FIRST]);

    let mut bm = [w(0); FIRST];
    let mut tail = Tail::new(inp, FIRST);

    for q in 1..FIRST {
        out[q] = (w(out[q]) + w(out[q - 1])).0;
    }

    for q in FIRST..n {
        let ip = inp[q];
        let history = &inp[q - FIRST..q];

        let mut dot = w(0);
        if ip > 0 {
            for (b, &x) in bm.iter_mut().zip(history) {
                dot += w(x) * *b;
                *b += sign30(w(x));
            }
        }
        else if ip < 0 {
            for (b, &x) in bm.iter_mut().zip(history) {
                dot += w(x) * *b;
                *b -= sign30(w(x));
            }
        }
        else {
            for (b, &x) in bm.iter().zip(history) {
                dot += w(x) * *b;
            }
        }

        inp[q] = (w(ip) - (dot >> 9)).0;

        out[q] = tail.step(w(inp[q]), out[q - 1], 10);
    }
}

// ---------------------------------------------------------------------------------------------
// Extra high
// ---------------------------------------------------------------------------------------------

/// CAntiPredictorExtraHigh0000To3320::AntiPredictorOffset
fn offset_plain(inp: &[i32], out: &mut [i32], g: i64, dm: i32, max_order: usize) {
    let n = inp.len();
    if g == 0 || n <= max_order {
        out.copy_from_slice(inp);
        return;
    }

    out[..max_order].copy_from_slice(&inp[..max_order]);

    // The coefficients are 6 bit values, and the first `max_order` outputs are not predicted.
    let g = g as usize;
    if dm > 0 {
        for q in max_order..n {
            out[q] = (w(inp[q]) + (w(out[q - g]) >> 3)).0;
        }
    }
    else {
        for q in max_order..n {
            out[q] = (w(inp[q]) - (w(out[q - g]) >> 3)).0;
        }
    }
}

/// CAntiPredictorExtraHigh3320To3600::AntiPredictorOffset
fn offset_adaptive(inp: &[i32], out: &mut [i32], g: i64, dm: i32, max_order: usize) {
    let n = inp.len();
    if g == 0 || n <= max_order {
        out.copy_from_slice(inp);
        return;
    }

    out[..max_order].copy_from_slice(&inp[..max_order]);

    let g = g as usize;
    let mut m = w(512);

    if dm > 0 {
        for q in max_order..n {
            out[q] = (w(inp[q]) + ((w(out[q - g]) * m) >> 12)).0;
            if (w(inp[q]) ^ w(out[q - g])).0 > 0 {
                m += w(8);
            }
            else {
                m -= w(8);
            }
        }
    }
    else {
        for q in max_order..n {
            out[q] = (w(inp[q]) - ((w(out[q - g]) * m) >> 12)).0;
            if (w(inp[q]) ^ w(out[q - g])).0 > 0 {
                m -= w(8);
            }
            else {
                m += w(8);
            }
        }
    }
}

/// CAntiPredictorExtraHigh3600To3700::AntiPredictorOffset (identical to the one of 3700 to 3800)
fn offset_double(inp: &[i32], out: &mut [i32], g1: i64, g2: i64, max_order: usize) {
    let n = inp.len();
    if g1 == 0 || g2 == 0 || n <= max_order {
        out.copy_from_slice(inp);
        return;
    }

    out[..max_order].copy_from_slice(&inp[..max_order]);

    let (g1, g2) = (g1 as usize, g2 as usize);
    let mut m = w(64);
    let mut m2 = w(64);

    for q in max_order..n {
        out[q] = (w(inp[q]) + ((w(out[q - g1]) * m) >> 9) - ((w(out[q - g2]) * m2) >> 9)).0;

        if (w(inp[q]) ^ w(out[q - g1])).0 > 0 {
            m += w(1);
        }
        else {
            m -= w(1);
        }

        if (w(inp[q]) ^ w(out[q - g2])).0 > 0 {
            m2 -= w(1);
        }
        else {
            m2 += w(1);
        }
    }
}

/// The loop of AntiPredictCustom of the extra high 3600 to 3700 and 3700 to 3800 versions: the
/// result is left in `inp`.
fn offset_pairs(inp: &mut [i32], out: &mut [i32], count: usize, a: &[i64; 64], b: &[i64; 64]) {
    let mut z = count as i64;
    while z >= 0 {
        offset_double(inp, out, a[z as usize], b[z as usize], 64);
        z -= 1;

        if z >= 0 {
            offset_double(out, inp, a[z as usize], b[z as usize], 64);
            z -= 1;
        }
        else {
            inp.copy_from_slice(out);
            break;
        }
    }
}

/// CAntiPredictorExtraHighHelper::ConventionalDotProduct, for a number of iterations that is a
/// multiple of 32.
fn conventional_dot_product(bip: &[i16], bbm: &mut [i16], adapt: &[i16], op: i32) -> I {
    let mut dot = w(0);

    if op == 0 {
        for (&x, &b) in bip.iter().zip(bbm.iter()) {
            dot += w(i32::from(x)) * w(i32::from(b));
        }
    }
    else if op > 0 {
        for ((&x, b), &a) in bip.iter().zip(bbm.iter_mut()).zip(adapt) {
            dot += w(i32::from(x)) * w(i32::from(*b));
            *b = b.wrapping_add(a);
        }
    }
    else {
        for ((&x, b), &a) in bip.iter().zip(bbm.iter_mut()).zip(adapt) {
            dot += w(i32::from(x)) * w(i32::from(*b));
            *b = b.wrapping_sub(a);
        }
    }

    dot
}

/// CAntiPredictorExtraHigh3800To3929::AntiPredictCustom
fn extra_high_3800_to_3929(version: i32, inp: &mut [i32], out: &mut [i32], scratch: &mut Scratch) {
    let old = version < 3830;
    let filter_elements: usize = if old { 128 } else { 256 };
    let filter_shift = if old { 11 } else { 12 };
    let max_elements: usize = if old { 134 } else { 262 };
    let first: usize = if old { 128 } else { 256 };
    let stage_c_shift = if old { 10 } else { 11 };

    let n = inp.len();
    if n < max_elements {
        out.copy_from_slice(inp);
        return;
    }

    out[..first].copy_from_slice(&inp[..first]);

    let mut bm = [0i16; 256];
    let mut tail = Tail::new(inp, first);

    for q in 1..first {
        out[q] = (w(out[q]) + w(out[q - 1])).0;
    }

    scratch.adapt.resize(n, 0);
    scratch.short.resize(n, 0);
    let (adapt, short) = (&mut scratch.adapt, &mut scratch.short);
    for q in 0..first {
        adapt[q] = sign30(w(inp[q])).0 as i16;
        short[q] = inp[q] as i16;
    }

    let mut fm = [w(0); 9];
    let mut fp = [w(0); 9];

    for q in first..n {
        if version >= 3830 {
            fp[0] = w(inp[q]);

            let mut dot = w(0);
            if fp[0].0 == 0 {
                for k in (1..=8).rev() {
                    dot += fp[k] * fm[k];
                    fp[k] = fp[k - 1];
                }
            }
            else if fp[0].0 > 0 {
                for k in (1..=8).rev() {
                    dot += fp[k] * fm[k];
                    fm[k] += sign30(fp[k]);
                    fp[k] = fp[k - 1];
                }
            }
            else {
                for k in (1..=8).rev() {
                    dot += fp[k] * fm[k];
                    fm[k] -= sign30(fp[k]);
                    fp[k] = fp[k - 1];
                }
            }

            inp[q] = (w(inp[q]) - (dot >> 9)).0;
        }

        let original = inp[q];

        let dot = conventional_dot_product(
            &short[q - first..q - first + filter_elements],
            &mut bm[..filter_elements],
            &adapt[q - first..q - first + filter_elements],
            original,
        );
        inp[q] = (w(inp[q]) - (dot >> filter_shift)).0;

        short[q] = inp[q] as i16;
        adapt[q] = sign30(w(inp[q])).0 as i16;

        out[q] = tail.step(w(inp[q]), out[q - 1], stage_c_shift);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_selection_follows_the_sdk() {
        assert_eq!(Kind::new(1000, 3300).unwrap(), Kind::Fast0000To3320);
        assert_eq!(Kind::new(1000, 3320).unwrap(), Kind::Fast3320To3930);
        assert_eq!(Kind::new(2000, 3799).unwrap(), Kind::Normal3320To3800);
        assert_eq!(Kind::new(2000, 3800).unwrap(), Kind::Normal3800To3929);
        assert_eq!(Kind::new(3000, 3599).unwrap(), Kind::High3320To3600);
        assert_eq!(Kind::new(3000, 3600).unwrap(), Kind::High3600To3700);
        assert_eq!(Kind::new(3000, 3700).unwrap(), Kind::High3700To3800);
        assert_eq!(Kind::new(4000, 3910).unwrap(), Kind::ExtraHigh3800To3929);
        assert!(Kind::new(5000, 3910).is_err());
    }

    #[test]
    fn short_arrays_are_copied() {
        let coefficients = Coefficients { count: 0, a: [0; 64], b: [0; 64] };
        let mut scratch = Scratch::default();
        for kind in [
            Kind::Fast0000To3320,
            Kind::Normal0000To3320,
            Kind::Normal3320To3800,
            Kind::Normal3800To3929,
            Kind::High0000To3320,
            Kind::High3320To3600,
            Kind::High3600To3700,
            Kind::High3700To3800,
            Kind::High3800To3929,
            Kind::ExtraHigh0000To3320,
            Kind::ExtraHigh3800To3929,
        ] {
            let mut inp: Vec<i32> = (0..5).collect();
            let mut out = vec![0; 5];
            kind.anti_predict(3900, &mut inp, &mut out, &coefficients, &mut scratch);
            assert_eq!(out, [0, 1, 2, 3, 4], "{kind:?}");
        }
    }
}

/// The anti-predictors against the SDK: the hashes of the output of the SDK's `CAntiPredictor*`
/// classes (MACLib/Old, compiled with `-fwrapv`) for the pseudo-random input below.
#[cfg(test)]
mod sdk_vectors {
    use super::*;

    fn xorshift(state: &mut u64) -> u64 {
        *state ^= *state << 13;
        *state ^= *state >> 7;
        *state ^= *state << 17;
        *state
    }

    /// FNV-1a over the 32-bit words.
    fn hash(words: &[i32]) -> u64 {
        words.iter().fold(1469598103934665603, |h, &v| {
            (h ^ u64::from(v as u32)).wrapping_mul(1099511628211)
        })
    }

    /// Run the anti-predictor of `(level, version)` on `n` pseudo-random values.
    ///
    /// `mode` 1 is 16-bit residuals, 3 is residuals up to 28 bits, which make the arithmetic
    /// wrap. `rep` perturbs the seed.
    fn run(level: u32, version: i32, n: usize, mode: u64, rep: u64) -> u64 {
        let mut st: u64 = 0x9E3779B97F4A7C15
            ^ (u64::from(level) * 131
                + version as u64 * 7
                + n as u64 * 13
                + mode * 1000003
                + rep * 77);
        let mut inp: Vec<i32> = (0..n)
            .map(|_| {
                let r = xorshift(&mut st);
                if mode == 1 {
                    (r % 65537) as i32 - 32768
                }
                else {
                    ((r >> 16) as u32 as i32) % (1 << 28) - (1 << 27)
                }
            })
            .collect();
        let mut out = vec![0i32; n];

        let kind = Kind::new(level, version).unwrap();
        let mut co = Coefficients { count: 0, a: [0; 64], b: [0; 64] };
        if let Some((count_bits, value_bits)) = kind.coefficient_bits() {
            co.count = (xorshift(&mut st) % (1u64 << count_bits)) as usize;
            for z in 0..=co.count {
                co.a[z] = (xorshift(&mut st) % (1u64 << value_bits)) as i64;
                co.b[z] = (xorshift(&mut st) % (1u64 << value_bits)) as i64;
            }
        }

        let mut scratch = Scratch::default();
        if kind.in_place() {
            kind.anti_predict(version, &mut inp, &mut [], &co, &mut scratch);
            hash(&inp)
        }
        else {
            kind.anti_predict(version, &mut inp, &mut out, &co, &mut scratch);
            hash(&out)
        }
    }

    #[test]
    fn anti_predictors_match_the_sdk() {
        // (level, version, hash of 513 16-bit values, hash of 2000 28-bit values)
        const VECTORS: &[(u32, i32, u64, u64)] = &[
            (1000, 3000, 0x91494f16087e8686, 0xe7cec056e3a5f8f6),
            (1000, 3200, 0x7a716aaa475640f3, 0xb01961b0309b3d19),
            (1000, 3319, 0x6dcbe4690d06ceba, 0x1a918da47a8ab4f6),
            (1000, 3320, 0x559442a4b0281d16, 0x42280f7b8e6ef608),
            (1000, 3500, 0x05c9e03cec284f7f, 0x33a38f14e7b0adbe),
            (1000, 3599, 0xd5b051d20fd7ccf7, 0xd34a170161eac1e5),
            (1000, 3600, 0xe8bbe2f51ab4afac, 0xea99fc1c3e73bc34),
            (1000, 3650, 0x5a11340a953443d3, 0xba62e5c1099dbbf9),
            (1000, 3699, 0xe66c3c6b82c5c972, 0x26652e530f40c39c),
            (1000, 3700, 0x7d206f26a92513fa, 0x6e43dc6471ca62bb),
            (1000, 3750, 0xab49f668fa4040c1, 0x912e75e85a4ac3e1),
            (1000, 3799, 0x93cf43f757635f48, 0xdf0986fd2eb538f5),
            (1000, 3800, 0xa1a99ef180fa9b37, 0x89189cc4aab69c46),
            (1000, 3820, 0x0723e8c345412034, 0x5e91f92c54be299e),
            (1000, 3829, 0x0128919afa61ea56, 0xb9a209752b4ff0ad),
            (1000, 3830, 0x9ff6a0be7db50e8e, 0x11d53cd27175a0c2),
            (1000, 3880, 0xe1e207e2c1fb35cc, 0x585ca82326558a93),
            (1000, 3900, 0x881afc8dc3753fba, 0x82051f2314ea4362),
            (1000, 3929, 0xb2dedeecec45ddcd, 0x1d54a4ee2310ead2),
            (2000, 3000, 0xaf4582fb3670d0ad, 0x6daec661c378bb8c),
            (2000, 3200, 0x2851d6e3ea625cb5, 0x7d8e93257b1ccea8),
            (2000, 3319, 0x62a64316035b0b3f, 0x37ca524dd690b8bb),
            (2000, 3320, 0x5ad375db4c0c81d3, 0x45e653c417e28b0b),
            (2000, 3500, 0x7cc2bead1a64022e, 0x840f5cb741e607e1),
            (2000, 3599, 0xe827d0413be911fb, 0xc124b58b99ecd952),
            (2000, 3600, 0xf737cbe89417672c, 0xe9186f43a1247585),
            (2000, 3650, 0x491328879d233317, 0x86589d5eb85722d9),
            (2000, 3699, 0x74c300d36922eed0, 0x7675b4d5e53a7ffd),
            (2000, 3700, 0x016931bf0ccbdb7c, 0x6a1934cb913b8d98),
            (2000, 3750, 0x70ce3ac10821d13e, 0xa0a2cedb978267ad),
            (2000, 3799, 0x11cb0a9cba95c5a2, 0x38381bb51c906f35),
            (2000, 3800, 0xda89ff69e421f78c, 0x6eeb96a8e17415a0),
            (2000, 3820, 0xe114dc857ec9fa6c, 0x75152d8eceac8ff2),
            (2000, 3829, 0x9670d24af55a0d79, 0x1ef5f095f30b782d),
            (2000, 3830, 0xfb7c308dd79dea43, 0x3d2d1d3bb1112f54),
            (2000, 3880, 0x16d3af8efcb26bf0, 0x9c3cce7be7cacf1c),
            (2000, 3900, 0xbab528913dad09ec, 0xcd7c31adef650524),
            (2000, 3929, 0x9f9a95c80f519728, 0x01bd711bbc5cdf6e),
            (3000, 3000, 0x60f57fbb4a2340fd, 0x497fc1d25193f569),
            (3000, 3200, 0xb24b476640c682cd, 0xcacf00f9779e6176),
            (3000, 3319, 0x61adf56fa74d1f5c, 0x169518266288ce71),
            (3000, 3320, 0x540d7f9a2044452e, 0x5c5d5c402cf12144),
            (3000, 3500, 0x0848d8e41ce4af26, 0x4f6840fcb8cdcaf4),
            (3000, 3599, 0xf8ad5fe945e59e69, 0x71818dcdccbe08c2),
            (3000, 3600, 0xbbed195bf91e3ff2, 0xc02b57d886b967bf),
            (3000, 3650, 0xe647983535eb8280, 0x2d1a5625e29008df),
            (3000, 3699, 0x8568706bd647b4b8, 0x28de5f160c08382d),
            (3000, 3700, 0x40b1b694bd947da4, 0xcae66f4700206bcf),
            (3000, 3750, 0x2a70280809cb0828, 0x6f379c0e15c36523),
            (3000, 3799, 0x65b8d720f7c76ef8, 0x19c7079c528f3a64),
            (3000, 3800, 0x990d63d26f63d9b5, 0x3688b50a590d4436),
            (3000, 3820, 0x3bbef5072b4ea701, 0xb81a2209c4610935),
            (3000, 3829, 0x24205c7946ce23eb, 0x8f28120bc33f30b9),
            (3000, 3830, 0xd031b07b617606d3, 0xc740c98c254d7f49),
            (3000, 3880, 0x5b4bc4cb8553cdd0, 0x2d7825166056d1ad),
            (3000, 3900, 0x333729b23a317039, 0xcbeb1e1332bbe9a5),
            (3000, 3929, 0x854fe0ed66cf8cdc, 0x6fcfe87edc696660),
            (4000, 3000, 0x9233e088a1415b62, 0x3795653b897ac5eb),
            (4000, 3200, 0x1bd8dbd143bfe4ac, 0x294cdcec2fd5f61c),
            (4000, 3319, 0xb74caa9a089f3fc7, 0x0f7a4b652661cd51),
            (4000, 3320, 0x9a9a4e60368dde28, 0xadf0b0152633cbc7),
            (4000, 3500, 0x161d8baf22a14397, 0x4bc94c1e845a09ef),
            (4000, 3599, 0x45f92f2debe04993, 0x8da74e7a3cfaca85),
            (4000, 3600, 0xb0f2a9b39d29b4e7, 0x0f696bf502c08206),
            (4000, 3650, 0x1f9b9adbd5e4a628, 0xff528a8c31968877),
            (4000, 3699, 0x0013781259538ebc, 0xb54e0528d3715423),
            (4000, 3700, 0xfa03ad7b394bdbf3, 0x7dfbb376b40a9f1e),
            (4000, 3750, 0xee49d62d1cfe92fd, 0x5cfc44b46987922c),
            (4000, 3799, 0x16370260428f304d, 0xa673451a938fe62b),
            (4000, 3800, 0xaf7241b98816e8e9, 0x07335604d0dbce19),
            (4000, 3820, 0x2065b4ee0e1bcf06, 0x655398effcea19a9),
            (4000, 3829, 0xd65d148e2a084975, 0x92b5b9a3d5444206),
            (4000, 3830, 0x6b0bec1a834e8b4d, 0xc6e69aed8a81d951),
            (4000, 3880, 0xf7b551344da5f01f, 0x442aa907984685f2),
            (4000, 3900, 0xa25d61494473fc20, 0x5514875201dfd9de),
            (4000, 3929, 0xd6813b99d668178b, 0xf1a552c51d3c38a2),
        ];

        for &(level, version, small, large) in VECTORS {
            assert_eq!(run(level, version, 513, 1, 0), small, "level {level} version {version}");
            assert_eq!(run(level, version, 2000, 3, 1), large, "level {level} version {version}");
        }
    }
}
