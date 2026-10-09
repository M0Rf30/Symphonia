// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Encoder-side signal analysis and rate decisions. Ported from libopus `celt/celt_encoder.c`
//! (`transient_analysis`, `patch_transient_decision`, `tf_analysis`, `tf_encode`,
//! `alloc_trim_analysis`, `stereo_analysis`, `dynalloc_analysis`, `compute_vbr`), float build,
//! BSD-3-Clause, see NOTICE. The tone detector, surround masking, LFE and the "analysis"
//! (tonality) side-information of libopus are intentionally not ported.

use super::entenc::RangeEncoder;
use crate::celt::bands::haar1;
use crate::celt::celt::TF_SELECT_TABLE;
use crate::celt::modes::{CeltMode, EPSILON, celt_exp2, celt_log2};
use crate::celt::quant_bands::E_MEANS;
use crate::range::BITRES;

/// Result of [`transient_analysis`].
pub(crate) struct Transient {
    pub is_transient: bool,
    /// C: `tf_estimate`, a 0..~1 measure of how transient the frame is (drives VBR/trim).
    pub tf_estimate: f32,
    /// C: `tf_chan`, the channel with the strongest transient (used by the TF analysis).
    pub tf_chan: usize,
}

/// C: `transient_analysis` (float build, no tone detector, no weak-transient handling).
/// `input` holds `channels` consecutive blocks of `len` pre-emphasised samples.
pub(crate) fn transient_analysis(input: &[f32], len: usize, channels: usize) -> Transient {
    // Table of 6*64/x, trained on real data to minimise the average error.
    const INV_TABLE: [u8; 128] = [
        255, 255, 156, 110, 86, 70, 59, 51, 45, 40, 37, 33, 31, 28, 26, 25, 23, 22, 21, 20, 19, 18,
        17, 16, 16, 15, 15, 14, 13, 13, 12, 12, 12, 12, 11, 11, 11, 10, 10, 10, 9, 9, 9, 9, 9, 9,
        8, 8, 8, 8, 8, 7, 7, 7, 7, 7, 7, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 6, 5, 5, 5,
        5, 5, 5, 5, 5, 5, 5, 5, 5, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4, 4,
        4, 4, 4, 4, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 3, 2,
    ];
    // Forward masking: 6.7 dB/ms.
    const FORWARD_DECAY: f32 = 0.0625;

    let mut tmp = vec![0.0f32; len];
    let len2 = len / 2;
    let mut mask_metric = 0i32;
    let mut tf_chan = 0usize;

    for c in 0..channels {
        let mut mem0 = 0.0f32;
        let mut mem1 = 0.0f32;
        // High-pass filter: (1 - 2*z^-1 + z^-2) / (1 - z^-1 + .5*z^-2).
        for i in 0..len {
            let x = input[i + c * len];
            let y = mem0 + x;
            let mem00 = mem0;
            mem0 = mem0 - x + 0.5 * mem1;
            mem1 = x - mem00;
            tmp[i] = y;
        }
        // The first few samples are bad because we don't propagate the memory.
        tmp[..12].fill(0.0);

        let mut mean = 0.0f32;
        mem0 = 0.0;
        // Forward pass to compute the post-echo threshold (grouping by two).
        for i in 0..len2 {
            let x2 = tmp[2 * i] * tmp[2 * i] + tmp[2 * i + 1] * tmp[2 * i + 1];
            mean += x2;
            mem0 = x2 + (1.0 - FORWARD_DECAY) * mem0;
            tmp[i] = FORWARD_DECAY * mem0;
        }

        mem0 = 0.0;
        let mut max_e = 0.0f32;
        // Backward pass to compute the pre-echo threshold (13.9 dB/ms).
        for i in (0..len2).rev() {
            mem0 = tmp[i] + 0.875 * mem0;
            tmp[i] = 0.125 * mem0;
            max_e = max_e.max(0.125 * mem0);
        }

        // The ratio of the frame energy over the harmonic mean of the energy: effectively a
        // bitrate-normalised temporal noise-to-mask ratio. Frame energy is the geometric mean
        // of the energy and half the max.
        let mean = (mean * max_e * 0.5 * len2 as f32).sqrt();
        let norm = len2 as f32 / (EPSILON + mean);

        // Harmonic mean discarding the unreliable boundaries; the data is smooth, so only 1/4th
        // of the samples are used.
        let mut unmask = 0i32;
        let mut i = 12;
        while i + 5 < len2 {
            let id = (64.0 * norm * (tmp[i] + EPSILON)).floor().clamp(0.0, 127.0) as usize;
            unmask += INV_TABLE[id] as i32;
            i += 4;
        }
        // Normalise, compensating for the 1/4th of the samples and the factor 6 in the table.
        let unmask = 64 * unmask * 4 / (6 * (len2 as i32 - 17));
        if unmask > mask_metric {
            tf_chan = c;
            mask_metric = unmask;
        }
    }
    let is_transient = mask_metric > 200;
    // Arbitrary metric for VBR boost.
    let tf_max = ((27 * mask_metric) as f32).sqrt() - 42.0;
    let tf_max = tf_max.max(0.0);
    let tf_estimate = (0.0069 * tf_max.min(163.0) - 0.139).max(0.0).sqrt();
    Transient { is_transient, tf_estimate, tf_chan }
}

