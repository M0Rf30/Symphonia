// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The MPEG audio 32-band polyphase synthesis filter, in floating point.
//!
//! This follows FFmpeg's `ff_mpa_synth_filter_float` (`libavcodec/mpegaudiodsp_template.c`,
//! `libavcodec/dct32_template.c`, `libavcodec/mpegaudiodsp_data.c`) operation by operation, since
//! the QDM2 decoder output is defined by it.

/// Number of subbands.
pub(crate) const SBLIMIT: usize = 32;

/// Size of the synthesis ring buffer, with the room for the wrap-around copy.
const SYNTH_BUF_LEN: usize = 1024;

/// Half of the MPEG encoding window, scaled by 2^16 (`ff_mpa_enwindow`, ISO/IEC 11172-3 Table
/// 3-B.3 "D[i]").
#[rustfmt::skip]
static ENWINDOW: [i32; 257] = [
    0, -1, -1, -1, -1, -1, -1, -2,
    -2, -2, -2, -3, -3, -4, -4, -5,
    -5, -6, -7, -7, -8, -9, -10, -11,
    -13, -14, -16, -17, -19, -21, -24, -26,
    -29, -31, -35, -38, -41, -45, -49, -53,
    -58, -63, -68, -73, -79, -85, -91, -97,
    -104, -111, -117, -125, -132, -139, -147, -154,
    -161, -169, -176, -183, -190, -196, -202, -208,
    213, 218, 222, 225, 227, 228, 228, 227,
    224, 221, 215, 208, 200, 189, 177, 163,
    146, 127, 106, 83, 57, 29, -2, -36,
    -72, -111, -153, -197, -244, -294, -347, -401,
    -459, -519, -581, -645, -711, -779, -848, -919,
    -991, -1064, -1137, -1210, -1283, -1356, -1428, -1498,
    -1567, -1634, -1698, -1759, -1817, -1870, -1919, -1962,
    -2001, -2032, -2057, -2075, -2085, -2087, -2080, -2063,
    2037, 2000, 1952, 1893, 1822, 1739, 1644, 1535,
    1414, 1280, 1131, 970, 794, 605, 402, 185,
    -45, -288, -545, -814, -1095, -1388, -1692, -2006,
    -2330, -2663, -3004, -3351, -3705, -4063, -4425, -4788,
    -5153, -5517, -5879, -6237, -6589, -6935, -7271, -7597,
    -7910, -8209, -8491, -8755, -8998, -9219, -9416, -9585,
    -9727, -9838, -9916, -9959, -9966, -9935, -9863, -9750,
    -9592, -9389, -9139, -8840, -8492, -8092, -7640, -7134,
    6574, 5959, 5288, 4561, 3776, 2935, 2037, 1082,
    70, -998, -2122, -3300, -4533, -5818, -7154, -8540,
    -9975, -11455, -12980, -14548, -16155, -17799, -19478, -21189,
    -22929, -24694, -26482, -28289, -30112, -31947, -33791, -35640,
    -37489, -39336, -41176, -43006, -44821, -46617, -48390, -50137,
    -51853, -53534, -55178, -56778, -58333, -59838, -61289, -62684,
    -64019, -65290, -66494, -67629, -68692, -69679, -70590, -71420,
    -72169, -72835, -73415, -73908, -74313, -74630, -74856, -74992,
    75038,
];

/// The synthesis window of the filter, `ff_mpa_synth_window_float`.
pub(crate) struct SynthWindow {
    window: [f32; 512],
}

impl SynthWindow {
    pub(crate) fn new() -> Self {
        let mut window = [0f32; 512];

        for (i, &e) in ENWINDOW.iter().enumerate() {
            // The 23 fractional bits of the subband samples and 16 of the window.
            let mut v = (f64::from(e) / (1u64 << 39) as f64) as f32;
            window[i] = v;
            if (i & 63) != 0 {
                v = -v;
            }
            if i != 0 {
                window[512 - i] = v;
            }
        }

        SynthWindow { window }
    }
}

/// The state of the synthesis filter of one channel.
pub(crate) struct SynthFilter {
    buf: [f32; SYNTH_BUF_LEN],
    offset: usize,
}

impl SynthFilter {
    pub(crate) fn new() -> Self {
        SynthFilter { buf: [0.0; SYNTH_BUF_LEN], offset: 0 }
    }

    /// Synthesizes 32 PCM samples from 32 subband samples. The samples are written to
    /// `out[0]`, `out[incr]`, ..., `out[31 * incr]`.
    pub(crate) fn synth(
        &mut self,
        window: &SynthWindow,
        sb_samples: &[f32; SBLIMIT],
        out: &mut [f32],
        incr: usize,
    ) {
        let base = self.offset;
        let synth = &mut self.buf[base..];

        dct32(&mut synth[..32], sb_samples);
        apply_window(synth, &window.window, out, incr);

        self.offset = (base.wrapping_sub(32)) & 511;
    }
}

