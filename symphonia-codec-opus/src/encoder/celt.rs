// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The CELT frame encoder. Ported from libopus `celt/celt_encoder.c` (`celt_encode_with_ec`),
//! float build, restricted to what a music-streaming CELT-only encoder needs:
//! 48 kHz, 20 ms frames, mono or stereo, CBR / VBR / constrained VBR, pre-emphasis, transient
//! detection with short blocks, TF analysis, spreading decision, dynamic allocation boosts,
//! allocation trim, intensity / dual / mid-side stereo. BSD-3-Clause, see NOTICE.
//!
//! Not ported (the produced stream is still fully standard CELT): the pitch pre-filter
//! (the post-filter flag is always sent as off), the tone detector, hybrid/low-delay/LFE modes,
//! surround masking, the tonality "analysis" input and QEXT. See `analysis`/`bands` for the
//! remaining simplifications.

use super::analysis::{
    VbrInputs, alloc_trim_analysis, compute_vbr, dynalloc_analysis, patch_transient_decision,
    stereo_analysis, tf_analysis, tf_encode, transient_analysis,
};
use super::bands::{
    QuantParams, compute_band_energies, hysteresis_decision, normalise_bands, quant_all_bands,
    spreading_decision,
};
use super::energy::{quant_coarse_energy, quant_energy_finalise, quant_fine_energy};
use super::entenc::RangeEncoder;
use crate::celt::bands::{SPREAD_NONE, SPREAD_NORMAL};
use crate::celt::celt::init_caps;
use crate::celt::mdct::MDCT_LOOKUP_960;
use crate::celt::modes::{CeltMode, MODE_48000_960};
use crate::celt::quant_bands::amp2_log2;
use crate::celt::rate::{AllocCoder, SkipInfo, clt_compute_allocation};
use crate::range::BITRES;

/// Samples per channel in a 20 ms frame at 48 kHz.
pub(crate) const FRAME_SIZE: usize = 960;
/// `LM` of a 20 ms frame (`FRAME_SIZE == shortMdctSize << LM`).
const LM: i32 = 3;
/// Maximum CELT payload of one frame, in bytes.
pub(crate) const MAX_FRAME_BYTES: usize = 1275;

/// C: `tapset_icdf`-sibling tables from `celt_encoder.c`.
const SPREAD_ICDF: [u8; 4] = [25, 23, 2, 0];
const TRIM_ICDF: [u8; 11] = [126, 124, 119, 109, 87, 41, 19, 9, 4, 2, 0];
/// C: `intensity_thresholds` / `intensity_histeresis` (kb/s), index = intensity band.
const INTENSITY_THRESHOLDS: [f32; 21] = [
    1., 2., 3., 4., 5., 6., 7., 8., 16., 24., 36., 44., 50., 56., 62., 67., 72., 79., 88., 106.,
    134.,
];
const INTENSITY_HYSTERESIS: [f32; 21] =
    [1., 1., 1., 1., 1., 1., 1., 2., 2., 2., 2., 2., 2., 2., 3., 3., 4., 5., 6., 8., 8.];
/// C: `mode->preemph[0]` of the 48 kHz mode.
const PREEMPH_COEF: f32 = 0.85000610;
/// C: `CELT_SIG_SCALE`.
const SIG_SCALE: f32 = 32768.0;

/// Static configuration of a [`CeltEncoder`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct CeltConfig {
    pub channels: usize,
    /// Target bitrate in bits per second (CELT payload only; the TOC byte is accounted for by
    /// the caller).
    pub bitrate: i32,
    pub vbr: bool,
    pub constrained_vbr: bool,
    /// 0..=10, same meaning as `OPUS_SET_COMPLEXITY`.
    pub complexity: i32,
    /// Number of coded bands (13, 17, 19 or 21).
    pub end: i32,
}