/// C: `patch_transient_decision`. Looks for sudden energy increases that the time-domain
/// detector missed.
pub(crate) fn patch_transient_decision(
    new_e: &[f32],
    old_e: &[f32],
    nb_ebands: usize,
    start: usize,
    end: usize,
    channels: usize,
) -> bool {
    let mut spread_old = [0.0f32; 26];
    // Apply an aggressive (-6 dB/Bark) spreading function to the old frame to avoid false
    // detection caused by irrelevant bands.
    if channels == 1 {
        spread_old[start] = old_e[start];
        for i in start + 1..end {
            spread_old[i] = (spread_old[i - 1] - 1.0).max(old_e[i]);
        }
    }
    else {
        spread_old[start] = old_e[start].max(old_e[start + nb_ebands]);
        for i in start + 1..end {
            spread_old[i] = (spread_old[i - 1] - 1.0).max(old_e[i].max(old_e[i + nb_ebands]));
        }
    }
    for i in (start..end - 1).rev() {
        spread_old[i] = spread_old[i].max(spread_old[i + 1] - 1.0);
    }
    // Compute the mean increase.
    let mut mean_diff = 0.0f32;
    for c in 0..channels {
        for i in start.max(2)..end - 1 {
            let x1 = new_e[i + c * nb_ebands].max(0.0);
            let x2 = spread_old[i].max(0.0);
            mean_diff += (x1 - x2).max(0.0);
        }
    }
    mean_diff /= (channels * (end - 1 - start.max(2))) as f32;
    mean_diff > 1.0
}

/// C: `l1_metric` (float build).
fn l1_metric(tmp: &[f32], n: usize, lm: i32, bias: f32) -> f32 {
    let l1: f32 = tmp[..n].iter().map(|v| v.abs()).sum();
    // When in doubt, prefer good frequency resolution.
    l1 + lm as f32 * bias * l1
}