/// Applies the synthesis window, `ff_mpadsp_apply_window_float`.
fn apply_window(synth: &mut [f32], window: &[f32; 512], out: &mut [f32], incr: usize) {
    // Copy to avoid wrap.
    synth.copy_within(0..32, 512);

    let w = window;
    let mut sum = 0f32;

    // The sum of 8 products spaced 64 apart, accumulated with the given sign.
    macro_rules! sum8 {
        ($sign:tt, $sum:ident, $w:expr, $p:expr) => {
            for k in 0..8 {
                $sum $sign w[$w + k * 64] * synth[$p + k * 64];
            }
        };
    }

    sum8!(+=, sum, 0, 16);
    sum8!(-=, sum, 32, 48);
    set(out, 0, sum);
    sum = 0.0;

    let mut w1 = 1usize;
    let mut w2 = 31usize;

    for j in 1..16 {
        let mut sum2 = 0f32;

        let p = 16 + j;
        for k in 0..8 {
            let tmp = synth[p + k * 64];
            sum += w[w1 + k * 64] * tmp;
            sum2 -= w[w2 + k * 64] * tmp;
        }
        let p = 48 - j;
        for k in 0..8 {
            let tmp = synth[p + k * 64];
            sum -= w[w1 + 32 + k * 64] * tmp;
            sum2 -= w[w2 + 32 + k * 64] * tmp;
        }

        set(out, j * incr, sum);
        sum = 0.0;
        sum += sum2;
        set(out, (32 - j) * incr, sum);
        sum = 0.0;

        w1 += 1;
        w2 -= 1;
    }

    sum8!(-=, sum, w1 + 32, 32);
    set(out, 16 * incr, sum);
}

/// Writes a sample if the output is large enough.
#[inline]
fn set(out: &mut [f32], idx: usize, v: f32) {
    if let Some(o) = out.get_mut(idx) {
        *o = v;
    }
}

const COS0_0: f32 = (0.50060299823519630134_f64 / 2.0) as f32;
const COS0_1: f32 = (0.50547095989754365998_f64 / 2.0) as f32;
const COS0_2: f32 = (0.51544730992262454697_f64 / 2.0) as f32;
const COS0_3: f32 = (0.53104259108978417447_f64 / 2.0) as f32;
const COS0_4: f32 = (0.55310389603444452782_f64 / 2.0) as f32;
const COS0_5: f32 = (0.58293496820613387367_f64 / 2.0) as f32;
const COS0_6: f32 = (0.62250412303566481615_f64 / 2.0) as f32;
const COS0_7: f32 = (0.67480834145500574602_f64 / 2.0) as f32;
const COS0_8: f32 = (0.74453627100229844977_f64 / 2.0) as f32;
const COS0_9: f32 = (0.83934964541552703873_f64 / 2.0) as f32;
const COS0_10: f32 = (0.97256823786196069369_f64 / 2.0) as f32;
const COS0_11: f32 = (1.16943993343288495515_f64 / 4.0) as f32;
const COS0_12: f32 = (1.48416461631416627724_f64 / 4.0) as f32;
const COS0_13: f32 = (2.05778100995341155085_f64 / 8.0) as f32;
const COS0_14: f32 = (3.40760841846871878570_f64 / 8.0) as f32;
const COS0_15: f32 = (10.19000812354805681150_f64 / 32.0) as f32;
const COS1_0: f32 = (0.50241928618815570551_f64 / 2.0) as f32;
const COS1_1: f32 = (0.52249861493968888062_f64 / 2.0) as f32;
const COS1_2: f32 = (0.56694403481635770368_f64 / 2.0) as f32;
const COS1_3: f32 = (0.64682178335999012954_f64 / 2.0) as f32;
const COS1_4: f32 = (0.78815462345125022473_f64 / 2.0) as f32;
const COS1_5: f32 = (1.06067768599034747134_f64 / 4.0) as f32;
const COS1_6: f32 = (1.72244709823833392782_f64 / 4.0) as f32;
const COS1_7: f32 = (5.10114861868916385802_f64 / 16.0) as f32;
const COS2_0: f32 = (0.50979557910415916894_f64 / 2.0) as f32;
const COS2_1: f32 = (0.60134488693504528054_f64 / 2.0) as f32;
const COS2_2: f32 = (0.89997622313641570463_f64 / 2.0) as f32;
const COS2_3: f32 = (2.56291544774150617881_f64 / 8.0) as f32;
const COS3_0: f32 = (0.54119610014619698439_f64 / 2.0) as f32;
const COS3_1: f32 = (1.30656296487637652785_f64 / 4.0) as f32;
const COS4_0: f32 = (std::f64::consts::FRAC_1_SQRT_2 / 2.0) as f32;

