// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The hybrid analysis and synthesis filterbank of MPEG Surround (ISO/IEC 23003-1 6.6.2): the
//! lowest three QMF bands are split into 8, 2 and 2 subbands, giving 71 hybrid bands for 64 QMF
//! bands.
//!
//! The structure follows `ixheaacd_mps_qmf_hybrid_analysis` of libxaac (Apache-2.0).

use symphonia_core::dsp::complex::Complex64;

use super::tables::{HYB_COSINE, HYB_FILT_2, HYB_FILT_8, HYB_SINE};

type C = Complex64;

/// The order of the hybrid filters (13 taps).
const TAPS: usize = 13;

/// The number of QMF bands that are split.
const SPLIT_BANDS: usize = 3;

/// The maximum number of hybrid bands.
pub(super) const HYBRID_BANDS: usize = 64 - SPLIT_BANDS + 10;

/// The analysis filterbank state: the last 12 slots of the split bands. The split bands carry
/// the filter delay of 6 slots; the caller feeds them 6 slots ahead of the other bands.
pub(super) struct HybridAnalysis {
    lf: [[C; TAPS - 1]; SPLIT_BANDS],
}

impl HybridAnalysis {
    pub fn new() -> Self {
        HybridAnalysis { lf: [[C::default(); TAPS - 1]; SPLIT_BANDS] }
    }

    /// Filter one QMF slot of `bands` bands into `out[..bands + 7]`.
    pub fn push_slot(&mut self, qmf: &[C], out: &mut [C]) {
        let bands = qmf.len();

        for (k, hist) in self.lf.iter_mut().enumerate() {
            let mut win = [C::default(); TAPS];
            win[..TAPS - 1].copy_from_slice(hist);
            win[TAPS - 1] = qmf[k];

            hist.rotate_left(1);
            hist[TAPS - 2] = qmf[k];

            match k {
                0 => {
                    let s = filter_8(&win);

                    for i in 0..2 {
                        out[i] = s[i + 6];
                        out[i + 2] = s[i];
                        out[i + 4] = s[i + 2] + s[5 - i];
                    }
                }
                1 => {
                    let s = filter_2(&win);
                    out[6] = s[1];
                    out[7] = s[0];
                }
                _ => {
                    let s = filter_2(&win);
                    out[8] = s[0];
                    out[9] = s[1];
                }
            }
        }

        let high = bands.saturating_sub(SPLIT_BANDS);
        out[10..10 + high].copy_from_slice(&qmf[SPLIT_BANDS..SPLIT_BANDS + high]);
    }
}

/// The 8-band split of the lowest QMF band (`ixheaacd_mps_hyb_filt_type1`).
fn filter_8(input: &[C; TAPS]) -> [C; 8] {
    let mut x0 = [C::default(); TAPS];

    for n in 0..TAPS {
        x0[n] = input[n] * f64::from(HYB_FILT_8[TAPS - 1 - n]);
    }

    let x1 = [
        x0[6],
        x0[7],
        x0[8] - x0[0],
        x0[9] - x0[1],
        x0[10] - x0[2],
        x0[11] - x0[3],
        x0[12] - x0[4],
        -x0[5],
    ];

    let acc_re = [
        x1[0].re,
        x1[1].re - x1[7].re,
        x1[2].re - x1[6].re,
        x1[3].re - x1[5].re,
        x1[1].im + x1[7].im,
        x1[2].im + x1[6].im,
        x1[3].im + x1[5].im,
        x1[4].im,
    ];

    let acc_im = [
        x1[0].im,
        x1[1].im - x1[7].im,
        x1[2].im - x1[6].im,
        x1[3].im - x1[5].im,
        x1[1].re + x1[7].re,
        x1[2].re + x1[6].re,
        x1[3].re + x1[5].re,
        x1[4].re,
    ];

    let mut out = [C::default(); 8];

    for q in 0..4 {
        let cos = |i: usize| f64::from(HYB_COSINE[q][i]);
        let sin = |i: usize| f64::from(HYB_SINE[q][i]);

        let mut re_h = acc_re[0] + acc_re[1] * cos(1) + acc_re[2] * cos(2) + acc_re[3] * cos(3);
        let re_l = re_h
            - acc_re[4] * sin(1)
            - acc_re[5] * sin(2)
            - acc_re[6] * sin(3)
            - acc_re[7] * sin(4);
        re_h -= re_l - re_h;

        let mut im_h = acc_im[0] + acc_im[1] * cos(1) + acc_im[2] * cos(2) + acc_im[3] * cos(3);
        let im_l = im_h
            + acc_im[4] * sin(1)
            + acc_im[5] * sin(2)
            + acc_im[6] * sin(3)
            + acc_im[7] * sin(4);
        im_h -= im_l - im_h;

        out[q] = C::new(re_l, im_l);
        out[7 - q] = C::new(re_h, im_h);
    }

    out
}

/// The 2-band split of the second and third QMF band (`ixheaacd_mps_hyb_filt_type2`).
fn filter_2(input: &[C; TAPS]) -> [C; 2] {
    let tap = |n: usize| f64::from(HYB_FILT_2[TAPS - 1 - n]);

    let mut odd = C::default();

    for n in [1, 3, 5] {
        odd += (input[n] + input[12 - n]) * tap(n);
    }

    let center = input[6] * tap(6);

    [center + odd, center - odd]
}

/// Merge the hybrid bands back into `bands` QMF bands.
pub(super) fn synthesis(hyb: &[C], out: &mut [C]) {
    let bands = out.len();

    out[0] = hyb[..6].iter().copied().sum();
    out[1] = hyb[6] + hyb[7];
    out[2] = hyb[8] + hyb[9];

    out[3..bands].copy_from_slice(&hyb[10..bands + 7]);
}