/// C: `tf_analysis`. Chooses the per-band time/frequency resolution change (`tf_res`, one entry
/// per band, `0`/`1` before [`tf_encode`] maps them) and returns `tf_select`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn tf_analysis(
    m: &CeltMode,
    len: usize,
    is_transient: bool,
    tf_res: &mut [i32],
    lambda: i32,
    x: &[f32],
    n0: usize,
    lm: i32,
    tf_estimate: f32,
    tf_chan: usize,
    importance: &[i32],
) -> i32 {
    let bias = 0.04 * (-0.25f32).max(0.5 - tf_estimate);
    let mut metric = vec![0i32; len];
    let max_w = ((m.e_bands[len] - m.e_bands[len - 1]) as usize) << lm;
    let mut tmp = vec![0.0f32; max_w];
    let mut tmp_1 = vec![0.0f32; max_w];
    let mut path0 = vec![0i32; len];
    let mut path1 = vec![0i32; len];
    let it = is_transient as usize;

    for i in 0..len {
        let band_w = (m.e_bands[i + 1] - m.e_bands[i]) as usize;
        let n = band_w << lm;
        // Band is too narrow to be split down to LM=-1.
        let narrow = band_w == 1;
        let off = tf_chan * n0 + ((m.e_bands[i] as usize) << lm);
        tmp[..n].copy_from_slice(&x[off..off + n]);
        let mut l1 = l1_metric(&tmp, n, if is_transient { lm } else { 0 }, bias);
        let mut best_l1 = l1;
        let mut best_level = 0i32;
        // Check the -1 case for transients.
        if is_transient && !narrow {
            tmp_1[..n].copy_from_slice(&tmp[..n]);
            haar1(&mut tmp_1[..n], (n >> lm) as i32, 1 << lm);
            l1 = l1_metric(&tmp_1, n, lm + 1, bias);
            if l1 < best_l1 {
                best_l1 = l1;
                best_level = -1;
            }
        }
        let kmax = lm + !(is_transient || narrow) as i32;
        for k in 0..kmax {
            let b = if is_transient { lm - k - 1 } else { k + 1 };
            haar1(&mut tmp[..n], (n >> k) as i32, 1 << k);
            l1 = l1_metric(&tmp, n, b, bias);
            if l1 < best_l1 {
                best_l1 = l1;
                best_level = k + 1;
            }
        }
        // metric is in Q1 to be able to select the mid-point (-0.5) for narrower bands.
        metric[i] = if is_transient { 2 * best_level } else { -2 * best_level };
        // For bands that can't be split to -1, set the metric to the half-way point to avoid
        // biasing the decision.
        if narrow && (metric[i] == 0 || metric[i] == -2 * lm) {
            metric[i] -= 1;
        }
    }

    let tbl =
        |sel: usize, flag: usize| 2 * TF_SELECT_TABLE[lm as usize][4 * it + 2 * sel + flag] as i32;

    // Search for the optimal tf resolution, including tf_select.
    let mut selcost = [0i32; 2];
    for (sel, sc) in selcost.iter_mut().enumerate() {
        let mut cost0 = importance[0] * (metric[0] - tbl(sel, 0)).abs();
        let mut cost1 =
            importance[0] * (metric[0] - tbl(sel, 1)).abs() + if is_transient { 0 } else { lambda };
        for i in 1..len {
            let curr0 = cost0.min(cost1 + lambda);
            let curr1 = (cost0 + lambda).min(cost1);
            cost0 = curr0 + importance[i] * (metric[i] - tbl(sel, 0)).abs();
            cost1 = curr1 + importance[i] * (metric[i] - tbl(sel, 1)).abs();
        }
        *sc = cost0.min(cost1);
    }
    // For now, only allow tf_select=1 for transients.
    let tf_select = if selcost[1] < selcost[0] && is_transient { 1usize } else { 0 };

    let mut cost0 = importance[0] * (metric[0] - tbl(tf_select, 0)).abs();
    let mut cost1 = importance[0] * (metric[0] - tbl(tf_select, 1)).abs()
        + if is_transient { 0 } else { lambda };
    // Viterbi forward pass.
    for i in 1..len {
        let from0 = cost0;
        let from1 = cost1 + lambda;
        let curr0;
        if from0 < from1 {
            curr0 = from0;
            path0[i] = 0;
        }
        else {
            curr0 = from1;
            path0[i] = 1;
        }
        let from0 = cost0 + lambda;
        let from1 = cost1;
        let curr1;
        if from0 < from1 {
            curr1 = from0;
            path1[i] = 0;
        }
        else {
            curr1 = from1;
            path1[i] = 1;
        }
        cost0 = curr0 + importance[i] * (metric[i] - tbl(tf_select, 0)).abs();
        cost1 = curr1 + importance[i] * (metric[i] - tbl(tf_select, 1)).abs();
    }
    tf_res[len - 1] = if cost0 < cost1 { 0 } else { 1 };
    // Viterbi backward pass to check the decisions.
    for i in (0..len - 1).rev() {
        tf_res[i] = if tf_res[i + 1] == 1 { path1[i + 1] } else { path0[i + 1] };
    }
    tf_select as i32
}

