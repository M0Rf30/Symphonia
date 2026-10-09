// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Band-energy quantisation (encoder side). Ported from libopus `celt/quant_bands.c`
//! (`quant_coarse_energy`, `quant_fine_energy`, `quant_energy_finalise`) and
//! `celt/laplace.c` (`ec_laplace_encode`), float build. BSD-3-Clause, see NOTICE.

use super::entenc::RangeEncoder;
use crate::celt::laplace::{LAPLACE_LOG_MINP, LAPLACE_MINP, ec_laplace_get_freq1};
use crate::celt::modes::CeltMode;
use crate::celt::quant_bands::{
    BETA_COEF, BETA_INTRA, E_PROB_MODEL, MAX_FINE_BITS, PRED_COEF, SMALL_ENERGY_ICDF,
};

/// C: `ec_laplace_encode`. Writes `value` (clamped to the largest representable magnitude, which
/// is returned) with the Laplace-like model given by the probability of zero `fs` and `decay`.
pub(crate) fn ec_laplace_encode(enc: &mut RangeEncoder, value: i32, fs: u32, decay: u32) -> i32 {
    let mut fl = 0u32;
    let mut fs = fs;
    let mut out = value;
    if value != 0 {
        let s: i32 = if value < 0 { -1 } else { 0 };
        let mut val = (value + s) ^ s;
        fl = fs;
        fs = ec_laplace_get_freq1(fs, decay as i32);
        let mut i = 1;
        // Search the decaying part of the pdf.
        while fs > 0 && i < val {
            fs *= 2;
            fl += fs + 2 * LAPLACE_MINP;
            fs = (((fs as i64) * decay as i64) >> 15) as u32;
            i += 1;
        }
        // Everything beyond that has probability LAPLACE_MINP.
        if fs == 0 {
            let ndi_max = (32768 - fl + LAPLACE_MINP - 1) >> LAPLACE_LOG_MINP;
            let ndi_max = ((ndi_max as i32) - s) >> 1;
            let di = (val - i).min(ndi_max - 1);
            fl += (2 * di + 1 + s) as u32 * LAPLACE_MINP;
            fs = LAPLACE_MINP.min(32768 - fl);
            val = (i + di + s) ^ s;
            out = val;
        }
        else {
            fs += LAPLACE_MINP;
            fl += if s == 0 { fs } else { 0 };
        }
        debug_assert!(fl + fs <= 32768);
        debug_assert!(fs > 0);
    }
    enc.encode_bin(fl, fl + fs, 15);
    out
}

/// C: `loss_distortion`.
fn loss_distortion(
    e_bands: &[f32],
    old_e_bands: &[f32],
    start: i32,
    end: i32,
    len: i32,
    channels: i32,
) -> f32 {
    let mut dist = 0.0f32;
    for c in 0..channels {
        for i in start..end {
            let idx = (i + c * len) as usize;
            let d = e_bands[idx] - old_e_bands[idx];
            dist += d * d;
        }
    }
    dist.min(200.0)
}