/// The entropy-coding half of the allocator for the encoder (see [`AllocCoder`]).
struct EncAlloc<'a> {
    enc: &'a mut RangeEncoder,
    /// C: `prev` (`lastCodedBands`), for the hysteresis of the band-skip decision.
    prev: i32,
    signal_bandwidth: i32,
    intensity: i32,
    dual_stereo: bool,
}

impl AllocCoder for EncAlloc<'_> {
    fn skip_band(&mut self, info: &SkipInfo) -> bool {
        // This is the only part of the allocation that is not a mandatory part of the bitstream:
        // any band skipped here must be explicitly signalled. A threshold with some hysteresis
        // keeps bands from fluctuating in and out, but we try not to fold below a certain point.
        let depth_threshold =
            if info.coded_bands > 17 { if info.band < self.prev { 7 } else { 9 } } else { 0 };
        if info.coded_bands <= info.start + 2
            || (info.band_bits > ((depth_threshold * info.band_width) << info.lm << BITRES) >> 4
                && info.band <= self.signal_bandwidth)
        {
            self.enc.enc_bit_logp(true, 1);
            true
        }
        else {
            self.enc.enc_bit_logp(false, 1);
            false
        }
    }

    fn intensity(&mut self, start: i32, coded_bands: i32) -> i32 {
        self.intensity = self.intensity.min(coded_bands);
        self.enc.enc_uint((self.intensity - start) as u32, (coded_bands + 1 - start) as u32);
        self.intensity
    }

    fn dual_stereo(&mut self) -> bool {
        self.enc.enc_bit_logp(self.dual_stereo, 1);
        self.dual_stereo
    }
}

/// Persistent state of the CELT encoder. C: `OpusCustomEncoder`.
pub(crate) struct CeltEncoder {
    mode: &'static CeltMode,
    cfg: CeltConfig,
    spread_decision: i32,
    delayed_intra: f32,
    tonal_average: i32,
    last_coded_bands: i32,
    consec_transient: i32,
    preemph_mem: [f32; 2],
    /// Last `overlap` pre-emphasised samples of each channel (C: `prefilter_mem`'s tail).
    in_mem: Vec<f32>,
    overlap_max: f32,
    vbr_reservoir: i32,
    vbr_drift: i32,
    vbr_offset: i32,
    vbr_count: i32,
    stereo_saving: f32,
    intensity: i32,
    spec_avg: f32,
    old_band_e: Vec<f32>,
    old_log_e: Vec<f32>,
    old_log_e2: Vec<f32>,
    energy_error: Vec<f32>,
    /// Final range of the last encoded frame (C: `st->rng`).
    final_range: u32,
}

impl CeltEncoder {
    pub fn new(cfg: CeltConfig) -> Self {
        let mode = &MODE_48000_960;
        let nb = mode.nb_ebands as usize;
        let cc = cfg.channels;
        assert!(cc == 1 || cc == 2);
        CeltEncoder {
            mode,
            cfg,
            spread_decision: SPREAD_NORMAL,
            delayed_intra: 1.0,
            tonal_average: 256,
            last_coded_bands: 0,
            consec_transient: 0,
            preemph_mem: [0.0; 2],
            in_mem: vec![0.0; cc * mode.overlap as usize],
            overlap_max: 0.0,
            vbr_reservoir: 0,
            vbr_drift: 0,
            vbr_offset: 0,
            vbr_count: 0,
            stereo_saving: 0.0,
            intensity: 0,
            spec_avg: 0.0,
            old_band_e: vec![0.0; cc * nb],
            old_log_e: vec![-28.0; cc * nb],
            old_log_e2: vec![-28.0; cc * nb],
            energy_error: vec![0.0; cc * nb],
            final_range: 0,
        }
    }

    /// The final range-coder state of the last frame.
    pub fn final_range(&self) -> u32 {
        self.final_range
    }

    pub fn config_mut(&mut self) -> &mut CeltConfig {
        &mut self.cfg
    }