/// Butterfly with a table input (`BF0`).
macro_rules! bf0 {
    ($v:ident, $tab:ident, $a:expr, $b:expr, $c:expr, $s:expr) => {{
        let tmp0 = $tab[$a] + $tab[$b];
        let tmp1 = $tab[$a] - $tab[$b];
        $v[$a] = tmp0;
        $v[$b] = ((1u32 << $s) as f32 * $c) * tmp1;
    }};
}

/// Butterfly (`BF`).
macro_rules! bf {
    ($v:ident, $a:expr, $b:expr, $c:expr, $s:expr) => {{
        let tmp0 = $v[$a] + $v[$b];
        let tmp1 = $v[$a] - $v[$b];
        $v[$a] = tmp0;
        $v[$b] = ((1u32 << $s) as f32 * $c) * tmp1;
    }};
}

macro_rules! bf1 {
    ($v:ident, $a:expr, $b:expr, $c:expr, $d:expr) => {{
        bf!($v, $a, $b, COS4_0, 1);
        bf!($v, $c, $d, -COS4_0, 1);
        $v[$c] += $v[$d];
    }};
}

macro_rules! bf2 {
    ($v:ident, $a:expr, $b:expr, $c:expr, $d:expr) => {{
        bf!($v, $a, $b, COS4_0, 1);
        bf!($v, $c, $d, -COS4_0, 1);
        $v[$c] += $v[$d];
        $v[$a] += $v[$c];
        $v[$c] += $v[$b];
        $v[$b] += $v[$d];
    }};
}