/// C: `tf_encode`. Writes the `tf_res` flags and `tf_select` and rewrites `tf_res[start..end]`
/// into the final per-band `tf_change` values used by the band quantiser.
pub(crate) fn tf_encode(
    start: usize,
    end: usize,
    is_transient: bool,
    tf_res: &mut [i32],
    lm: i32,
    tf_select: i32,
    enc: &mut RangeEncoder,
) {
    let it = is_transient as usize;
    let mut budget = enc.storage() * 8;
    let mut tell = enc.tell() as u32;
    let mut logp = if is_transient { 2 } else { 4 };
    // Reserve space to code the tf_select decision.
    let tf_select_rsv = lm > 0 && tell + logp < budget;
    budget -= tf_select_rsv as u32;
    let mut curr = 0i32;
    let mut tf_changed = 0i32;
    for i in start..end {
        if tell + logp <= budget {
            enc.enc_bit_logp((tf_res[i] ^ curr) != 0, logp);
            tell = enc.tell() as u32;
            curr = tf_res[i];
            tf_changed |= curr;
        }
        else {
            tf_res[i] = curr;
        }
        logp = if is_transient { 4 } else { 5 };
    }
    // Only code tf_select if it would actually make a difference.
    let row = &TF_SELECT_TABLE[lm as usize];
    let mut tf_select = tf_select as usize;
    if tf_select_rsv && row[4 * it + tf_changed as usize] != row[4 * it + 2 + tf_changed as usize] {
        enc.enc_bit_logp(tf_select != 0, 1);
    }
    else {
        tf_select = 0;
    }
    for r in tf_res[start..end].iter_mut() {
        *r = row[4 * it + 2 * tf_select + *r as usize] as i32;
    }
}

/// C: `alloc_trim_analysis` (float build, no surround/tonality inputs). Returns the allocation
/// trim (0..=10) and updates `stereo_saving`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn alloc_trim_analysis(
    m: &CeltMode,
    x: &[f32],
    band_log_e: &[f32],
    end: usize,
    lm: i32,
    channels: usize,
    n0: usize,
    stereo_saving: &mut f32,
    tf_estimate: f32,
    intensity: i32,
    equiv_rate: i32,
) -> i32 {
    let mut trim = 5.0f32;
    // At low bitrate, reducing the trim seems to help.
    if equiv_rate < 64000 {
        trim = 4.0;
    }
    else if equiv_rate < 80000 {
        let frac = (equiv_rate - 64000) >> 10;
        trim = 4.0 + (1.0 / 16.0) * frac as f32;
    }
    if channels == 2 {
        let partial = |i: usize| -> f32 {
            let lo = (m.e_bands[i] as usize) << lm;
            let hi = (m.e_bands[i + 1] as usize) << lm;
            x[lo..hi].iter().zip(&x[n0 + lo..n0 + hi]).map(|(a, b)| a * b).sum()
        };
        // Inter-channel correlation for low frequencies.
        let mut sum = 0.0f32;
        for i in 0..8 {
            sum += partial(i);
        }
        sum = (1.0f32 / 8.0 * sum).abs().min(1.0);
        let mut min_xc = sum;
        for i in 8..intensity.max(8) as usize {
            min_xc = min_xc.min(partial(i).abs());
        }
        min_xc = min_xc.abs().min(1.0);
        // Mid-side savings estimation based on the LF average.
        let log_xc = celt_log2(1.001 - sum * sum);
        // Mid-side savings estimation based on the min correlation.
        let log_xc2 = (0.5 * log_xc).max(celt_log2(1.001 - min_xc * min_xc));
        trim += (-4.0f32).max(0.75 * log_xc);
        *stereo_saving = (*stereo_saving + 0.25).min(-0.5 * log_xc2);
    }

    // Estimate the spectral tilt.
    let mut diff = 0.0f32;
    for c in 0..channels {
        for i in 0..end - 1 {
            diff +=
                band_log_e[i + c * m.nb_ebands as usize] * (2 + 2 * i as i32 - end as i32) as f32;
        }
    }
    diff /= (channels * (end - 1)) as f32;
    trim -= (-2.0f32).max(2.0f32.min((diff + 1.0) / 6.0));
    trim -= 2.0 * tf_estimate;
    ((0.5 + trim).floor() as i32).clamp(0, 10)
}