/// C: `quant_coarse_energy_impl`. Returns the "badness" (number of clamped residuals).
#[allow(clippy::too_many_arguments)]
fn quant_coarse_energy_impl(
    m: &CeltMode,
    start: i32,
    end: i32,
    e_bands: &[f32],
    old_e_bands: &mut [f32],
    budget: i32,
    mut tell: i32,
    prob_model: &[u8; 42],
    error: &mut [f32],
    enc: &mut RangeEncoder,
    channels: i32,
    lm: usize,
    intra: bool,
    max_decay: f32,
) -> i32 {
    let nb = m.nb_ebands;
    let mut badness = 0;
    let mut prev = [0.0f32; 2];

    if tell + 3 <= budget {
        enc.enc_bit_logp(intra, 3);
    }
    let (coef, beta) = if intra { (0.0f32, BETA_INTRA) } else { (PRED_COEF[lm], BETA_COEF[lm]) };

    for i in start..end {
        for c in 0..channels {
            let idx = (i + c * nb) as usize;
            let x = e_bands[idx];
            let old_e = old_e_bands[idx].max(-9.0);
            let f = x - coef * old_e - prev[c as usize];
            // Rounding to nearest integer here is really important!
            let mut qi = (0.5 + f).floor() as i32;
            let decay_bound = old_e_bands[idx].max(-28.0) - max_decay;
            // Prevent the energy from going down too quickly (e.g. for bands that have just one
            // bin).
            if qi < 0 && x < decay_bound {
                qi += (decay_bound - x) as i32;
                if qi > 0 {
                    qi = 0;
                }
            }
            let qi0 = qi;
            // If we don't have enough bits to encode all the energy, just assume something safe.
            tell = enc.tell();
            let bits_left = budget - tell - 3 * channels * (end - i);
            if i != start && bits_left < 30 {
                if bits_left < 24 {
                    qi = qi.min(1);
                }
                if bits_left < 16 {
                    qi = qi.max(-1);
                }
            }
            if budget - tell >= 15 {
                let pi = 2 * (i.min(20)) as usize;
                qi = ec_laplace_encode(
                    enc,
                    qi,
                    (prob_model[pi] as u32) << 7,
                    (prob_model[pi + 1] as u32) << 6,
                );
            }
            else if budget - tell >= 2 {
                qi = qi.clamp(-1, 1);
                enc.enc_icdf((2 * qi) ^ -((qi < 0) as i32), &SMALL_ENERGY_ICDF, 2);
            }
            else if budget - tell >= 1 {
                qi = qi.min(0);
                enc.enc_bit_logp(qi != 0, 1);
            }
            else {
                qi = -1;
            }
            error[idx] = f - qi as f32;
            badness += (qi0 - qi).abs();
            let q = qi as f32;

            old_e_bands[idx] = coef * old_e + prev[c as usize] + q;
            prev[c as usize] = prev[c as usize] + q - beta * q;
        }
    }
    badness
}

/// C: `quant_coarse_energy`. `delayed_intra` is the encoder's persistent intra-cost tracker.
/// Two-pass mode tries both intra and inter coding and keeps the cheaper one.
#[allow(clippy::too_many_arguments)]
pub(crate) fn quant_coarse_energy(
    m: &CeltMode,
    start: i32,
    end: i32,
    eff_end: i32,
    e_bands: &[f32],
    old_e_bands: &mut [f32],
    budget: u32,
    error: &mut [f32],
    enc: &mut RangeEncoder,
    channels: i32,
    lm: usize,
    nb_available_bytes: i32,
    force_intra: bool,
    delayed_intra: &mut f32,
    two_pass: bool,
) {
    let nb = m.nb_ebands;
    let n = (channels * nb) as usize;
    let mut two_pass = two_pass;
    let mut intra = force_intra
        || (!two_pass
            && *delayed_intra > (2 * channels * (end - start)) as f32
            && nb_available_bytes > (end - start) * channels);
    let new_distortion = loss_distortion(e_bands, old_e_bands, start, eff_end, nb, channels);

    let tell = enc.tell();
    if tell + 3 > budget as i32 {
        two_pass = false;
        intra = false;
    }

    let mut max_decay = 16.0f32;
    if end - start > 10 {
        max_decay = max_decay.min(0.125 * nb_available_bytes as f32);
    }
    let enc_start_state = if two_pass { Some(enc.clone()) } else { None };

    let mut old_intra = old_e_bands[..n].to_vec();
    let mut error_intra = vec![0.0f32; n];
    let mut badness1 = 0;
    if two_pass || intra {
        badness1 = quant_coarse_energy_impl(
            m,
            start,
            end,
            e_bands,
            &mut old_intra,
            budget as i32,
            tell,
            &E_PROB_MODEL[lm][1],
            &mut error_intra,
            enc,
            channels,
            lm,
            true,
            max_decay,
        );
    }

    if !intra {
        let tell_intra = enc.tell_frac();
        let intra_state = enc_start_state.as_ref().map(|_| enc.clone());
        if let Some(start_state) = enc_start_state {
            *enc = start_state;
        }
        let badness2 = quant_coarse_energy_impl(
            m,
            start,
            end,
            e_bands,
            old_e_bands,
            budget as i32,
            tell,
            &E_PROB_MODEL[lm][0],
            error,
            enc,
            channels,
            lm,
            false,
            max_decay,
        );
        // C also biases this tie-break by `intra_bias` (packet-loss resilience), which is zero
        // here because no loss rate is configured.
        if two_pass
            && (badness1 < badness2 || (badness1 == badness2 && enc.tell_frac() > tell_intra))
        {
            *enc = intra_state.expect("two-pass keeps the intra state");
            old_e_bands[..n].copy_from_slice(&old_intra);
            error[..n].copy_from_slice(&error_intra);
            intra = true;
        }
    }
    else {
        old_e_bands[..n].copy_from_slice(&old_intra);
        error[..n].copy_from_slice(&error_intra);
    }

    if intra {
        *delayed_intra = new_distortion;
    }
    else {
        *delayed_intra = PRED_COEF[lm] * PRED_COEF[lm] * *delayed_intra + new_distortion;
    }
}