    /// C: `compute_mdcts`. Windows and transforms all sub-frames of all channels. `input` holds
    /// `channels` blocks of `FRAME_SIZE + overlap` samples, `out` `channels` blocks of
    /// `FRAME_SIZE` coefficients with short blocks interleaved.
    fn compute_mdcts(&self, short_blocks: bool, input: &[f32], out: &mut [f32]) {
        let mode = self.mode;
        let overlap = mode.overlap as usize;
        let (b, nn, shift) = if short_blocks {
            (1usize << LM, mode.short_mdct_size as usize, mode.max_lm)
        }
        else {
            (1usize, FRAME_SIZE, mode.max_lm - LM)
        };
        let mut win_buf = [0f32; FRAME_SIZE + 120];
        let mut tmp_buf = [0f32; FRAME_SIZE];
        debug_assert!(overlap <= 120);
        let win = &mut win_buf[..nn + overlap];
        let tmp = &mut tmp_buf[..nn];
        for c in 0..self.cfg.channels {
            for bi in 0..b {
                let off = c * (FRAME_SIZE + overlap) + bi * nn;
                win.copy_from_slice(&input[off..off + nn + overlap]);
                MDCT_LOOKUP_960.forward(win, tmp, mode.window, mode.overlap, shift);
                // Interleave the sub-frames while doing the MDCTs.
                for (k, &v) in tmp.iter().enumerate() {
                    out[bi + c * nn * b + k * b] = v;
                }
            }
        }
    }