/// C: `stereo_analysis`. Decides between dual (L/R) and joint (M/S) stereo from the L1 norms.
pub(crate) fn stereo_analysis(m: &CeltMode, x: &[f32], lm: i32, n0: usize) -> bool {
    let mut sum_lr = EPSILON;
    let mut sum_ms = EPSILON;
    // Use the L1 norm to model the entropy of the L/R signal vs the M/S signal.
    for i in 0..13 {
        for j in ((m.e_bands[i] as usize) << lm)..((m.e_bands[i + 1] as usize) << lm) {
            let l = x[j];
            let r = x[n0 + j];
            let mid = l + r;
            let side = l - r;
            sum_lr += l.abs() + r.abs();
            sum_ms += mid.abs() + side.abs();
        }
    }
    // C: `QCONST16(0.707107f, 15)`; kept literally (a 1/sqrt(2) approximation).
    #[allow(clippy::approx_constant)]
    const INV_SQRT2: f32 = 0.707107;
    sum_ms *= INV_SQRT2;
    let mut thetas = 13;
    // We don't need thetas for lower bands with LM<=1.
    if lm <= 1 {
        thetas -= 8;
    }
    let e13 = (m.e_bands[13] as i32) << (lm + 1);
    ((e13 + thetas) as f32) * sum_ms > (e13 as f32) * sum_lr
}

/// C: `median_of_5`.
fn median_of_5(x: &[f32]) -> f32 {
    let t2 = x[2];
    let (mut t0, mut t1) = if x[0] > x[1] { (x[1], x[0]) } else { (x[0], x[1]) };
    let (mut t3, mut t4) = if x[3] > x[4] { (x[4], x[3]) } else { (x[3], x[4]) };
    if t0 > t3 {
        std::mem::swap(&mut t0, &mut t3);
        std::mem::swap(&mut t1, &mut t4);
    }
    if t2 > t1 {
        if t1 < t3 { t2.min(t3) } else { t4.min(t1) }
    }
    else if t2 < t3 {
        t1.min(t3)
    }
    else {
        t2.min(t4)
    }
}

/// C: `median_of_3`.
fn median_of_3(x: &[f32]) -> f32 {
    let (t0, t1) = if x[0] > x[1] { (x[1], x[0]) } else { (x[0], x[1]) };
    let t2 = x[2];
    if t1 < t2 {
        t1
    }
    else if t0 < t2 {
        t2
    }
    else {
        t0
    }
}

/// Outputs of [`dynalloc_analysis`].
pub(crate) struct DynAlloc {
    /// C: `maxDepth`.
    pub max_depth: f32,
    /// C: `tot_boost`, the total boost (1/8 bit units) requested by `offsets`.
    pub tot_boost: i32,
}