/// The 32-point DCT without the 1/sqrt(2) zero coefficient scaling, `ff_dct32_float`.
#[allow(clippy::too_many_lines)]
fn dct32(out: &mut [f32], tab: &[f32; SBLIMIT]) {
    let mut v = [0f32; SBLIMIT];

    bf0!(v, tab, 0, 31, COS0_0, 1);
    bf0!(v, tab, 15, 16, COS0_15, 5);
    bf!(v, 0, 15, COS1_0, 1);
    bf!(v, 16, 31, -COS1_0, 1);
    bf0!(v, tab, 7, 24, COS0_7, 1);
    bf0!(v, tab, 8, 23, COS0_8, 1);
    bf!(v, 7, 8, COS1_7, 4);
    bf!(v, 23, 24, -COS1_7, 4);
    bf!(v, 0, 7, COS2_0, 1);
    bf!(v, 8, 15, -COS2_0, 1);
    bf!(v, 16, 23, COS2_0, 1);
    bf!(v, 24, 31, -COS2_0, 1);
    bf0!(v, tab, 3, 28, COS0_3, 1);
    bf0!(v, tab, 12, 19, COS0_12, 2);
    bf!(v, 3, 12, COS1_3, 1);
    bf!(v, 19, 28, -COS1_3, 1);
    bf0!(v, tab, 4, 27, COS0_4, 1);
    bf0!(v, tab, 11, 20, COS0_11, 2);
    bf!(v, 4, 11, COS1_4, 1);
    bf!(v, 20, 27, -COS1_4, 1);
    bf!(v, 3, 4, COS2_3, 3);
    bf!(v, 11, 12, -COS2_3, 3);
    bf!(v, 19, 20, COS2_3, 3);
    bf!(v, 27, 28, -COS2_3, 3);
    bf!(v, 0, 3, COS3_0, 1);
    bf!(v, 4, 7, -COS3_0, 1);
    bf!(v, 8, 11, COS3_0, 1);
    bf!(v, 12, 15, -COS3_0, 1);
    bf!(v, 16, 19, COS3_0, 1);
    bf!(v, 20, 23, -COS3_0, 1);
    bf!(v, 24, 27, COS3_0, 1);
    bf!(v, 28, 31, -COS3_0, 1);
    bf0!(v, tab, 1, 30, COS0_1, 1);
    bf0!(v, tab, 14, 17, COS0_14, 3);
    bf!(v, 1, 14, COS1_1, 1);
    bf!(v, 17, 30, -COS1_1, 1);
    bf0!(v, tab, 6, 25, COS0_6, 1);
    bf0!(v, tab, 9, 22, COS0_9, 1);
    bf!(v, 6, 9, COS1_6, 2);
    bf!(v, 22, 25, -COS1_6, 2);
    bf!(v, 1, 6, COS2_1, 1);
    bf!(v, 9, 14, -COS2_1, 1);
    bf!(v, 17, 22, COS2_1, 1);
    bf!(v, 25, 30, -COS2_1, 1);
    bf0!(v, tab, 2, 29, COS0_2, 1);
    bf0!(v, tab, 13, 18, COS0_13, 3);
    bf!(v, 2, 13, COS1_2, 1);
    bf!(v, 18, 29, -COS1_2, 1);
    bf0!(v, tab, 5, 26, COS0_5, 1);
    bf0!(v, tab, 10, 21, COS0_10, 1);
    bf!(v, 5, 10, COS1_5, 2);
    bf!(v, 21, 26, -COS1_5, 2);
    bf!(v, 2, 5, COS2_2, 1);
    bf!(v, 10, 13, -COS2_2, 1);
    bf!(v, 18, 21, COS2_2, 1);
    bf!(v, 26, 29, -COS2_2, 1);
    bf!(v, 1, 2, COS3_1, 2);
    bf!(v, 5, 6, -COS3_1, 2);
    bf!(v, 9, 10, COS3_1, 2);
    bf!(v, 13, 14, -COS3_1, 2);
    bf!(v, 17, 18, COS3_1, 2);
    bf!(v, 21, 22, -COS3_1, 2);
    bf!(v, 25, 26, COS3_1, 2);
    bf!(v, 29, 30, -COS3_1, 2);
    bf1!(v, 0, 1, 2, 3);
    bf2!(v, 4, 5, 6, 7);
    bf1!(v, 8, 9, 10, 11);
    bf2!(v, 12, 13, 14, 15);
    bf1!(v, 16, 17, 18, 19);
    bf2!(v, 20, 21, 22, 23);
    bf1!(v, 24, 25, 26, 27);
    bf2!(v, 28, 29, 30, 31);
    v[8] += v[12];
    v[12] += v[10];
    v[10] += v[14];
    v[14] += v[9];
    v[9] += v[13];
    v[13] += v[11];
    v[11] += v[15];
    out[0] = v[0];
    out[16] = v[1];
    out[8] = v[2];
    out[24] = v[3];
    out[4] = v[4];
    out[20] = v[5];
    out[12] = v[6];
    out[28] = v[7];
    out[2] = v[8];
    out[18] = v[9];
    out[10] = v[10];
    out[26] = v[11];
    out[6] = v[12];
    out[22] = v[13];
    out[14] = v[14];
    out[30] = v[15];
    v[24] += v[28];
    v[28] += v[26];
    v[26] += v[30];
    v[30] += v[25];
    v[25] += v[29];
    v[29] += v[27];
    v[27] += v[31];
    out[1] = v[16] + v[24];
    out[17] = v[17] + v[25];
    out[9] = v[18] + v[26];
    out[25] = v[19] + v[27];
    out[5] = v[20] + v[28];
    out[21] = v[21] + v[29];
    out[13] = v[22] + v[30];
    out[29] = v[23] + v[31];
    out[3] = v[24] + v[20];
    out[19] = v[25] + v[21];
    out[11] = v[26] + v[22];
    out[27] = v[27] + v[23];
    out[7] = v[28] + v[18];
    out[23] = v[29] + v[19];
    out[15] = v[30] + v[17];
    out[31] = v[31];
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The window must be anti-symmetric around the centre as described in the standard.
    #[test]
    fn verify_window_values() {
        let w = SynthWindow::new();
        assert_eq!(w.window[0], 0.0);
        assert_eq!(w.window[256], 75038.0 / (1u64 << 39) as f32);
        assert_eq!(w.window[255], -w.window[257]);
    }

    /// The DCT32 must match the direct evaluation of the DCT-II, `out[k] = sum(x[n] *
    /// cos((2n + 1) * k * pi / 64))`.
    #[test]
    fn verify_dct32_against_matrix() {
        let mut x = [0f32; SBLIMIT];
        for (i, v) in x.iter_mut().enumerate() {
            *v = ((i * 37 + 11) % 23) as f32 - 11.0;
        }
        let mut y = [0f32; SBLIMIT];
        dct32(&mut y, &x);

        for (k, &out) in y.iter().enumerate() {
            let expected: f64 = x
                .iter()
                .enumerate()
                .map(|(n, &v)| {
                    f64::from(v)
                        * (std::f64::consts::PI * (2 * n + 1) as f64 * k as f64 / 64.0).cos()
                })
                .sum();
            assert!((f64::from(out) - expected).abs() < 1e-3, "bin {k}");
        }
    }
}