    /// C: `celt_encode_with_ec`. Encodes one 20 ms frame of interleaved samples (`pcm.len() ==
    /// channels * 960`, nominal range +-1.0) into a CELT payload. In CBR mode the payload is
    /// exactly `cbr_bytes` long; in VBR modes `cbr_bytes` is ignored.
    pub fn encode_frame(&mut self, pcm: &[f32], cbr_bytes: usize) -> Vec<u8> {
        let mode = self.mode;
        let cfg = self.cfg;
        let cc = cfg.channels;
        let ci = cc as i32;
        let nb = mode.nb_ebands as usize;
        let overlap = mode.overlap as usize;
        let n = FRAME_SIZE;
        let m = 1i32 << LM;
        let start = 0i32;
        let end = cfg.end;
        let eff_end = end.min(mode.effective_ebands);
        assert_eq!(pcm.len(), n * cc, "a frame is exactly 960 samples per channel");

        let nb_filled_bytes = 0i32;
        let mut nb_compressed_bytes: i32;
        let vbr_rate: i32;
        let effective_bytes: i32;
        if cfg.vbr {
            // bitrate_to_bits(bitrate, 48000, 960) << BITRES, less the Opus TOC byte that the
            // caller prepends (the configured bitrate counts whole packets).
            let bits = ((cfg.bitrate as i64 * FRAME_SIZE as i64) / 48000) as i32 - 8;
            vbr_rate = bits.max(16) << BITRES;
            effective_bytes = vbr_rate >> (3 + BITRES);
            nb_compressed_bytes = MAX_FRAME_BYTES as i32;
        }
        else {
            vbr_rate = 0;
            nb_compressed_bytes = (cbr_bytes as i32).clamp(2, MAX_FRAME_BYTES as i32);
            effective_bytes = nb_compressed_bytes - nb_filled_bytes;
        }
        let mut nb_available_bytes = nb_compressed_bytes - nb_filled_bytes;
        let equiv_rate = {
            let r =
                ((nb_compressed_bytes * 8 * 50) << (3 - LM)) - (40 * ci + 20) * ((400 >> LM) - 50);
            r.min(cfg.bitrate - (40 * ci + 20) * ((400 >> LM) - 50))
        };

        let mut enc = RangeEncoder::new(nb_compressed_bytes as usize);
        if vbr_rate > 0 && cfg.constrained_vbr {
            // The max bit-rate allowed in VBR mode to avoid violating the target rate and
            // buffering. Any multiple of vbr_rate could be used as a bound (depending on the
            // delay); this is clamped to ensure at least two bytes if the encoder was empty.
            let vbr_bound = vbr_rate;
            let max_allowed = 2
                .max((vbr_rate + vbr_bound - self.vbr_reservoir) >> (BITRES + 3))
                .min(nb_available_bytes);
            if max_allowed < nb_available_bytes {
                nb_compressed_bytes = nb_filled_bytes + max_allowed;
                nb_available_bytes = max_allowed;
                enc.shrink(nb_compressed_bytes as usize);
            }
        }
        let mut total_bits = nb_compressed_bytes * 8;

        // Digital silence.
        let silence = {
            let max_abs = |s: &[f32]| s.iter().fold(0.0f32, |a, &v| a.max(v.abs()));
            let split = cc * (n - overlap);
            let mut sample_max = self.overlap_max.max(max_abs(&pcm[..split]));
            self.overlap_max = max_abs(&pcm[split..]);
            sample_max = sample_max.max(self.overlap_max);
            sample_max <= 1.0 / (1u32 << 24) as f32
        };
        enc.enc_bit_logp(silence, 15);
        let mut effective_bytes = effective_bytes;
        if silence {
            // In VBR mode there is no need to send more than the minimum.
            if vbr_rate > 0 {
                nb_compressed_bytes = nb_compressed_bytes.min(nb_filled_bytes + 2);
                effective_bytes = nb_compressed_bytes;
                total_bits = nb_compressed_bytes * 8;
                nb_available_bytes = 2;
                enc.shrink(nb_compressed_bytes as usize);
            }
            // Pretend we've filled all the remaining bits with zeros (that's what the
            // initialiser did anyway).
            enc.force_tell(nb_compressed_bytes * 8);
        }

        // Pre-emphasis. `input` is [overlap carried over | N new samples] per channel.
        let stride = n + overlap;
        let mut input = vec![0f32; cc * stride];
        for ch in 0..cc {
            let base = ch * stride;
            input[base..base + overlap]
                .copy_from_slice(&self.in_mem[ch * overlap..(ch + 1) * overlap]);
            let mut mem = self.preemph_mem[ch];
            for i in 0..n {
                let s = pcm[cc * i + ch];
                let x = if s.is_finite() { s.clamp(-2.0, 2.0) } else { 0.0 } * SIG_SCALE;
                input[base + overlap + i] = x - mem;
                mem = PREEMPH_COEF * x;
            }
            self.preemph_mem[ch] = mem;
            self.in_mem[ch * overlap..(ch + 1) * overlap]
                .copy_from_slice(&input[base + n..base + n + overlap]);
        }

        let mut is_transient = false;
        let mut tf_estimate = 0.0f32;
        let mut tf_chan = 0usize;
        if cfg.complexity >= 1 {
            let t = transient_analysis(&input, n + overlap, cc);
            is_transient = t.is_transient;
            tf_estimate = t.tf_estimate;
            tf_chan = t.tf_chan;
        }

        // Pitch pre-filter: never enabled here; just signal "off".
        if enc.tell() + 16 <= total_bits {
            enc.enc_bit_logp(false, 1);
        }

        let mut short_blocks = false;
        let mut transient_got_disabled = false;
        if enc.tell() + 3 <= total_bits {
            if is_transient {
                short_blocks = true;
            }
        }
        else {
            is_transient = false;
            transient_got_disabled = true;
        }

        let mut freq = vec![0f32; cc * n];
        let mut band_e_buf = [0f32; 42];
        let band_e = &mut band_e_buf[..cc * nb];
        let mut band_log_e_buf = [0f32; 42];
        let band_log_e = &mut band_log_e_buf[..cc * nb];
        let mut band_log_e2_buf = [0f32; 42];
        let band_log_e2 = &mut band_log_e2_buf[..cc * nb];

        let second_mdct = short_blocks && cfg.complexity >= 8;
        if second_mdct {
            self.compute_mdcts(false, &input, &mut freq);
            compute_band_energies(mode, &freq, &mut *band_e, eff_end, ci, LM);
            amp2_log2(mode, eff_end, end, &band_e, &mut *band_log_e2, ci);
            for c in 0..cc {
                for i in 0..end as usize {
                    band_log_e2[nb * c + i] += 0.5 * LM as f32;
                }
            }
        }
        self.compute_mdcts(short_blocks, &input, &mut freq);
        compute_band_energies(mode, &freq, &mut *band_e, eff_end, ci, LM);
        amp2_log2(mode, eff_end, end, &band_e, &mut *band_log_e, ci);

        // Temporal VBR.
        let temporal_vbr = {
            let mut follow = -10.0f32;
            let mut frame_avg = 0.0f32;
            let offset = if short_blocks { 0.5 * LM as f32 } else { 0.0 };
            for i in start as usize..end as usize {
                follow = (follow - 1.0).max(band_log_e[i] - offset);
                if cc == 2 {
                    follow = follow.max(band_log_e[i + nb] - offset);
                }
                frame_avg += follow;
            }
            frame_avg /= (end - start) as f32;
            let tv = (frame_avg - self.spec_avg).clamp(-1.5, 3.0);
            self.spec_avg += 0.02 * tv;
            tv
        };

        if !second_mdct {
            band_log_e2.copy_from_slice(&band_log_e);
        }

        // Last chance to catch any transient missed by the time-domain analysis.
        if enc.tell() + 3 <= total_bits
            && !is_transient
            && cfg.complexity >= 5
            && patch_transient_decision(
                &band_log_e,
                &self.old_band_e,
                nb,
                start as usize,
                end as usize,
                cc,
            )
        {
            is_transient = true;
            short_blocks = true;
            self.compute_mdcts(true, &input, &mut freq);
            compute_band_energies(mode, &freq, &mut *band_e, eff_end, ci, LM);
            amp2_log2(mode, eff_end, end, &band_e, &mut *band_log_e, ci);
            // Compensate for the scaling of short vs long MDCTs.
            for c in 0..cc {
                for i in 0..end as usize {
                    band_log_e2[nb * c + i] += 0.5 * LM as f32;
                }
            }
            tf_estimate = 0.2;
        }

        if enc.tell() + 3 <= total_bits {
            enc.enc_bit_logp(is_transient, 3);
        }

        // Band normalisation.
        let mut x = vec![0f32; cc * n];
        normalise_bands(mode, &freq, &mut x, &band_e, eff_end, ci, m);

        let enable_tf_analysis = effective_bytes >= 15 * ci && cfg.complexity >= 2;

        let mut offsets_buf = [0i32; 21];
        let offsets = &mut offsets_buf[..nb];
        let mut importance_buf = [0i32; 21];
        let importance = &mut importance_buf[..nb];
        let mut spread_weight_buf = [0i32; 21];
        let spread_weight = &mut spread_weight_buf[..nb];
        let dyn_alloc = dynalloc_analysis(
            mode,
            &band_log_e,
            &band_log_e2,
            start as usize,
            end as usize,
            cc,
            &mut *offsets,
            is_transient,
            cfg.vbr,
            cfg.constrained_vbr,
            LM,
            effective_bytes,
            &mut *importance,
            &mut *spread_weight,
        );

        let mut tf_res_buf = [0i32; 21];
        let tf_res = &mut tf_res_buf[..nb];
        let tf_select;
        if enable_tf_analysis {
            let lambda = 80.max(20480 / effective_bytes + 2);
            tf_select = tf_analysis(
                mode,
                eff_end as usize,
                is_transient,
                &mut *tf_res,
                lambda,
                &x,
                n,
                LM,
                tf_estimate,
                tf_chan,
                &importance,
            );
            for i in eff_end as usize..end as usize {
                tf_res[i] = tf_res[eff_end as usize - 1];
            }
        }
        else {
            for r in tf_res[..end as usize].iter_mut() {
                *r = is_transient as i32;
            }
            tf_select = 0;
        }

        let mut error_buf = [0f32; 42];
        let error = &mut error_buf[..cc * nb];
        for c in 0..cc {
            for i in start as usize..end as usize {
                // When the energy is stable, slightly bias energy quantisation towards the
                // previous error to make the gain more stable (a constant offset is better than
                // fluctuations).
                let idx = i + c * nb;
                if (band_log_e[idx] - self.old_band_e[idx]).abs() < 2.0 {
                    band_log_e[idx] -= 0.25 * self.energy_error[idx];
                }
            }
        }
        quant_coarse_energy(
            mode,
            start,
            end,
            eff_end,
            &band_log_e,
            &mut self.old_band_e,
            total_bits as u32,
            &mut *error,
            &mut enc,
            ci,
            LM as usize,
            nb_available_bytes,
            false,
            &mut self.delayed_intra,
            cfg.complexity >= 4,
        );

        tf_encode(
            start as usize,
            end as usize,
            is_transient,
            &mut *tf_res,
            LM,
            tf_select,
            &mut enc,
        );

        if enc.tell() + 4 <= total_bits {
            if short_blocks || cfg.complexity < 3 || nb_available_bytes < 10 * ci {
                self.spread_decision =
                    if cfg.complexity == 0 { SPREAD_NONE } else { SPREAD_NORMAL };
            }
            else {
                self.spread_decision = spreading_decision(
                    mode,
                    &x,
                    &mut self.tonal_average,
                    self.spread_decision,
                    eff_end,
                    ci,
                    m,
                    &spread_weight,
                );
            }
            enc.enc_icdf(self.spread_decision, &SPREAD_ICDF, 5);
        }
        else {
            self.spread_decision = SPREAD_NORMAL;
        }

        let mut cap_buf = [0i32; 21];
        let cap = &mut cap_buf[..nb];
        init_caps(mode, &mut *cap, LM, ci);

        let mut dynalloc_logp = 6i32;
        let total_bits_frac = total_bits << BITRES;
        let mut total_boost = 0i32;
        let mut tell_frac = enc.tell_frac();
        for i in start as usize..end as usize {
            let width = (ci * (mode.e_bands[i + 1] - mode.e_bands[i]) as i32) << LM;
            // quanta is 6 bits, but no more than 1 bit/sample and no less than 1/8 bit/sample.
            let quanta = (width << BITRES).min((6 << BITRES).max(width));
            let mut dynalloc_loop_logp = dynalloc_logp;
            let mut boost = 0i32;
            let mut j = 0;
            while tell_frac + (dynalloc_loop_logp << BITRES) < total_bits_frac - total_boost
                && boost < cap[i]
            {
                let flag = j < offsets[i];
                enc.enc_bit_logp(flag, dynalloc_loop_logp as u32);
                tell_frac = enc.tell_frac();
                if !flag {
                    break;
                }
                boost += quanta;
                total_boost += quanta;
                dynalloc_loop_logp = 1;
                j += 1;
            }
            // Making dynalloc more likely.
            if j > 0 {
                dynalloc_logp = 2.max(dynalloc_logp - 1);
            }
            offsets[i] = boost;
        }

        let mut dual_stereo = false;
        if cc == 2 {
            // Always use MS for 2.5 ms frames until a better analysis exists.
            if LM != 0 {
                dual_stereo = stereo_analysis(mode, &x, LM, n);
            }
            let prev = self.intensity.clamp(0, 21) as usize;
            let i = hysteresis_decision(
                (equiv_rate / 1000) as f32,
                &INTENSITY_THRESHOLDS,
                &INTENSITY_HYSTERESIS,
                prev,
            );
            self.intensity = (i as i32).max(start).min(end);
        }

        let mut alloc_trim = 5;
        if tell_frac + (6 << BITRES) <= total_bits_frac - total_boost {
            alloc_trim = alloc_trim_analysis(
                mode,
                &x,
                &band_log_e,
                end as usize,
                LM,
                cc,
                n,
                &mut self.stereo_saving,
                tf_estimate,
                self.intensity,
                equiv_rate,
            );
            enc.enc_icdf(alloc_trim, &TRIM_ICDF, 7);
            tell_frac = enc.tell_frac();
        }

        // In VBR mode the frame size must not be reduced so much that it would result in the
        // encoder running out of bits. The margin of 2 bytes ensures that none of the
        // bust-prevention logic in the decoder will have triggered so far.
        let min_allowed = ((tell_frac + total_boost + (1 << (BITRES + 3)) - 1) >> (BITRES + 3)) + 2;
        if vbr_rate > 0 {
            // Don't attempt to use more than 510 kb/s, even for frames smaller than 20 ms.
            nb_compressed_bytes = nb_compressed_bytes.min(MAX_FRAME_BYTES as i32 >> (3 - LM));
            let lm_diff = mode.max_lm - LM;
            let mut base_target = vbr_rate - ((40 * ci + 20) << BITRES);
            if cfg.constrained_vbr {
                base_target += self.vbr_offset >> lm_diff;
            }
            let mut target = compute_vbr(
                mode,
                &VbrInputs {
                    base_target,
                    lm: LM,
                    bitrate: equiv_rate,
                    last_coded_bands: self.last_coded_bands,
                    channels: ci,
                    intensity: self.intensity,
                    constrained_vbr: cfg.constrained_vbr,
                    stereo_saving: self.stereo_saving,
                    tot_boost: dyn_alloc.tot_boost,
                    tf_estimate,
                    max_depth: dyn_alloc.max_depth,
                    temporal_vbr,
                },
            );
            // The current offset is removed from the target and the space used so far is added.
            target += tell_frac;

            nb_available_bytes = (target + (1 << (BITRES + 2))) >> (BITRES + 3);
            nb_available_bytes = nb_available_bytes.max(min_allowed).min(nb_compressed_bytes);

            // By how much did we "miss" the target on that frame.
            let mut delta = target - vbr_rate;
            let mut target = nb_available_bytes << (BITRES + 3);

            // If the frame is silent we don't adjust our drift, otherwise the encoder would
            // shoot to very high rates after a span of silence, but we do allow the bit
            // reservoir to refill.
            if silence {
                nb_available_bytes = 2;
                target = (2 * 8) << BITRES;
                delta = 0;
            }

            let alpha = if self.vbr_count < 970 {
                self.vbr_count += 1;
                1.0 / (self.vbr_count + 20) as f32
            }
            else {
                0.001
            };
            if cfg.constrained_vbr {
                // How many bits we have used in excess of what we're allowed.
                self.vbr_reservoir += target - vbr_rate;
                // The offset we need to apply in order to reach the target.
                self.vbr_drift += (alpha
                    * ((delta * (1 << lm_diff)) - self.vbr_offset - self.vbr_drift) as f32)
                    as i32;
                self.vbr_offset = -self.vbr_drift;
                if self.vbr_reservoir < 0 {
                    // We're under the min value: increase the rate, unless just coding silence.
                    let adjust = (-self.vbr_reservoir) / (8 << BITRES);
                    nb_available_bytes += if silence { 0 } else { adjust };
                    self.vbr_reservoir = 0;
                }
            }
            nb_compressed_bytes = nb_compressed_bytes.min(nb_available_bytes);
            // This moves the raw bits to take into account the new compressed size.
            enc.shrink(nb_compressed_bytes as usize);
        }

        // Bit allocation.
        let mut fine_quant_buf = [0i32; 21];
        let fine_quant = &mut fine_quant_buf[..nb];
        let mut pulses_buf = [0i32; 21];
        let pulses = &mut pulses_buf[..nb];
        let mut fine_priority_buf = [0i32; 21];
        let fine_priority = &mut fine_priority_buf[..nb];

        // bits = packet size - where we are - safety.
        let mut bits = ((nb_compressed_bytes * 8) << BITRES) - enc.tell_frac() - 1;
        let anti_collapse_rsv =
            if is_transient && LM >= 2 && bits >= ((LM + 2) << BITRES) { 1 << BITRES } else { 0 };
        bits -= anti_collapse_rsv;
        let signal_bandwidth = end - 1;
        let alloc = {
            let mut coder = EncAlloc {
                enc: &mut enc,
                prev: self.last_coded_bands,
                signal_bandwidth,
                intensity: self.intensity,
                dual_stereo,
            };
            clt_compute_allocation(
                mode,
                start,
                end,
                &offsets,
                &cap,
                alloc_trim,
                bits,
                LM,
                ci,
                &mut coder,
                &mut *pulses,
                &mut *fine_quant,
                &mut *fine_priority,
            )
        };
        self.intensity = alloc.intensity;
        let dual_stereo = alloc.dual_stereo;
        let coded_bands = alloc.coded_bands;
        self.last_coded_bands = if self.last_coded_bands != 0 {
            (self.last_coded_bands + 1).min((self.last_coded_bands - 1).max(coded_bands))
        }
        else {
            coded_bands
        };

        quant_fine_energy(
            mode,
            start,
            end,
            &mut self.old_band_e,
            &mut *error,
            &fine_quant,
            &mut enc,
            ci,
        );
        self.energy_error.fill(0.0);

        // Residual quantisation.
        {
            let (x0, x1) = x.split_at_mut(n);
            quant_all_bands(
                mode,
                &QuantParams {
                    start,
                    end,
                    pulses: &pulses,
                    short_blocks,
                    spread: self.spread_decision,
                    dual_stereo,
                    intensity: self.intensity,
                    tf_res: &tf_res,
                    total_bits: nb_compressed_bytes * (8 << BITRES) - anti_collapse_rsv,
                    balance: alloc.balance,
                    lm: LM,
                    coded_bands,
                },
                x0,
                if cc == 2 { Some(x1) } else { None },
                &band_e,
                &mut enc,
            );
        }

        if anti_collapse_rsv > 0 {
            let anti_collapse_on = self.consec_transient < 2;
            enc.enc_bits(anti_collapse_on as u32, 1);
        }
        quant_energy_finalise(
            mode,
            start,
            end,
            &mut self.old_band_e,
            &mut *error,
            &fine_quant,
            &fine_priority,
            nb_compressed_bytes * 8 - enc.tell(),
            &mut enc,
            ci,
        );
        for c in 0..cc {
            for i in start as usize..end as usize {
                self.energy_error[i + c * nb] = error[i + c * nb].clamp(-0.5, 0.5);
            }
        }
        if silence {
            self.old_band_e.fill(-28.0);
        }

        if !is_transient {
            self.old_log_e2.copy_from_slice(&self.old_log_e);
            self.old_log_e.copy_from_slice(&self.old_band_e);
        }
        else {
            for (l, &e) in self.old_log_e.iter_mut().zip(self.old_band_e.iter()) {
                *l = l.min(e);
            }
        }
        // In case start or end were to change.
        for c in 0..cc {
            for i in (0..start as usize).chain(end as usize..nb) {
                self.old_band_e[c * nb + i] = 0.0;
                self.old_log_e[c * nb + i] = -28.0;
                self.old_log_e2[c * nb + i] = -28.0;
            }
        }
        if is_transient || transient_got_disabled {
            self.consec_transient += 1;
        }
        else {
            self.consec_transient = 0;
        }

        self.final_range = enc.rng();
        let (buf, error) = enc.done();
        assert!(!error, "CELT range coder overflow (encoder bug)");
        debug_assert_eq!(buf.len(), nb_compressed_bytes as usize);
        buf
    }
}