/// C: `dynalloc_analysis` (float build, no tone/surround/analysis side-info, `lsb_depth = 24`).
/// Fills `offsets` (boost per band), `importance` (TF weights) and `spread_weight`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn dynalloc_analysis(
    m: &CeltMode,
    band_log_e: &[f32],
    band_log_e2: &[f32],
    start: usize,
    end: usize,
    channels: usize,
    offsets: &mut [i32],
    is_transient: bool,
    vbr: bool,
    constrained_vbr: bool,
    lm: i32,
    effective_bytes: i32,
    importance: &mut [i32],
    spread_weight: &mut [i32],
) -> DynAlloc {
    const LSB_DEPTH: i32 = 24;
    let nb = m.nb_ebands as usize;
    let e_bands = m.e_bands;
    let mut follower = vec![0.0f32; channels * nb];
    let mut noise_floor = vec![0.0f32; nb];
    let mut band_log_e3 = vec![0.0f32; nb];
    let mut tot_boost = 0i32;
    offsets[..nb].fill(0);

    // Dynamic allocation code. The noise floor must take into account eMeans, the depth, the
    // width of the bands and the preemphasis filter (approx. square of the Bark band ID).
    let mut max_depth = -31.9f32;
    for i in 0..end {
        noise_floor[i] = 0.0625 * m.log_n[i] as f32 + 0.5 + (9 - LSB_DEPTH) as f32 - E_MEANS[i]
            + 0.0062 * ((i + 5) * (i + 5)) as f32;
    }
    for c in 0..channels {
        for i in 0..end {
            max_depth = max_depth.max(band_log_e[c * nb + i] - noise_floor[i]);
        }
    }
    {
        // A really simple masking model, to avoid taking completely masked bands into account
        // when computing the spreading decision.
        let mut mask = vec![0.0f32; nb];
        let mut sig = vec![0.0f32; nb];
        for i in 0..end {
            mask[i] = band_log_e[i] - noise_floor[i];
        }
        if channels == 2 {
            for i in 0..end {
                mask[i] = mask[i].max(band_log_e[nb + i] - noise_floor[i]);
            }
        }
        sig[..end].copy_from_slice(&mask[..end]);
        for i in 1..end {
            mask[i] = mask[i].max(mask[i - 1] - 2.0);
        }
        for i in (0..end.saturating_sub(1)).rev() {
            mask[i] = mask[i].max(mask[i + 1] - 3.0);
        }
        for i in 0..end {
            // SMR: the mask is never more than 72 dB below the peak and never below the noise
            // floor.
            let smr = sig[i] - (0.0f32.max(max_depth - 12.0)).max(mask[i]);
            // Clamp SMR to make sure we're not shifting by something negative or too large.
            let shift = 5.min(0.max(-((0.5 + smr).floor() as i32)));
            spread_weight[i] = 32 >> shift;
        }
    }

    // Make sure that dynamic allocation can't make us bust the budget. It is enabled starting
    // at 24 kb/s for 20 ms frames and 96 kb/s for 2.5 ms frames.
    if effective_bytes >= 30 + 5 * lm {
        let mut last = 0usize;
        for c in 0..channels {
            band_log_e3[..end].copy_from_slice(&band_log_e2[c * nb..c * nb + end]);
            if lm == 0 {
                // For 2.5 ms frames the first 8 bands have just one bin, so the energy is
                // highly unreliable. Not reachable with 20 ms frames; kept for completeness.
                for i in 0..8.min(end) {
                    band_log_e3[i] = band_log_e2[c * nb + i];
                }
            }
            let f = &mut follower[c * nb..(c + 1) * nb];
            f[0] = band_log_e3[0];
            for i in 1..end {
                // The last band to be at least 3 dB higher than the previous one is the last
                // we'll consider. Otherwise we run into problems on bandlimited signals.
                if band_log_e3[i] > band_log_e3[i - 1] + 0.5 {
                    last = i;
                }
                f[i] = (f[i - 1] + 1.5).min(band_log_e3[i]);
            }
            for i in (0..last).rev() {
                f[i] = f[i].min((f[i + 1] + 2.0).min(band_log_e3[i]));
            }
            // Combine with a median filter to avoid dynalloc triggering unnecessarily. A higher
            // offset reduces the impact of the median filter and makes dynalloc use more bits.
            let offset = 1.0f32;
            for i in 2..end.saturating_sub(2) {
                f[i] = f[i].max(median_of_5(&band_log_e3[i - 2..]) - offset);
            }
            let tmp = median_of_3(&band_log_e3[0..]) - offset;
            f[0] = f[0].max(tmp);
            f[1] = f[1].max(tmp);
            let tmp = median_of_3(&band_log_e3[end - 3..]) - offset;
            f[end - 2] = f[end - 2].max(tmp);
            f[end - 1] = f[end - 1].max(tmp);
            for i in 0..end {
                f[i] = f[i].max(noise_floor[i]);
            }
        }
        if channels == 2 {
            for i in start..end {
                // Consider 24 dB "cross-talk".
                follower[nb + i] = follower[nb + i].max(follower[i] - 4.0);
                follower[i] = follower[i].max(follower[nb + i] - 4.0);
                follower[i] = 0.5
                    * ((band_log_e[i] - follower[i]).max(0.0)
                        + (band_log_e[nb + i] - follower[nb + i]).max(0.0));
            }
        }
        else {
            for i in start..end {
                follower[i] = (band_log_e[i] - follower[i]).max(0.0);
            }
        }
        for i in start..end {
            importance[i] = (0.5 + 13.0 * celt_exp2(follower[i].min(4.0))).floor() as i32;
        }
        // For non-transient CBR/CVBR frames, halve the dynalloc contribution.
        if (!vbr || constrained_vbr) && !is_transient {
            for f in follower[start..end].iter_mut() {
                *f *= 0.5;
            }
        }
        for i in start..end {
            if i < 8 {
                follower[i] *= 2.0;
            }
            if i >= 12 {
                follower[i] *= 0.5;
            }
        }
        for i in start..end {
            follower[i] = follower[i].min(4.0);
            let width = (channels as i32 * (e_bands[i + 1] - e_bands[i]) as i32) << lm;
            let (boost, boost_bits);
            if width < 6 {
                boost = follower[i] as i32;
                boost_bits = (boost * width) << BITRES;
            }
            else if width > 48 {
                boost = (follower[i] * 8.0) as i32;
                boost_bits = ((boost * width) << BITRES) / 8;
            }
            else {
                boost = (follower[i] * width as f32 / 6.0) as i32;
                boost_bits = (boost * 6) << BITRES;
            }
            // For CBR and non-transient CVBR frames, limit dynalloc to 2/3 of the bits.
            if (!vbr || (constrained_vbr && !is_transient))
                && (tot_boost + boost_bits) >> BITRES >> 3 > 2 * effective_bytes / 3
            {
                let cap = (2 * effective_bytes / 3) << BITRES << 3;
                offsets[i] = cap - tot_boost;
                tot_boost = cap;
                break;
            }
            else {
                offsets[i] = boost;
                tot_boost += boost_bits;
            }
        }
    }
    else {
        for imp in importance[start..end].iter_mut() {
            *imp = 13;
        }
    }
    DynAlloc { max_depth, tot_boost }
}