/// C: `quant_fine_energy`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn quant_fine_energy(
    m: &CeltMode,
    start: i32,
    end: i32,
    old_e_bands: &mut [f32],
    error: &mut [f32],
    extra_quant: &[i32],
    enc: &mut RangeEncoder,
    channels: i32,
) {
    let nb = m.nb_ebands;
    for i in start..end {
        let iu = i as usize;
        if extra_quant[iu] <= 0 {
            continue;
        }
        let extra = 1i32 << extra_quant[iu];
        if enc.tell() + channels * extra_quant[iu] > (enc.storage() * 8) as i32 {
            continue;
        }
        for c in 0..channels {
            let idx = (i + c * nb) as usize;
            let mut q2 = ((error[idx] + 0.5) * extra as f32).floor() as i32;
            if q2 > extra - 1 {
                q2 = extra - 1;
            }
            if q2 < 0 {
                q2 = 0;
            }
            enc.enc_bits(q2 as u32, extra_quant[iu] as u32);
            let offset =
                (q2 as f32 + 0.5) * (1 << (14 - extra_quant[iu])) as f32 * (1.0 / 16384.0) - 0.5;
            old_e_bands[idx] += offset;
            error[idx] -= offset;
        }
    }
}

/// C: `quant_energy_finalise`. Spends any left-over bits on extra fine-energy resolution.
#[allow(clippy::too_many_arguments)]
pub(crate) fn quant_energy_finalise(
    m: &CeltMode,
    start: i32,
    end: i32,
    old_e_bands: &mut [f32],
    error: &mut [f32],
    fine_quant: &[i32],
    fine_priority: &[i32],
    bits_left: i32,
    enc: &mut RangeEncoder,
    channels: i32,
) {
    let nb = m.nb_ebands;
    let mut bits_left = bits_left;
    for prio in 0..2 {
        let mut i = start;
        while i < end && bits_left >= channels {
            let iu = i as usize;
            if fine_quant[iu] >= MAX_FINE_BITS || fine_priority[iu] != prio {
                i += 1;
                continue;
            }
            for c in 0..channels {
                let idx = (i + c * nb) as usize;
                let q2 = if error[idx] < 0.0 { 0 } else { 1 };
                enc.enc_bits(q2 as u32, 1);
                let offset =
                    (q2 as f32 - 0.5) * (1 << (14 - fine_quant[iu] - 1)) as f32 * (1.0 / 16384.0);
                old_e_bands[idx] += offset;
                error[idx] -= offset;
                bits_left -= 1;
            }
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::celt::laplace::ec_laplace_decode;
    use crate::range::RangeDecoder;

    #[test]
    fn laplace_encode_decode_round_trip() {
        let mut enc = RangeEncoder::new(512);
        let values: Vec<i32> = (0..200).map(|i| ((i * 37) % 23) - 11).collect();
        let mut coded = Vec::new();
        for (i, &v) in values.iter().enumerate() {
            let pi = 2 * (i % 21);
            let p = E_PROB_MODEL[3][0][pi] as u32;
            let d = E_PROB_MODEL[3][0][pi + 1] as u32;
            coded.push(ec_laplace_encode(&mut enc, v, p << 7, d << 6));
        }
        let (buf, err) = enc.done();
        assert!(!err);
        let mut dec = RangeDecoder::new(&buf);
        for (i, &v) in coded.iter().enumerate() {
            let pi = 2 * (i % 21);
            let p = E_PROB_MODEL[3][0][pi] as u32;
            let d = E_PROB_MODEL[3][0][pi + 1] as u32;
            assert_eq!(ec_laplace_decode(&mut dec, p << 7, d << 6), v, "symbol {i}");
        }
    }
}