/// Inputs of [`compute_vbr`] that do not depend on the packet being built.
pub(crate) struct VbrInputs {
    pub base_target: i32,
    pub lm: i32,
    /// Equivalent bitrate (C: `equiv_rate`).
    pub bitrate: i32,
    pub last_coded_bands: i32,
    pub channels: i32,
    pub intensity: i32,
    pub constrained_vbr: bool,
    pub stereo_saving: f32,
    pub tot_boost: i32,
    pub tf_estimate: f32,
    pub max_depth: f32,
    pub temporal_vbr: f32,
}

/// C: `compute_vbr` (float build, no tonality/surround side-info). Returns the target size of
/// the frame in 1/8 bit units.
pub(crate) fn compute_vbr(m: &CeltMode, v: &VbrInputs) -> i32 {
    let nb = m.nb_ebands;
    let e_bands = m.e_bands;
    let lm = v.lm;
    let coded_bands = if v.last_coded_bands != 0 { v.last_coded_bands } else { nb };
    let mut coded_bins = (e_bands[coded_bands as usize] as i32) << lm;
    if v.channels == 2 {
        coded_bins += (e_bands[v.intensity.min(coded_bands) as usize] as i32) << lm;
    }

    let mut target = v.base_target;

    // Stereo savings.
    if v.channels == 2 {
        let coded_stereo_bands = v.intensity.min(coded_bands);
        let coded_stereo_dof =
            ((e_bands[coded_stereo_bands as usize] as i32) << lm) - coded_stereo_bands;
        // Maximum fraction of the bits we can save if the signal is mono.
        let max_frac = 0.8 * coded_stereo_dof as f32 / coded_bins as f32;
        let stereo_saving = v.stereo_saving.min(1.0);
        target -= (max_frac * target as f32)
            .min((stereo_saving - 0.1) * (coded_stereo_dof << BITRES) as f32)
            as i32;
    }
    // Boost the rate according to dynalloc (minus the dynalloc average for calibration).
    target += v.tot_boost - (19 << lm);
    // Apply the transient boost, compensating for the average boost.
    const TF_CALIBRATION: f32 = 0.044;
    target += ((v.tf_estimate - TF_CALIBRATION) * target as f32) as i32;

    let bins = (e_bands[nb as usize - 2] as i32) << lm;
    let mut floor_depth = ((v.channels * bins) << BITRES) as f32 * v.max_depth;
    floor_depth = floor_depth.max((target >> 2) as f32);
    target = target.min(floor_depth as i32);

    // Make VBR less aggressive for constrained VBR because we can't keep a higher bitrate for
    // long.
    if v.constrained_vbr {
        target = v.base_target + (0.67 * (target - v.base_target) as f32) as i32;
    }

    if v.tf_estimate < 0.2 {
        let amount = 0.0000031 * 0.max(32000.min(96000 - v.bitrate)) as f32;
        let tvbr_factor = v.temporal_vbr * amount;
        target += (tvbr_factor * target as f32) as i32;
    }

    // Don't allow more than doubling the rate.
    target.min(2 * v.base_target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lcg(seed: &mut u32) -> f32 {
        *seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        ((*seed >> 8) as f32 / (1u32 << 24) as f32) - 0.5
    }

    #[test]
    fn transient_detector_fires_on_click_not_on_noise() {
        let len = 1080;
        let mut seed = 1;
        // Steady noise: not transient.
        let noise: Vec<f32> = (0..len).map(|_| lcg(&mut seed) * 2000.0).collect();
        let t = transient_analysis(&noise, len, 1);
        assert!(!t.is_transient, "noise flagged transient");
        // Near silence with a sharp attack late in the frame: transient.
        let mut click = vec![0.0f32; len];
        for (i, v) in click.iter_mut().enumerate() {
            *v = lcg(&mut seed) * 2.0;
            if i > 700 && i < 760 {
                *v += lcg(&mut seed) * 20000.0;
            }
        }
        let t = transient_analysis(&click, len, 1);
        assert!(t.is_transient, "click not flagged transient");
        assert!(t.tf_estimate > 0.0);
    }

    #[test]
    fn medians() {
        assert_eq!(median_of_3(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median_of_3(&[1.0, 2.0, 3.0]), 2.0);
        assert_eq!(median_of_5(&[5.0, 1.0, 4.0, 2.0, 3.0]), 3.0);
        assert_eq!(median_of_5(&[1.0, 2.0, 3.0, 4.0, 5.0]), 3.0);
        assert_eq!(median_of_5(&[9.0, 9.0, 1.0, 1.0, 5.0]), 5.0);
    }

    #[test]
    fn tf_encode_round_trips_flags() {
        use crate::range::RangeDecoder;
        let lm = 3;
        let mut tf_res = vec![0i32; 21];
        tf_res[3] = 1;
        tf_res[4] = 1;
        let mut enc = RangeEncoder::new(32);
        tf_encode(0, 21, false, &mut tf_res, lm, 0, &mut enc);
        let (buf, _) = enc.done();
        let mut dec = RangeDecoder::new(&buf);
        let mut curr = 0;
        let mut got = vec![];
        for i in 0..21 {
            let logp = if i == 0 { 4 } else { 5 };
            curr ^= dec.dec_bit_logp(logp) as i32;
            got.push(curr);
        }
        assert_eq!(&got[..6], &[0, 0, 0, 1, 1, 0]);
    }
}
